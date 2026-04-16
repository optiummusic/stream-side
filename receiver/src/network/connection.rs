use common::fec::assembler::FrameAssembler;

use super::*;

pub(crate) struct VideoState {
    pub waiting_for_key: bool,
    pub expected_frame_id: Option<u64>,
    pub expected_slice_idx: u8,
}

pub(crate) async fn connect_quic(
    endpoint: &quinn::Endpoint,
    sender_addr: SocketAddr,
) -> Result<quinn::Connection, Box<dyn Error>> {
    let connecting = endpoint.connect(sender_addr, "localhost")?;
    let conn = connecting.await?;
    Ok(conn)
}

pub(crate) async fn receive_datagrams<B: VideoBackend>(
    conn:     quinn::Connection,
    backend:  Arc<Mutex<B>>,
    control_tx: mpsc::Sender<ControlPacket>,
    idr_needed_tx: watch::Sender<bool>,
) {
    let mut video_state = VideoState {
        waiting_for_key: true,
        expected_frame_id: None,
        expected_slice_idx: 0
    };
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
 
                log::trace!("[JitterBuf] releasing {} frame(s)", ready.len());
 
                for packet in ready {
                    push_frame_to_backend(
                        packet,
                        &mut video_state,
                        &backend,
                        &control_tx,
                        &idr_needed_tx,
                    ).await;
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
                        if let Some(packet) = assembled {
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

pub(crate) async fn push_frame_to_backend<B: VideoBackend + Send + 'static>(
    mut packet: VideoPacket,
    state:      &mut VideoState,
    backend:    &Arc<Mutex<B>>,
    control_tx: &mpsc::Sender<ControlPacket>,
    idr_needed_tx: &watch::Sender<bool>, // To set to false on I-frame
) {
    // ── In-order / IDR gate ───────────────────────────────────────────────
    if packet.is_key {
        state.waiting_for_key = false;
        state.expected_frame_id = Some(packet.frame_id);
        state.expected_slice_idx = 0;
        let _ = idr_needed_tx.send(false);
    } 

    // Если мы уже ждем ключ — просто игнорируем любые дельта-кадры
    if state.waiting_for_key {
        return;
    }
    if Some(packet.frame_id) != state.expected_frame_id {
        state.waiting_for_key = true;
        let _ = idr_needed_tx.send(true);
        return;
    }
    if packet.slice_idx != state.expected_slice_idx {
        // пропустили slice → десинк
        state.waiting_for_key = true;
        let _ = idr_needed_tx.send(true);
        return;
    }
    state.expected_slice_idx += 1;
    
    if packet.is_last {
        state.expected_frame_id = Some(packet.frame_id + 1);
        state.expected_slice_idx = 0;
    }
 
    let frame_id    = packet.frame_id;
    let payload     = packet.payload;
    let trace       = packet.trace.take();
    let backend_arc = backend.clone();
    let ctrl        = control_tx.clone();
 
    let push_result = tokio::task::spawn_blocking(move || {
        let mut backend_lock = backend_arc.lock().unwrap();
        backend_lock.push_encoded(&payload, frame_id, trace)
    })
    .await;
 
    match push_result {
        Ok(Ok(PushStatus::Dropped { age })) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!(
                    "DROPPED ON PUSH#{}, age: {:.1}ms",
                    frame_id,
                    age.unwrap_or(0.0)
                ),
            });
        }
        Ok(Ok(PushStatus::Accepted)) => {}
        Ok(Err(e)) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!("ERROR ON BACKEND: {e}"),
            });
        }
        Err(e) => log::error!("[Video] push task join error: {e}"),
    }
}