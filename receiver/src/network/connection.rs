use std::sync::mpsc::{SyncSender, sync_channel};
use common::fec::assembler::FrameAssembler;


const IDR_INTERVAL_MS: u64 = 50; 
const FAIL_WINDOW: usize = 26;
use crate::VideoWorkerMsg;

use super::*;

pub(crate) struct VideoState {
    pub waiting_for_key: bool,
    pub expected_frame_id: Option<u64>,
    pub expected_slice_idx: u8,
    pub last_idr: Instant,
    pub recovery: RecoveryState,
    pub loss_window: [bool; FAIL_WINDOW],
    pub loss_window_pos: usize,
    pub got_zero_slice: bool,
}

impl VideoState {
    fn record_loss(&mut self) {
        self.loss_window[self.loss_window_pos] = true;
        // Было % 16, стало % FAIL_WINDOW
        self.loss_window_pos = (self.loss_window_pos + 1) % FAIL_WINDOW;
    }

    fn record_success(&mut self) {
        self.loss_window[self.loss_window_pos] = false;
        // Было % 16, стало % FAIL_WINDOW
        self.loss_window_pos = (self.loss_window_pos + 1) % FAIL_WINDOW;
    }

    fn loss_rate(&self) -> u32 {
        self.loss_window.iter().filter(|&&x| x).count() as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecoveryState {
    /// Обычный режим — concealment включён
    Concealing,
    /// Запросили IDR, дропаем все кадры до него
    WaitingForIdr,
    /// IDR получен, ждём N хороших кадров перед тем как снова concealment
    StabilizingAfterIdr { good_frames_left: u32 },
}

pub(crate) fn spawn_video_backend_worker<B: VideoBackend>(
    mut backend: B,
    control_tx: mpsc::Sender<ControlPacket>,
    idr_needed_tx: watch::Sender<bool>,
    frame_tx: Option<mpsc::Sender<DecodedFrame>>,
    proxy: AppProxy,
) -> SyncSender<VideoWorkerMsg>
where
    B: VideoBackend + 'static,
{
    let (tx, rx) = sync_channel::<VideoWorkerMsg>(1024);

    #[cfg(target_os = "android")]
    if let Ok(mut guard) = crate::backend::android::WORKER_TX.lock() {
        *guard = Some(tx.clone());
    }

    #[cfg(target_os = "android")]
    {
        log::info!("[Worker] Checking PENDING_SURFACE on spawn");
        if let Ok(mut pending) = crate::backend::android::PENDING_SURFACE.lock() {
            log::info!("[Worker] PENDING_SURFACE is_some: {}", pending.is_some());
            if let Some(msg) = pending.take() {
                let _ = tx.send(msg);
                log::info!("[Worker] Sent pending InitSurface to worker");
            }
        }
    }
    std::thread::spawn(move || {
        let mut state = VideoState {
            waiting_for_key: true,
            loss_window: [false; FAIL_WINDOW],
            loss_window_pos: 0,
            expected_frame_id: None,
            expected_slice_idx: 0,
            last_idr: Instant::now(),
            recovery: RecoveryState::Concealing,
            got_zero_slice: false,
        };

        while let Ok(msg) = rx.recv() {
            match msg {
                VideoWorkerMsg::Push(packet) => {
                    push_frame_to_backend(
                        packet,
                        &mut state,
                        &mut backend,
                        &control_tx,
                        &idr_needed_tx,
                    );
                },
                VideoWorkerMsg::PollDecoder => {
                    const MAX_DRAIN_PER_TICK: usize = 32;

                    let mut drained = 0usize;

                    loop {
                        if drained >= MAX_DRAIN_PER_TICK {
                            break;
                        }

                        match backend.poll_output() {
                            Ok(FrameOutput::Pending) => {
                                break},

                            Ok(FrameOutput::Dropped { age }) => {
                                let _ = control_tx.try_send(ControlPacket::Communication {
                                    message: format!(
                                        "DROPPED ON POLL, age: {:.1}ms",
                                        age.unwrap_or(0.0)
                                    ),
                                });
                            }

                            Ok(FrameOutput::Yuv(f)) => {
                                if let Some(tx) = &frame_tx {
                                    if tx.try_send(DecodedFrame::Yuv(f)).is_err() {
                                        break;
                                    }
                                }

                                #[cfg(not(target_os = "android"))]
                                if let Some(p) = &proxy {
                                    let _ = p.send_event(crate::UserEvent::NewFrame);
                                }

                                drained += 1;
                            }

                            #[cfg(unix)]
                            Ok(FrameOutput::DmaBuf(f)) => {
                                if let Some(tx) = &frame_tx {
                                    if tx.try_send(DecodedFrame::DmaBuf(f)).is_err() {
                                        break;
                                    }
                                }

                                #[cfg(not(target_os = "android"))]
                                if let Some(p) = &proxy {
                                    let _ = p.send_event(crate::UserEvent::NewFrame);
                                }

                                drained += 1;
                            }

                            Ok(_) => {}

                            Err(e) => {
                                let _ = control_tx.try_send(ControlPacket::Communication {
                                    message: format!("poll_output error: {e:?}"),
                                });
                                break;
                            }
                        }
                    }
                },
                VideoWorkerMsg::Shutdown => {
                    backend.shutdown();
                    break;
                }

                #[cfg(target_os = "android")]
                VideoWorkerMsg::InitSurface { window, width, height } => {
                    log::info!("[Worker] Got InitSurface {}x{}", width, height);
                    unsafe {
                        match backend.init_with_surface(window, width, height) {
                            Ok(()) => log::info!("[Worker] init_with_surface OK"),
                            Err(e) => log::error!("[Worker] init_with_surface FAILED: {:?}", e),
                        }
                    }
                }
            }
        }
    });

    tx
}

pub(crate) fn request_idr(
    state:      &mut VideoState,
    idr_needed_tx: &watch::Sender<bool>,
    packet_frame_id: u64, 
) {
    let elapsed = state.last_idr.elapsed();
    if elapsed >= Duration::from_millis(IDR_INTERVAL_MS) {
        state.last_idr = Instant::now();
        let _ = idr_needed_tx.send(true);
        
        log::info!(
            "IDR Requested. Frame mismatch at #{} (Expected {:?}). Last request was {:?} ago", 
            packet_frame_id, 
            state.expected_frame_id,
            elapsed
        );
    } else {
        log::trace!("IDR request suppressed: cooldown active ({:?} left)", 
            Duration::from_millis(IDR_INTERVAL_MS).saturating_sub(elapsed));
    }
}

pub(crate) async fn connect_quic(
    endpoint: &quinn::Endpoint,
    sender_addr: SocketAddr,
) -> Result<quinn::Connection, Box<dyn Error>> {
    let connecting = endpoint.connect(sender_addr, "localhost")?;
    let conn = connecting.await?;
    Ok(conn)
}

pub(crate) async fn receive_datagrams(
    conn:     quinn::Connection,
    worker_tx: SyncSender<VideoWorkerMsg>,
    control_tx: mpsc::Sender<ControlPacket>,
) {
    let mut jitter_buf = JitterBuffer::new(JITTER_TARGET_MS);
    let mut assembler = FrameAssembler::new();
    
    let sleep_until = Instant::now() + Duration::from_secs(3600);
    let sleep = time::sleep_until(sleep_until);
    
    tokio::pin!(sleep);
    loop {
        // ── Sleep duration until the next buffered frame is due ───────────────
        // If the buffer is empty we park the timer for 1 hour; it will be
        // cancelled the moment the datagram arm fires.
        let next_deadline = match jitter_buf.time_to_next() {
            Some(d) => Instant::now() + d,
            None    => Instant::now() + Duration::from_secs(3600),
        };
        sleep.as_mut().reset(next_deadline);

        
        tokio::select! {
            // ── 1. Incoming datagram ─────────────────────────────────────────
            // ── 2. Jitter-buffer drain timer ─────────────────────────────────
            // Fires when the earliest buffered frame has waited long enough.
            // All frames whose deadline has now passed are released at once.
            _ = &mut sleep => {
                let ready = jitter_buf.drain_ready();
                if ready.is_empty() { continue; }

                for packet in ready {
                    if let Err(e) = worker_tx.send(VideoWorkerMsg::Push(packet)) {
                        log::error!("Video worker channel closed: {e}");
                        return;
                    }
                }
            }

            raw = conn.read_datagram() => {
                let raw = match raw {
                    Ok(b)  => b,
                    Err(e) => { log::error!("QUIC READ ERROR: {:?}", e); break; }
                };
 
                log::trace!("Got packet: {} bytes", raw.len());
 
                let chunk = match DatagramChunk::decode(raw) {
                    Some(c) => c,
                    None    => { log::warn!("Failed to decode chunk header"); continue; }
                };
 
                match chunk.packet_type {
                    TYPE_VIDEO => { // WE RECEIVE VIDEOSLICE TYPE HERE
                        // Reassemble; if a frame completed, enqueue it in the
                        // jitter buffer instead of pushing to the backend directly.
                        let (assembled, nack) = assembler.insert(&chunk);
                        if let Some(nack_pkt) = nack {
                            if let Err(e) = control_tx.try_send(nack_pkt) {
                                log::debug!("[NACK] control_tx full, NACK dropped: {e}");
                            }
                        }
                        for packet in assembled {
                            // Если твой джиттер-буфер принимает по одному пакету:
                            jitter_buf.push(packet);
                        }
                    }
                    TYPE_AUDIO => {
                        // Audio bypasses the jitter buffer (separate sync path).
                        // handle_audio_frame(chunk.data);
                    }
                    TYPE_CONTROL => {
                        if let Ok(ctrl) = postcard::from_bytes::<ControlPacket>(&chunk.data) {
                            if let Some((_, rtt_us)) = process_control_feedback(ctrl) {
                                let conn_clone = conn.clone();
                                tokio::spawn(async move {
                                    let _ = send_offset_update(&conn_clone, rtt_us).await;
                                });
                            }
                        }
                    }
                    _ => log::warn!("Unknown packet type"),
                }
            }
        }
        
    }
}

pub(crate) fn push_frame_to_backend<B: VideoBackend>(
    mut packet: VideoPacket,
    state: &mut VideoState,
    backend: &mut B,
    control_tx: &mpsc::Sender<ControlPacket>,
    idr_needed_tx: &watch::Sender<bool>,
) {
    // ── IDR gate ─────────────────────────────────────────────────────────
    if packet.is_key && packet.slice_idx == 0 {
        // Всегда синхронизируемся на IDR, независимо от waiting_for_key
        state.expected_frame_id = Some(packet.frame_id);
        state.expected_slice_idx = 0;
        
        if state.waiting_for_key {
            state.loss_window.fill(false);
            state.loss_window_pos = 0;
            state.waiting_for_key = false;
            let _ = idr_needed_tx.send(false);
            log::info!("[Video] IDR received, stabilizing (5 frames)");
            state.recovery = RecoveryState::StabilizingAfterIdr { good_frames_left: 5 };
        }
    }

    if state.waiting_for_key {
        log::trace!("[Video] Dropped frame #{} — waiting for IDR", packet.frame_id);
        return;
    }

    // ── Frame gap detection ───────────────────────────────────────────────
    if Some(packet.frame_id) != state.expected_frame_id {
        if Some(packet.frame_id) < state.expected_frame_id {
            return; // старый кадр
        }
        state.record_loss();
        let _ = control_tx.try_send(ControlPacket::LostFrame);

        match state.recovery {
            RecoveryState::StabilizingAfterIdr { .. } => {
                // Потеря во время стабилизации — сразу снова IDR
                log::info!("[Video] Loss during stabilization, re-requesting IDR");
                state.waiting_for_key = true;
                state.recovery = RecoveryState::WaitingForIdr;
                request_idr(state, idr_needed_tx, packet.frame_id);
                return;
            }
            RecoveryState::Concealing => {
                backend.clear_buffer();
                if state.loss_rate() > 4 {
                    log::info!("[Video] Too many losses ({}), switching to IDR", state.loss_rate());

                    state.waiting_for_key = true;
                    state.recovery = RecoveryState::WaitingForIdr;
                    request_idr(state, idr_needed_tx, packet.frame_id);
                    return
                }
            }
            RecoveryState::WaitingForIdr => {
                return; // уже ждём IDR
            }
        }
        state.expected_frame_id = Some(packet.frame_id);
        state.expected_slice_idx = 0;
    }

    if packet.slice_idx == 0 {
        state.got_zero_slice = true;
    }

    if !state.got_zero_slice {
        log::trace!("[Video] Dropping slice {} for frame #{} - no start slice yet", 
            packet.slice_idx, packet.frame_id);
        return; // Пока не увидим 0-й слайс, остальное игнорируем
    }
    // ── Slice order check ─────────────────────────────────────────────────
    if packet.slice_idx < state.expected_slice_idx {
        log::trace!("[Video] Old slice for frame #{} dropped", packet.frame_id);
        return;
    }

    if packet.slice_idx > state.expected_slice_idx {
        log::debug!("[Video] Slice loss detected in frame #{} (gap {}->{})", 
            packet.frame_id, state.expected_slice_idx, packet.slice_idx);
        // Мы НЕ выходим из функции, а позволяем слайсу уйти в бэкенд
    }

    // Обновляем ожидание: следующий слайс должен быть сразу за текущим
    state.expected_slice_idx = packet.slice_idx + 1;

    if packet.is_last {
        state.record_success();
        state.expected_frame_id = Some(packet.frame_id + 1);
        state.expected_slice_idx = 0;

        // Считаем хорошие кадры в режиме стабилизации
        if let RecoveryState::StabilizingAfterIdr { good_frames_left } = &mut state.recovery {
            *good_frames_left = good_frames_left.saturating_sub(1);
            if *good_frames_left == 0 {
                log::info!("[Video] Stream stable, concealment re-enabled");
                state.recovery = RecoveryState::Concealing;
            }
        }
    }

    // ── Push to decoder ───────────────────────────────────────────────────
    let frame_id = packet.frame_id;
    let payload  = packet.payload;
    let trace    = packet.trace.take();
    let ctrl     = control_tx.clone();

    match backend.push_encoded(&payload, frame_id, trace, packet.is_last) {
        Ok(PushStatus::Dropped { age }) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!("DROPPED ON PUSH#{}, age: {:.1}ms", frame_id, age.unwrap_or(0.0)),
            });
        }
        Ok(PushStatus::Accepted { fps }) => {
            #[cfg(target_os = "android")]
            if fps > 0 {
                let _ = ctrl.try_send(ControlPacket::Communication {
                    message: format!("My fps is {fps}"),
                });
            }
        }
        Ok(PushStatus::Accumulating) => {}
        Err(e) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!("ERROR ON BACKEND: {e}"),
            });
        }
    }
}