use std::{sync::Mutex, time::Instant};

use common::clock::FrameStep;

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
                let label = info.label().await;
                let clock = &info.clock; 

                // Хелпер для локального времени
                let get = |step: FrameStep| trace.get_local(step, clock) as u64;

                // ANSI Colors
                let cy = "\x1b[36m"; // Cyan (Server)
                let yl = "\x1b[33m"; // Yellow (Network)
                let gr = "\x1b[32m"; // Green (Client)
                let rd = "\x1b[31m"; // Red (Critical)
                let rs = "\x1b[0m";  // Reset

                log::info!(
                    "[QUIC] {} frame feedback. #{}:
                    {cy}Capture→Encode: {rs}{:>6.1}ms | {cy}Encode→Serial:  {rs}{:>6.1}ms
                    {yl}Serial→Network: {rs}{:>6.1}ms | {yl}Net→Reassem:    {rs}{:>6.1}ms
                    {gr}Reassem→Jitter: {rs}{:>6.1}ms | {gr}Jitter→Submit:  {rs}{:>6.1}ms
                    {gr}Submit→Decode:  {rs}{:>6.1}ms | {gr}Decode→Present: {rs}{:>6.1}ms
                    ---------------------------------------------------------
                    TOTAL (G2G):    {rd}{:>6.1}ms{rs}",
                    label, frame_id,
                    // Строка 1: Сервер
                    FrameTrace::ms(get(FrameStep::Capture),      get(FrameStep::Encode)),
                    FrameTrace::ms(get(FrameStep::Encode),       get(FrameStep::Serialize)),
                    // Строка 2: Сеть
                    FrameTrace::ms(get(FrameStep::Serialize),    get(FrameStep::Receive)),
                    FrameTrace::ms(get(FrameStep::Receive),      get(FrameStep::Reassembled)),
                    // Строка 3: Клиент (Джиттер и подготовка)
                    FrameTrace::ms(get(FrameStep::Reassembled),  get(FrameStep::JitterOut)),
                    FrameTrace::ms(get(FrameStep::JitterOut),    get(FrameStep::DecoderSubmit)),
                    // Строка 4: Клиент (Железо)
                    FrameTrace::ms(get(FrameStep::DecoderSubmit),get(FrameStep::Decode)),
                    FrameTrace::ms(get(FrameStep::Decode),       get(FrameStep::Present)),
                    // Итог
                    FrameTrace::ms(get(FrameStep::Capture),      get(FrameStep::Present)),
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
                    "[NACK] {label} frame={frame_id} slice={slice_idx} \
                    group={group_idx} received={received_mask:#066b}"
                );
            
                let retransmit = shard_cache.retransmit(
                    frame_id,
                    slice_idx,
                    group_idx,
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
                    log::warn!(
                        "[NACK] frame={frame_id} slice={slice_idx} group={group_idx} \
                        not in cache for {label}."
                    );
                }
            }
            
            ControlPacket::NackBatch { frame_id, entries } => {
                let label = info.label().await;
                log::debug!(
                    "[NACK] {label} batch frame={frame_id} groups={}",
                    entries.len()
                );
            
                // Одна блокировка чтения на весь батч.
                let retransmit = shard_cache.retransmit_batch(frame_id, &entries);
                let count = retransmit.len();
            
                for dgram in retransmit {
                    if let Err(e) = conn.send_datagram(dgram) {
                        log::warn!("[NACK] batch retransmit send failed for {label}: {e}");
                        break;
                    }
                }
            
                if count > 0 {
                    log::debug!(
                        "[NACK] retransmitted {count} shard(s) for \
                        frame={frame_id} ({} groups) to {label}",
                        entries.len()
                    );
                } else {
                    log::warn!(
                        "[NACK] frame={frame_id} not in cache for {label} \
                        (batch of {} groups).",
                        entries.len()
                    );
                }
            }
        _ => {}
        }
    }
    Ok(())
}

