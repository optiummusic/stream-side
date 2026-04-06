// src/quic.rs
//
// QUIC-сервер на стороне отправителя.
//
// Архитектура:
//
//   Encoder thread
//       │  try_send(Vec<u8>)          ← sync, O(1), без блокировки
//       ▼
//   mpsc::channel  (буфер 4 кадра)
//       │
//       ▼
//   serializer task                   ← сериализует VideoPacket ОДИН РАЗ
//       │  watch::send(Arc<Vec<u8>>)  ← автоматически дропает старый кадр
//       ▼
//   watch::channel
//       ├──▶ client task A  ──▶ persistent SendStream A
//       ├──▶ client task B  ──▶ persistent SendStream B
//       └──▶ ...
//
// Ключевые решения:
//   - ONE stream per connection:  вместо open_uni() на каждый кадр —
//     открываем стрим один раз при подключении и пишем в него бесконечно.
//   - Arc<Vec<u8>>: сериализованный пакет живёт в Arc, клиентские таски
//     получают только Arc::clone() — O(1), без копирования данных.
//   - watch вместо broadcast: watch автоматически выбрасывает старые кадры,
//     если клиент не успевает — именно то что нужно для low-latency видео.
use tokio::sync::broadcast;
use quinn::{Endpoint, ServerConfig};
use quinn::crypto::rustls::QuicServerConfig;
use bytes::Bytes;
use std::sync::Arc;
use std::net::SocketAddr;
use common::{DatagramChunk, VideoPacket};
use std::time::{SystemTime, UNIX_EPOCH};
// ─────────────────────────────────────────────────────────────────────────────

pub struct QuicServer {
    /// Отправитель закодированных HEVC-кадров.
    /// Используется из синхронного потока энкодера через try_send.
    tx: tokio::sync::mpsc::Sender<(Vec<u8>, bool)>,
}

impl QuicServer {
    /// Запустить QUIC-сервер, привязанный к `listen_addr`.
    ///
    /// Возвращает немедленно — все async-задачи работают в фоне.
    pub async fn new(listen_addr: SocketAddr) -> Self {
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, bool)>(4);

        let (broadcast_tx, _) = broadcast::channel::<Arc<(u64, Bytes, bool)>>(32);
        let server_broadcast_tx = broadcast_tx.clone();

        // ── Задача сериализации ──────────────────────────────────────────────
        //
        // Сериализуем VideoPacket ОДИН РАЗ для всех клиентов.
        // Результат (length-prefix + postcard-байты) упаковывается в Arc,
        // чтобы клиентские таски могли дёшево его клонировать.
        // ── Задача сериализации ──────────────────────────────────────────────
        tokio::spawn(async move {
            let mut frame_id = 0u64;
 
            while let Some((payload, is_key)) = frame_rx.recv().await {
                frame_id += 1;
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let packet = VideoPacket {
                    timestamp,
                    frame_id,
                    payload,
                    is_key,
                };

                if let Ok(bin) = postcard::to_allocvec(&packet) {
                    // Рассылаем данные ВМЕСТЕ с флагом ключевого кадра
                    let _ = server_broadcast_tx.send(Arc::new((frame_id, Bytes::from(bin), is_key)));
                }
            }
        });

        // ── Сервер ──────────────────────────────────────────────────────────
        let endpoint = build_server_endpoint(listen_addr);
        eprintln!("🎯 QUIC-сервер слушает на {}", listen_addr);

        tokio::spawn(async move {
            while let Some(connecting) = endpoint.accept().await {
                let client_rx = broadcast_tx.subscribe();

                tokio::spawn(async move {
                    match connecting.await {
                        Ok(conn) => {
                            let remote = conn.remote_address();
                            eprintln!("✅ Клиент подключился: {}", remote);

                            // Открываем ОДИН персистентный стрим для этого клиента
                            match conn.open_uni().await {
                                Ok(stream) => send_datagrams_to_client(conn, client_rx).await,
                                Err(e)     => eprintln!("❌ open_uni для {remote}: {e}"),
                            }

                            eprintln!("📤 Клиент отключился: {}", remote);
                        }
                        Err(e) => eprintln!("❌ Handshake failed: {e}"),
                    }
                });
            }
        });

        Self { tx: frame_tx }
    }

    /// Поставить закодированный HEVC-кадр в очередь отправки.
    ///
    /// Вызывается из синхронного потока энкодера.
    /// Если очередь заполнена — кадр **молча выбрасывается** (backpressure).
    pub fn send(&self, data: Vec<u8>, is_key: bool) {
        use tokio::sync::mpsc::error::TrySendError;
        match self.tx.try_send((data, is_key)) {
            Ok(_) => {}
            Err(TrySendError::Full(_))   => { /* сеть не успевает — дропаем */ }
            Err(TrySendError::Closed(_)) => eprintln!("❌ QuicServer channel closed"),
        }
    }
}

async fn send_datagrams_to_client(
    conn: quinn::Connection,
    mut rx: broadcast::Receiver<Arc<(u64, Bytes, bool)>>,
) {
    let mut started = false;
    loop {
        let msg = match rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("⚠️ Клиент отстал на {n} кадров, картинка может рассыпаться");
                // Если клиент отстал, сбрасываем started, чтобы дождаться нового I-кадра
                // и не пытаться декодировать битые P-кадры.
                started = false; 
                continue;
            }
            Err(_) => break, 
        };
        let (frame_id, ref data, is_key) = *msg;
        if !started {
            if !is_key {
                // Игнорируем P-кадры, пока не придет первый I-кадр
                continue;
            }
            // Как только пришел I-кадр, разрешаем отправку всех последующих кадров
            started = true;
            eprintln!("🚀 Отправка данных клиенту {} началась с I-frame #{}", conn.remote_address(), frame_id);
        }
        // Определяем максимальный размер полезных данных в одном датаграмме.
        // max_datagram_size() учитывает согласованный MTU и QUIC-оверхед.
        // Вычитаем ~20 байт для заголовка DatagramChunk (postcard varint).
        let max_chunk_data = conn
            .max_datagram_size()
            .unwrap_or(1200)
            .saturating_sub(50);
 
        let chunks: Vec<&[u8]> = data.chunks(max_chunk_data).collect();
        let total_chunks = chunks.len() as u16;
 
        for (idx, chunk_data) in chunks.iter().enumerate() {
            let dgram = DatagramChunk {
                frame_id,
                chunk_idx:    idx as u16,
                total_chunks,
                data:         chunk_data.to_vec(),
            };
 
            if let Ok(bytes) = postcard::to_allocvec(&dgram) {
                if let Err(e) = conn.send_datagram(Bytes::from(bytes)) {
                    // Если буфер ОС переполнен, Quinn может вернуть ошибку.
                    // В real-time мы просто игнорируем этот чанк — пусть дропается.
                    if !matches!(e, quinn::SendDatagramError::TooLarge) {
                        return; // Фатальная ошибка связи
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Создание QUIC server endpoint с self-signed сертификатом
// ─────────────────────────────────────────────────────────────────────────────

fn build_server_endpoint(addr: SocketAddr) -> Endpoint {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("Failed to generate TLS certificate");

    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        cert.signing_key.serialize_der().into(),
    );
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)
        .expect("TLS ServerConfig error");
    tls.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls)
        .expect("QuicServerConfig error");
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    server_cfg.transport_config(Arc::new(transport));

    Endpoint::server(server_cfg, addr).expect("Failed to bind QUIC server")
}
