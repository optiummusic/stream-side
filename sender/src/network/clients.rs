use super::*;

pub(crate) async fn send_loop_to_client(
    conn: Connection,
    mut video_rx: broadcast::Receiver<Arc<SerializedFrame>>,
    info: Arc<ConnectionInfo>,
    clock_offset: &AtomicI64,
    idr_tx: watch::Sender<bool>,
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
                            if conn.send_datagram(dgram.clone()).is_err() {
                                log::error!("[QUIC] Failed to send datagram to {}", remote);
                                return; 
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
                            if chunk.packet_type == TYPE_CONTROL {
                                handle_control(&conn, chunk.data, &info, clock_offset, idr_clone).await;
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