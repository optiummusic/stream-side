use std::{sync::Mutex, time::Instant};

use crate::network::congestion::CongestionController;

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
    congestion_ctl: Arc<Mutex<CongestionController>>,
    senders: Senders,
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
                    let dgram = DatagramChunk{                  
                        payload_len: bin.len() as u16,
                        packet_type: TYPE_CONTROL,
                        data: bin.into(),
                        ..Default::default()
                    }.to_bytes();
                    let _ = conn.send_datagram(dgram);
                }
            },
            ControlPacket::Pong{..} => (),
            ControlPacket::LostFrame {..} => {
                let mut cc = congestion_ctl.lock().unwrap();
                cc.report_lost_frame();
            },
            ControlPacket::OffsetUpdate { offset_us, rtt_us } => {
                clock_offset.store(offset_us, Ordering::Relaxed);
                let rtt_ms = rtt_us as f64 / 1000.0;

                // 1. Получаем доступ к контроллеру
                let mut cc = congestion_ctl.lock().unwrap();
                
                // 2. Скармливаем метрики и получаем действие
                let action = cc.on_metrics(rtt_ms);

                // 3. Если контроллер решил сменить битрейт
                if let Some(new_bitrate) = action.new_bitrate {
                    log::warn!(
                        "[Congestion] RTT: {:.1}ms. New bitrate: {:.2} Mbps", 
                        rtt_ms, 
                        new_bitrate as f64 / 1e6
                    );
                    let _ = senders.bitrate_tx.send(new_bitrate);
                }

                if let Some(fps) = action.target_fps {
                    let _ = senders.capture_fps_tx.send(Some(fps));
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
                let _ = senders.idr_tx.send(true);
            }
            _ => ()
        }
    }
}