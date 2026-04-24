use std::sync::mpsc::{SyncSender, sync_channel};
use common::{AudioFrame, fec::assembler::FrameAssembler};


const IDR_INTERVAL_MS: u64 = 50; 
const FAIL_WINDOW: usize = 26;

#[cfg(not(target_os = "android"))]
use crate::backend::audio_cpal::CpalAudioOutput;

use crate::{AvJitterBuffer, VideoWorkerMsg, backend::audio_output::{self, AudioOutput}};

#[cfg(target_os = "android")]
use crate::backend::audio_oboe::OboeAudioOutput;

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


fn drain_logical<B: VideoBackend>(
    backend: &mut B, 
    frame_tx: &Option<mpsc::Sender<DecodedFrame>>, 
    proxy: &AppProxy,
    control_tx: &mpsc::Sender<ControlPacket>
) {
    let mut drained = 0;
    while drained < 32 {
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
                    drain_logical(&mut backend, &frame_tx, &proxy, &control_tx);
                },
                VideoWorkerMsg::PollDecoder => {},
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
    let (audio_tx, mut audio_rx) = mpsc::channel::<AudioFrame>(256);
    let (video_tx, mut video_rx) = mpsc::channel::<VideoPacket>(512);
    let (audio_out_tx, mut audio_out_rx) = mpsc::channel::<AudioFrame>(256);

    let mut assembler = FrameAssembler::new();
    
    tokio::spawn(async move {
        let mut jitter = AvJitterBuffer::new(JITTER_TARGET_MS);
        let mut next_drain_us: u64 = 0;

        // Вспомогательная функция для выгрузки готовых кадров
        let drain_jitter = |jitter: &mut AvJitterBuffer, now_us: u64| -> u64 {
            let (video, audio) = jitter.drain_ready_with(now_us);
            
            for packet in video {
                let _ = worker_tx.send(VideoWorkerMsg::Push(packet));
            }

            for frame in audio {
                let _ = audio_out_tx.try_send(frame);
            }

            // Возвращаем время следующей выгрузки или дефолтный таймаут
            jitter.time_to_next_us().unwrap_or(now_us + 50_000)
        };

        
        loop {
            let now_us = FrameTrace::now_us();
            let sleep_us = next_drain_us.saturating_sub(now_us);
            let sleep_dur = Duration::from_micros(sleep_us);

            tokio::select! {
                // Пришло видео: пушим и СРАЗУ пытаемся выгрузить
                Some(mut packet) = video_rx.recv() => {
                    let now = FrameTrace::now_us();
                    if let Some(ref mut trace) = packet.trace {
                        trace.jitter_out_us = now;
                    }
                    jitter.push_video(packet, now);
                    next_drain_us = drain_jitter(&mut jitter, now);
                }

                // Пришло аудио: пушим и СРАЗУ пытаемся выгрузить
                Some(frame) = audio_rx.recv() => {
                    let now = FrameTrace::now_us();
                    jitter.push_audio(frame, now);
                    next_drain_us = drain_jitter(&mut jitter, now);
                }

                // Проснулись по таймеру: выгружаем накопленное
                _ = tokio::time::sleep(sleep_dur), if !sleep_dur.is_zero() => {
                    next_drain_us = drain_jitter(&mut jitter, FrameTrace::now_us());
                }
            }

            // Если джиттер говорит, что пора отдавать прямо сейчас (целевая задержка 0),
            // делаем это не дожидаясь следующего select!
            let now = FrameTrace::now_us();
            if next_drain_us <= now {
                next_drain_us = drain_jitter(&mut jitter, now);
            }
        }
    });

    tokio::spawn(async move {
        #[cfg(not(target_os = "android"))]
        let mut audio_output = CpalAudioOutput::new();
        #[cfg(target_os = "android")]
        let mut audio_output = OboeAudioOutput::new();

        while let Some(frame) = audio_out_rx.recv().await {
            audio_output.push_opus(&frame.payload);
        }
    });

    // 3. Основной сетевой цикл (Hot Path)
    // Теперь он занимается ТОЛЬКО приемом и сборкой, не отвлекаясь на таймеры
    
    let mut poll_interval = tokio::time::interval(Duration::from_millis(1));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = conn.read_datagram() => {
                let raw = match result {
                    Ok(b)  => b,
                    Err(e) => { log::error!("QUIC READ ERROR: {:?}", e); break; }
                };

                let chunk = match DatagramChunk::decode(raw) {
                    Some(c) => c,
                    None    => { continue; }
                };

                match chunk.packet_type {
                    TYPE_VIDEO => {
                        let receive_us = FrameTrace::now_us();

                        let (assembled, nacks, stats) = assembler.insert(&chunk, receive_us);
                        for nack in nacks {
                            let _ = control_tx.try_send(nack);
                        }
                        for packet in assembled {
                            let _ = video_tx.try_send(packet);
                        }
                        if let Some(stat) = stats {
                            let _ = control_tx.try_send(stat);
                        }
                    }
                    TYPE_CONTROL => {
                        if let Ok(ctrl) = postcard::from_bytes::<ControlPacket>(&chunk.data) {
                            process_control(ctrl);
                        }
                    }
                    TYPE_AUDIO => {
                        if !chunk.data.is_empty() {
                            log::trace!("[Audio] received chunk, {} bytes", chunk.data.len());
                            match postcard::from_bytes::<AudioFrame>(&chunk.data) {
                                Ok(frame) => {
                                    log::trace!("[Audio] deserialized frame capture_us={}", frame.capture_us);
                                    match audio_tx.try_send(frame) {
                                        Ok(_) => log::debug!("[Audio] sent to jitter"),
                                        Err(e) => log::warn!("[Audio] channel send failed: {e}"),
                                    }
                                }
                                Err(e) => log::warn!("[Audio] deserialize failed: {e}"),
                            }
                        } else {
                            log::warn!("[Audio] empty chunk");
                        }
                    }
                    _ => {}
                }
            }

            _ = poll_interval.tick() => {
                let (packets, nacks) = assembler.poll();
                for nack in nacks {
                    let _ = control_tx.try_send(nack);
                }
                for packet in packets {
                    let _ = video_tx.try_send(packet);
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
        log::debug!("[Video] Dropped frame #{} — waiting for IDR", packet.frame_id);
        return;
    }

    // ── Frame gap detection ───────────────────────────────────────────────
    if Some(packet.frame_id) != state.expected_frame_id {
        if Some(packet.frame_id) < state.expected_frame_id {
            return; // старый кадр
        }
        log::debug!("[Video] A frame gap detected for frame #{}, expected #{}", packet.frame_id, state.expected_frame_id.unwrap());
        state.record_loss();
        let _ = control_tx.try_send(ControlPacket::LostFrame);
        state.got_zero_slice = false;
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
                log::debug!("[Video] Concealing Frame");
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
        log::debug!("[Video] Dropping slice {} for frame #{} - no start slice yet", 
            packet.slice_idx, packet.frame_id);
        return; // Пока не увидим 0-й слайс, остальное игнорируем
    }
    // ── Slice order check ─────────────────────────────────────────────────
    if packet.slice_idx < state.expected_slice_idx {
        log::debug!("[Video] Old slice for frame #{} dropped", packet.frame_id);
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

    match backend.push_encoded(&payload, frame_id, trace, packet.is_last, packet.slice_idx, packet.is_concealed) {
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