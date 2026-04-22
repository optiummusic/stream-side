
use bytes::BytesMut;
use common::{NaluType, TYPE_VIDEO};

use crate::network::congestion::CongestionController;

use super::*;

pub(crate) async fn send_loop_to_client(
    conn: Connection,
    mut video_rx: broadcast::Receiver<Arc<SerializedFrame>>,
    info: Arc<ConnectionInfo>,
    clock_offset: &AtomicI64,
    congestion_ctl: Arc<Mutex<CongestionController>>,
    senders: Senders,
) {
    let mut started = false;
    let mut requested_initial_idr = false;
    let remote = conn.remote_address();
    
    // Очередь для плавного выталкивания
    let mut pending_dgrams: std::collections::VecDeque<Bytes> = std::collections::VecDeque::with_capacity(4096);
    let mut pacer = FramePacer::new(1000.0, 4.0); // 100 Mbps baseline

    loop {
        tokio::select! {
            // 1. ПЕЙСЕР: Отправка следующей датаграммы из очереди
            _ = async {
                if pending_dgrams.is_empty() {
                    std::future::pending::<()>().await; // Бесконечно ждем, если очередь пуста
                }
                
                let next_len = pending_dgrams.front().unwrap().len();
                let wait = pacer.consume(DatagramChunk::HEADER_LEN + next_len);
                
                if wait.is_zero() {
                    return; // Можно слать немедленно
                } else if wait < std::time::Duration::from_millis(1) {
                    tokio::task::yield_now().await; // Микро-пауза
                } else {
                    tokio::time::sleep(wait).await; // Честный сон
                }
            }, if !pending_dgrams.is_empty() => {
                // Отправляем не 1 пакет, а небольшую пачку (например, 4 пакета), 
                // чтобы компенсировать задержки планировщика.
                for _ in 0..4 {
                    if let Some(dgram) = pending_dgrams.pop_front() {
                        if let Err(e) = conn.send_datagram(dgram.clone()) {
                            handle_send_error(e, &conn, &remote);
                        }
                    } else { break; }
                }
            }

            // 2. ПРИЕМ НОВЫХ КАДРОВ: Просто складываем их в очередь
            video_result = video_rx.recv() => {
                match video_result {
                    Ok(serialized_frame) => { 
                        if !info.ready.load(Ordering::Acquire) { continue; }

                        if !started {
                            if !serialized_frame.is_key {
                                if !requested_initial_idr {
                                    let _ = senders.idr_tx.send(true);
                                    requested_initial_idr = true;
                                }
                                continue;
                            }
                            started = true;
                            requested_initial_idr = false;
                        }

                        // Вместо немедленной отправки — пушим в очередь.
                        // Это и есть "разблокировка" — мы мгновенно освобождаем video_rx.
                        for dgram in &serialized_frame.datagrams {
                            pending_dgrams.push_back(dgram.clone());
                        }
                        
                        // Если очередь стала слишком большой (> 500 пакетов), 
                        // значит мы катастрофически не успеваем. Можно дропнуть старое.
                        if pending_dgrams.len() > 2048 {
                            log::warn!("[QUIC] Pacer overflow, current size: {}", pending_dgrams.len());
                            pending_dgrams.drain(..1024);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("[QUIC] Client {remote} lagged {n} frames");
                        started = false;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // 3. ПРИЕМ КОМАНД: Читается параллельно, не дожидаясь отправки видео
            incoming = conn.read_datagram() => {
                if let Ok(raw_data) = incoming {
                    if let Some(chunk) = DatagramChunk::decode(raw_data) {
                        if chunk.packet_type == TYPE_CONTROL {
                            handle_control(&conn, chunk.data, &info, clock_offset, congestion_ctl.clone(), senders.clone()).await;
                        }
                    }
                } else { break; }
            }
        }
    }
}

// Вынес обработку ошибок в хелпер для чистоты
fn handle_send_error(e: quinn::SendDatagramError, conn: &Connection, remote: &std::net::SocketAddr) {
    match e {
        quinn::SendDatagramError::TooLarge => {
            let limit = conn.max_datagram_size().unwrap_or(0);
            log::error!("[QUIC] Datagram too large: limit is {limit} bytes");
        }
        quinn::SendDatagramError::UnsupportedByPeer => log::warn!("[QUIC] Peer {remote} doesn't support datagrams"),
        quinn::SendDatagramError::Disabled => log::error!("[QUIC] Datagrams disabled"),
        _ => log::error!("[QUIC] Send error to {remote}: {:?}", e),
    }
}

pub(crate) async fn run_serialiser_task(
    mut slice_rx: mpsc::Receiver<EncodedFrame>,
    broadcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
    shard_cache: Arc<ShardCache>,
) {
    let mut current_frame_id = 0u64;
    let mut is_new_frame = true;
    
    // Calculate a safe max chunk size for the broadcast.
    // Quinn's initial_mtu is 1200, so we use that as our baseline constraint.
    let max_dgram = 1140;
    let max_chunk_data = max_dgram - DatagramChunk::HEADER_LEN - 64;

    while let Some(mut slice) = slice_rx.recv().await {
        // 1. Управление ID кадра: инкрементим, только когда пришел первый слайс нового кадра
        if is_new_frame {
            current_frame_id += 1;
            is_new_frame = false;
        }
        slice.frame_id = current_frame_id;

        if let Some(t) = slice.trace.as_mut() {
            t.serialize_us = FrameTrace::now_us(); 
        }

        // 2. Подготовка флагов для FEC
        // Используем nalu_type для определения критичности
        let mut flags = if slice.is_key { 1 } else { 0 };
        let nalu_type = slice.nalu_type;
        let is_critical = matches!(
            nalu_type, 
            NaluType::VideoParamSet | NaluType::SeqParamSet | NaluType::PicParamSet | NaluType::SliceIdr
        );
        if is_critical { flags |= 2; }
        let idx = slice.slice_idx;
        let total = slice.total_slices;
        // 3. Оборачиваем один NALU в VideoSlice
        let video_slice = VideoSlice {
            frame_id: slice.frame_id,
            slice_idx: idx,
            total_slices: total,
            is_key: slice.is_key,
            is_last: slice.is_last,
            payload: slice.data.to_vec(),
            trace: slice.trace,
            nal_type: nalu_type,
        };

        // 4. Сериализация и FEC
        let serialized = postcard::to_allocvec(&video_slice).unwrap_or_default();
        let (encoded_slice, total_groups) = common::fec::encode::FecEncoder::encode(
            slice.frame_id,
            slice.slice_idx,
            slice.total_slices,
            &serialized,
            max_chunk_data,
            flags
        );

        let mut datagrams = Vec::with_capacity(encoded_slice.chunks_meta.len());
        
        for meta in encoded_slice.chunks_meta {
            // Определяем размер конкретного шарда в буфере
            // Важно: берем данные или паддинг до полного размера шарда
            let shard_size = if meta.shard_idx < meta.k {
                // Для данных нам достаточно payload_len, но RS работает блоками. 
                // Чтобы ресивер не сошел с ума, шлем shard_size (он заложен в смещениях)
                // Но можно оптимизировать и слать только payload_len
                meta.payload_len as usize 
            } else {
                // Для паритета — всегда полный shard_size. 
                // Его можно вычислить как разницу смещений или из метаданных (если добавить туда)
                // Для простоты здесь предположим, что паритет того же размера что и данные
                meta.payload_len as usize 
            };

            // Делаем ZERO-COPY срез данных
            let shard_data = encoded_slice.all_shards_data.slice(meta.offset .. meta.offset + shard_size);

            // Собираем финальную датаграмму (с заголовком)
            let dgram = DatagramChunk::encode(
                slice.frame_id,
                slice.slice_idx,
                slice.total_slices,
                meta.shard_idx,
                meta.k,
                meta.m,
                meta.payload_len,
                TYPE_VIDEO,
                flags,
                meta.group_idx,
                total_groups,
                &shard_data // Передаем ссылку на срез
            );
            datagrams.push(dgram);
        }
        
        // 4. Формируем пакет для рассылки
        let shared_part = Arc::new(SerializedFrame {
            frame_id: slice.frame_id,
            is_key: slice.is_key,
            datagrams, // Здесь Vec<Bytes>, где каждый Bytes — это готовый UDP пакет
        });

        // Кэшируем для NACK-ов
        shard_cache.insert(shared_part.frame_id, &shared_part.datagrams);
        
        // Шлем в send_loop
        let _ = broadcast_tx.send(shared_part);

        if slice.is_last {
            is_new_frame = true;
        }
    }
}