use std::time::Instant;

use super::*;
static mut LAST_UPDATE: Option<Instant> = None;

pub(crate) async fn handle_bi_stream(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    identity_tx: watch::Sender<ClientIdentity>,
    info: Arc<ConnectionInfo>
) -> Result<(), Box<dyn std::error::Error>> {

    // читаем длину
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;

    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;

    let packet: ControlPacket = postcard::from_bytes(&data)?;

    match packet {
        ControlPacket::Identify { model, os } => {
            log::info!("[QUIC] IDENTIFY via stream: {model} [{os}]");

            let mut id = identity_tx.borrow().clone();
            id.model = Some(model.clone()); 
            id.os = Some(os.clone());
            id.ready = true;
            let _ = identity_tx.send(id);

            let label = format!("{} [{} {}]", info.remote, model, os);

            {
                let mut l = info.label.write().await;
                *l = label;
            }

            info.ready.store(true, Ordering::Release);
            let reply = ControlPacket::StartStreaming;
            let bytes = postcard::to_stdvec(&reply)?;

            send.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
            send.write_all(&bytes).await?;
            send.finish()?;
        }
        _ => {}
    }

    Ok(())
}

pub(crate) async fn handle_uni_stream(
    conn: Connection,
    mut recv: quinn::RecvStream,
    info: Arc<ConnectionInfo>,
    clock_offset: Arc<AtomicI64>,
    idr_tx: watch::Sender<bool>,
    shard_cache: Arc<ShardCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = recv.read_exact(&mut len_buf).await {
            log::info!("Stream closed: {:?}", e);
            break; 
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        let mut data = vec![0u8; len];
        recv.read_exact(&mut data).await?;
        let packet: ControlPacket = match postcard::from_bytes(&data) {
            Ok(p) => p,
            Err(e) => {
                let label = info.label().await;
                log::error!("[QUIC UNI] ERROR FOR {label}: {:?}", e);
                continue; // Выходим из текущей итерации цикла
            }
        };
        
        match packet {
            ControlPacket::FrameFeedback { frame_id, trace } => {
                let t = trace;
                let label = info.label().await;

                let receive_srv     = client_to_server_us(t.receive_us, &clock_offset);
                let reassembled_srv = client_to_server_us(t.reassembled_us, &clock_offset);
                let decode_srv      = client_to_server_us(t.decode_us, &clock_offset);
                let present_srv     = client_to_server_us(t.present_us, &clock_offset);
                let off = clock_offset.load(Ordering::Relaxed);

                log::info!(
                    "[QUIC] {label} got frame trace. Offset is: {off}:
                    #{frame_id}: capture→encode={:.1}ms encode→serial={:.1}ms \
                    serial→recv={:.1}ms recv→reassem={:.1}ms reassem→decode={:.1}ms \
                    decode→present={:.1}ms  TOTAL={:.1}ms",
                    FrameTrace::ms(t.capture_us, t.encode_us),
                    FrameTrace::ms(t.encode_us, t.serialize_us),
                    FrameTrace::ms(t.serialize_us, receive_srv),
                    FrameTrace::ms(receive_srv, reassembled_srv),
                    FrameTrace::ms(reassembled_srv, decode_srv),
                    FrameTrace::ms(decode_srv, present_srv),
                    FrameTrace::ms(t.capture_us, present_srv),
                );
            }
            ControlPacket::Communication { message } => {
                let label = info.label().await;
                log::info!("[QUIC] {label} send a MESSAGE: {message}");
            }

            ControlPacket::RequestKeyFrame => {
                let label = info.label().await;
                log::info!("[QUIC] Client {label} requested KeyFrame!");
                let _ = idr_tx.send(true);
                let _ = idr_tx.send(false);
            }
            ControlPacket::Nack {
                frame_id,
                slice_idx,
                group_idx,
                received_mask,
            } => {
                let label = info.label().await;
                log::debug!(
                    "[NACK] {label} requested retransmit \
                    frame={frame_id} slice={slice_idx} group={group_idx} \
                    received={received_mask:#066b}"
                );
            
                let retransmit = shard_cache.retransmit(
                    frame_id,
                    slice_idx,
                    group_idx,   // ← pass group through to cache lookup
                    received_mask,
                );
                let count = retransmit.len();
            
                for dgram in retransmit {
                    if let Err(e) = conn.send_datagram(dgram) {
                        log::warn!("[NACK] retransmit send failed for {label}: {e}");
                        break;
                    }
                }
            
                if count > 0 {
                    log::debug!(
                        "[NACK] retransmitted {count} shard(s) for \
                        frame={frame_id} slice={slice_idx} group={group_idx} to {label}"
                    );
                } else {
                    // Cache miss — frame evicted or IDR was already triggered.
                    log::warn!(
                        "[NACK] frame={frame_id} slice={slice_idx} group={group_idx} \
                        not in cache for {label}."
                    );
                }
            }
        _ => {}
        }
    }
    Ok(())
}

pub(crate) fn client_to_server_us(t: u64, clock_offset: &AtomicI64) -> u64 {
    let off = clock_offset.load(Ordering::Relaxed);
    if off >= 0 {
        t.saturating_add(off as u64)
    } else {
        t.saturating_sub((-off) as u64)
    }
}