pub(crate) async fn handle_control(
    conn: &Connection, 
    data: Bytes, 
    info: &ConnectionInfo, 
    congestion_ctl: Arc<Mutex<CongestionController>>,
    senders: Senders,
) {
    if let Ok(packet) = postcard::from_bytes::<ControlPacket>(&data) {
        match packet {
            ControlPacket::Ping { client_time_us } => {
                let rtt = conn.stats().path.rtt.as_micros() as u64;
                let server_now = FrameTrace::now_us();
                // Сервер считает оффсет для этого конкретного клиента
                info.clock.sync(server_now, client_time_us, rtt);
                
                // Достаем уже отфильтрованный оффсет
                let current_offset = info.clock.get_offset();

                let pong = ControlPacket::Pong {
                    // Мы можем даже не слать client_time назад, 
                    // а сразу слать актуальный оффсет
                    offset: current_offset,
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

                let mut cc = congestion_ctl.lock().unwrap();
                let rtt = rtt / 1000;
                // 2. Скармливаем метрики и получаем действие
                let action = cc.on_metrics(rtt);

                // 3. Если контроллер решил сменить битрейт
                if let Some(new_bitrate) = action.new_bitrate {
                    log::debug!(
                        "[Congestion] RTT: {:.1}ms. New bitrate: {:.2} Mbps", 
                        rtt, 
                        new_bitrate as f64 / 1e6
                    );
                    let _ = senders.bitrate_tx.send(new_bitrate);
                }

                if let Some(fps) = action.target_fps {
                    log::debug!(
                        "[Congestion] FPS: {}.", 
                        fps
                    );
                    let _ = senders.capture_fps_tx.send(Some(fps));
                }
            },
            ControlPacket::Pong{..} => (),
            ControlPacket::LostFrame {..} => {
                let mut cc = congestion_ctl.lock().unwrap();
                cc.report_lost_frame();
            },
            ControlPacket::FrameFeedback { frame_id, trace } => {
                let label = info.label().await;
                let clock = &info.clock; 

                // Хелпер для локального времени
                let get = |step: FrameStep| trace.get_local(step, clock) as u64;

                // ANSI Colors
                let cy = "\x1b[36m"; // Cyan (Server)
                let yl = "\x1b[33m"; // Yellow (Network)
                let gr = "\x1b[32m"; // Green (Client)
                let rd = "\x1b[31m"; // Red (Critical)
                let rs = "\x1b[0m";  // Reset

                log::info!(
                    "[QUIC] {} frame feedback. #{}:
                    {cy}Capture→Encode: {rs}{:>6.1}ms | {cy}Encode→Serial:  {rs}{:>6.1}ms
                    {yl}Serial→Network: {rs}{:>6.1}ms | {yl}Net→Reassem:    {rs}{:>6.1}ms
                    {gr}Reassem→Jitter: {rs}{:>6.1}ms | {gr}Jitter→Submit:  {rs}{:>6.1}ms
                    {gr}Submit→Decode:  {rs}{:>6.1}ms | {gr}Decode→Present: {rs}{:>6.1}ms
                    ---------------------------------------------------------
                    TOTAL (G2G):    {rd}{:>6.1}ms{rs}",
                    label, frame_id,
                    // Строка 1: Сервер
                    FrameTrace::ms(get(FrameStep::Capture),      get(FrameStep::Encode)),
                    FrameTrace::ms(get(FrameStep::Encode),       get(FrameStep::Serialize)),
                    // Строка 2: Сеть
                    FrameTrace::ms(get(FrameStep::Serialize),    get(FrameStep::Receive)),
                    FrameTrace::ms(get(FrameStep::Receive),      get(FrameStep::Reassembled)),
                    // Строка 3: Клиент (Джиттер и подготовка)
                    FrameTrace::ms(get(FrameStep::Reassembled),  get(FrameStep::JitterOut)),
                    FrameTrace::ms(get(FrameStep::JitterOut),    get(FrameStep::DecoderSubmit)),
                    // Строка 4: Клиент (Железо)
                    FrameTrace::ms(get(FrameStep::DecoderSubmit),get(FrameStep::Decode)),
                    FrameTrace::ms(get(FrameStep::Decode),       get(FrameStep::Present)),
                    // Итог
                    FrameTrace::ms(get(FrameStep::Capture),      get(FrameStep::Present)),
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