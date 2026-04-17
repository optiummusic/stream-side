use crate::VideoWorkerMsg;

use super::*;

pub(crate) fn spawn_control_writer_task(
    conn: quinn::Connection,
    mut control_rx: mpsc::Receiver<ControlPacket>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Открываем unidirectional stream для управления
        // В идеале: открываем один раз или на каждый важный пакет.
        // Для простоты и надежности при 1% потерь — открываем один поток
        // и пишем в него последовательно.
        
        let mut control_stream: Option<quinn::SendStream> = None;

        while let Some(packet) = control_rx.recv().await {
            let is_nack = matches!(packet, ControlPacket::Nack { .. });
            
            if let Ok(bytes) = postcard::to_stdvec(&packet) {
                if is_nack {
                    // --- ОТПРАВКА ЧЕРЕЗ STREAM (Надежно) ---
                    if control_stream.is_none() {
                        match conn.open_uni().await {
                            Ok(s) => control_stream = Some(s),
                            Err(e) => {
                                log::error!("[QUIC] Failed to open control stream: {e}");
                                continue;
                            }
                        }
                    }

                    if let Some(ref mut s) = control_stream {
                        // Пишем длину + данные (postcard не самоописывающийся в потоке)
                        let len = (bytes.len() as u32).to_le_bytes();
                        if s.write_all(&len).await.is_err() || s.write_all(&bytes).await.is_err() {
                            log::warn!("[QUIC] Control stream broken, resetting...");
                            control_stream = None; 
                        }
                    }
                } else {
                    // --- ОТПРАВКА ЧЕРЕЗ DATAGRAM (Быстро/Ненадежно: Пинги, фидбек) ---
                    let dgram = DatagramChunk::encode(
                         0, 0, 1, 0, 1, 0, 
                        bytes.len() as u16, 
                        TYPE_CONTROL, 0, &bytes
                    );
                    let _ = conn.send_datagram(dgram);
                }
            }
        }
    })
}

pub(crate) fn spawn_trace_feedback_task(
    trace_rx: watch::Receiver<Option<(u64, FrameTrace)>>,
    control_tx: mpsc::Sender<ControlPacket>,
) -> JoinHandle<()> {
            log::info!("Got some trace");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        let mut last_sent_id = 0u64;
        loop {
            interval.tick().await;

            let latest = trace_rx.borrow().clone();

            if let Some((frame_id, trace)) = latest {
                if frame_id != last_sent_id {
                    last_sent_id = frame_id;

                    let packet = ControlPacket::FrameFeedback { frame_id, trace };
                    if control_tx.send(packet).await.is_err() {
                        log::warn!("[NETWORK] Trace feedback task is broken!");
                        break;
                    }
                }
            }
        }
    })
}

pub(crate) fn spawn_ping_task(conn: quinn::Connection) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut count = 0u64;

        loop {
            let ping = ControlPacket::Ping {
                client_time_us: FrameTrace::now_us(),
            };

            if let Ok(bytes) = postcard::to_stdvec(&ping) {
                let dgram = DatagramChunk::encode(
                    0, 0, 1, 0, 1, 0, 
                    bytes.len() as u16, 
                    TYPE_CONTROL, 0, &bytes
                );
                let _ = conn.send_datagram(dgram);
            }

            count += 1;
            let delay = if count < 10 {
                Duration::from_millis(200)
            } else {
                Duration::from_secs(3)
            };

            tokio::time::sleep(delay).await;
        }
    })
}

pub(crate) fn spawn_idr_solicitor(
    control_tx: mpsc::Sender<ControlPacket>,
    mut idr_needed_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Настраиваем интервал один раз
        let mut interval = tokio::time::interval(Duration::from_millis(750));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // Ждем, пока значение станет true
            if !*idr_needed_rx.borrow_and_update() {
                if idr_needed_rx.changed().await.is_err() { break; }
                if !*idr_needed_rx.borrow() { continue; }
            }

            // Как только стало true — начинаем цикл запросов
            log::warn!("[Video] Loss detected, starting IDR solicitation loop...");
            
            while *idr_needed_rx.borrow() {
                if control_tx.send(ControlPacket::RequestKeyFrame).await.is_err() {
                    return; // Канал закрыт, выходим совсем
                }

                tokio::select! {
                    _ = interval.tick() => {}, 
                    // Если за время ожидания тика статус сменился на false — выходим из while
                    res = idr_needed_rx.changed() => {
                        if res.is_err() { return; }
                    }
                }
            }
            
            log::info!("[Video] IDR satisfied, solicitor going to sleep.");
            // Сбрасываем интервал, чтобы следующий цикл начался с чистого листа (без мгновенного тика)
            interval.reset(); 
        }
    })
}

pub(crate) fn spawn_decoder_poll_task(
    worker_tx: std::sync::mpsc::SyncSender<VideoWorkerMsg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(3));

        loop {
            interval.tick().await;

            if worker_tx.send(VideoWorkerMsg::PollDecoder).is_err() {
                log::error!("[Decoder] worker channel closed");
                break;
            }
        }
    })
}