pub(crate) async fn handle_control(
    conn: &Connection, 
    data: Bytes, 
    info: &ConnectionInfo, 
    clock_offset: &AtomicI64, 
    idr_tx: watch::Sender<bool>, 
    bitrate_tx: watch::Sender<u64>,
) {
    if let Ok(packet) = postcard::from_bytes::<ControlPacket>(&data) {
        match packet {
            ControlPacket::Ping { client_time_us } => {
                let pong = ControlPacket::Pong {
                    client_time_us,
                    server_time_us: FrameTrace::now_us(),
                };
                if let Ok(bin) = postcard::to_stdvec(&pong) {
                    // Отправляем ответ как TYPE_CONTROL
                    let dgram = DatagramChunk::encode(
                        0,                  // frame_id (для контроля можно 0)
                        0,                  // slice_idx
                        1,                  // total_slices
                        0,                  // shard_idx
                        1,                  // k
                        0,                  // m
                        bin.len() as u16,   // payload_len
                        TYPE_CONTROL,       // packet_type
                        0, 
                        0,
                        0,                 // flags
                        &bin                // data
                    );
                    let _ = conn.send_datagram(dgram);
                }
            },
            ControlPacket::Pong{..} => (),
            ControlPacket::OffsetUpdate { offset_us, rtt_us } => {
                clock_offset.store(offset_us, Ordering::Relaxed);
                let rtt_ms = rtt_us as f64 / 1000.0;
                // Константы для настройки
                const MIN_BITRATE: u64 = 100_000;  // 2 Мбит
                const MAX_BITRATE: u64 = 50_000_000; // 30 Мбит
                const RTT_THRESHOLD_MS: f64 = 60.0;  // Целевой пинг для облачного гейминга
                const STEP_UP: u64 = 500_000;      // Прибавляем по 1 Мбит
                const BACKOFF_FACTOR: f64 = 0.75;     // Срезаем 20% битрейта при лагах

                let current_bitrate = *bitrate_tx.borrow();
                let mut new_bitrate = current_bitrate;

                if rtt_ms > RTT_THRESHOLD_MS {
                    // 1. ЕСТЬ ЛАГ: Режем битрейт мультипликативно (быстро реагируем на затор)
                    new_bitrate = (current_bitrate as f64 * BACKOFF_FACTOR) as u64;
                } else if rtt_ms < (RTT_THRESHOLD_MS * 0.7) {
                    // 2. ВСЁ ЧИСТО: Постепенно повышаем (аддитивно), чтобы не вызвать резкий затор
                    new_bitrate = current_bitrate + STEP_UP;
                }

                // Зажимаем в рамки и проверяем на изменения
                new_bitrate = new_bitrate.clamp(MIN_BITRATE, MAX_BITRATE);

                let now = Instant::now();
                let can_update = unsafe {
                    LAST_UPDATE.map(|last| now.duration_since(last).as_millis() > 500).unwrap_or(true)
                };
                if new_bitrate != current_bitrate && can_update {
                    if (new_bitrate as i64 - current_bitrate as i64).abs() >= 50_000 {
                        log::warn!("[Congestion] RTT high ({:.1}ms), dropping bitrate to {:.2} Mbps", rtt_ms, new_bitrate as f64 / 1e6);
                        let _ = bitrate_tx.send(new_bitrate);
                        unsafe { LAST_UPDATE = Some(now); }
                    }
                }
            }
            ControlPacket::FrameFeedback { frame_id, trace } => {
                let t = trace;
                let label = info.label().await;

                let receive_srv     = client_to_server_us(t.receive_us, &clock_offset);
                let reassembled_srv = client_to_server_us(t.reassembled_us, &clock_offset);
                let decode_srv      = client_to_server_us(t.decode_us, &clock_offset);
                let present_srv     = client_to_server_us(t.present_us, &clock_offset);
                let off = clock_offset.load(Ordering::Relaxed);

                log::info!(
                    "[QUIC] {label} got frame trace. Offset is: {off}:
                    #{frame_id}: capture→encode={:.1}ms encode→serial={:.1}ms \
                    serial→recv={:.1}ms recv→reassem={:.1}ms reassem→decode={:.1}ms \
                    decode→present={:.1}ms  TOTAL={:.1}ms",
                    FrameTrace::ms(t.capture_us, t.encode_us),
                    FrameTrace::ms(t.encode_us, t.serialize_us),
                    FrameTrace::ms(t.serialize_us, receive_srv),
                    FrameTrace::ms(receive_srv, reassembled_srv),
                    FrameTrace::ms(reassembled_srv, decode_srv),
                    FrameTrace::ms(decode_srv, present_srv),
                    FrameTrace::ms(t.capture_us, present_srv),
                );
            }
            ControlPacket::Communication { message } => {
                let label = info.label().await;
                log::info!("[QUIC] {label} send a MESSAGE: {message}");
            }

            ControlPacket::RequestKeyFrame => {
                let label = info.label().await;
                log::info!("[QUIC] Client {label} requested KeyFrame!");
                let _ = idr_tx.send(true);
            }
            _ => ()
        }
    }
}