use common::NaluType;

use super::*;

pub(crate) async fn send_loop_to_client(
    conn: Connection,
    mut video_rx: broadcast::Receiver<Arc<SerializedFrame>>,
    info: Arc<ConnectionInfo>,
    clock_offset: &AtomicI64,
    idr_tx: watch::Sender<bool>,
    bitrate_tx: watch::Sender<u64>,
) {
    let mut started = false;
    let mut requested_initial_idr = false;
    let remote = conn.remote_address();
    
    let mut pacer = FramePacer::new(100.0, 4.0);
    
    loop {
        tokio::select! {
            // 1. ОТПРАВКА ВИДЕО
            video_result = video_rx.recv() => {
                match video_result {
                    Ok(serialized_frame) => { 
                        if !info.ready.load(Ordering::Acquire) {
                            continue;
                        }

                        if !started {
                            if !serialized_frame.is_key {
                                if !requested_initial_idr {
                                    let _ = idr_tx.send(true);
                                    let _ = idr_tx.send(false);
                                    requested_initial_idr = true;
                                    log::info!("[QUIC] Client {remote} waiting for keyframe: requested IDR");
                                }
                                continue;
                            }
                            started = true;
                            requested_initial_idr = false;
                        }

                        // Just pace and send the pre-computed bytes. Zero-copy.
                        for dgram in &serialized_frame.datagrams {
                            // As of now commented out because wait inside tokio produces blocking.
                            // let wait = pacer.consume(DatagramChunk::HEADER_LEN + dgram.len());
                            // if !wait.is_zero() {
                            //     tokio::time::sleep(wait).await;
                            // }

                            // .clone() on Bytes is extremely cheap (atomic ref-count bump)
                            if let Err(e) = conn.send_datagram(dgram.clone()) {
                                log::error!("[QUIC] Failed to send datagram to {}: {:?}", remote, e);
                                match e {
                                    quinn::SendDatagramError::TooLarge => {
                                        let limit = conn.max_datagram_size().unwrap_or(0);
                                        log::error!(
                                            "[QUIC] Datagram ({} bytes) is larger than allowed limit ({} bytes)", 
                                            dgram.len(), 
                                            limit
                                        );
                                        continue;
                                    }
                                    quinn::SendDatagramError::UnsupportedByPeer => {
                                        // Сеть перегружена. 
                                        // Вариант А: Просто пропустить (dropped by sender). 
                                        // Вариант Б: Немного подождать и попробовать снова (но это может вызвать лаг).
                                        return; 
                                    }
                                    quinn::SendDatagramError::Disabled => {
                                        return; // Смысла продолжать нет
                                    }
                                    quinn::SendDatagramError::ConnectionLost(_) => {
                                        return; 
                                    }
                                }
                                // Если ошибка "TooManyInFlight", можно попробовать 
                                // не делать return, а просто пропустить этот чанк.
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("[QUIC] Client {remote} lagged {n} frames, resetting to next IDR");
                        started = false; 
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // 2. ПРИЕМ КОМАНД ОТ КЛИЕНТА (Ping, RequestKeyFrame, и т.д.)
            
            incoming = conn.read_datagram() => {
                match incoming {
                    Ok(raw_data) => {
                        // Декодируем как наш чанк (клиент тоже шлет в этом формате)
                        if let Some(chunk) = DatagramChunk::decode(raw_data) {
                            let idr_clone = idr_tx.clone();
                            let bit_clone = bitrate_tx.clone();
                            if chunk.packet_type == TYPE_CONTROL {
                                handle_control(&conn, chunk.data, &info, clock_offset, idr_clone, bit_clone).await;
                            }
                        }
                    }
                    Err(e) => {
                        log::info!("[QUIC] Client {remote} read error: {e}");
                        break;
                    }
                }
            }
        }
        
            // 3. (В БУДУЩЕМ) ОТПРАВКА АУДИО
            // Ok(audio_msg) = audio_rx.recv() => { ... }
        
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
        let chunks = common::fec::encode::FecEncoder::encode(
            slice.frame_id,
            idx,
            total,
            &serialized, 
            max_chunk_data,
            flags
        );
        let mut datagrams = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            datagrams.push(chunk.to_bytes());
        }
        
        // 5. Broadcast: отправляем СЛАЙС немедленно
        let shared_part = Arc::new(SerializedFrame {
            frame_id: slice.frame_id,
            is_key: slice.is_key,
            datagrams,
        });

        // Кэшируем (важно: здесь кэш будет пополняться частями)
        shard_cache.insert(shared_part.frame_id, &shared_part.datagrams);
        
        let _ = broadcast_tx.send(shared_part);

        // 6. Если это был последний слайс в пакете от энкодера — следующий будет новым кадром
        if slice.is_last {
            is_new_frame = true;
        }
    }
}