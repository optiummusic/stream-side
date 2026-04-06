// src/network.rs
//
// Кроссплатформенный сетевой цикл приёма видеопотока.
//
// Receiver теперь — QUIC-КЛИЕНТ: подключается к sender'у (серверу).
//
// Поток исполнения:
//
//   run_quic_receiver(sender_addr)
//       │
//       ├── reconnect loop
//       │       │
//       │       ├── endpoint.connect(sender_addr) → connection
//       │       │
//       │       └── connection.accept_uni() → RecvStream
//       │               │
//       │               └── handle_stream (читает пакеты в цикле до закрытия стрима)
//       │                       │
//       │                       ├── backend.push_encoded(payload)
//       │                       └── backend.poll_output() → frame_tx или Surface
//       │
//       └── при потере соединения — пауза 2с → reconnect

use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use common::{DatagramChunk, VideoPacket};
use quinn::{Endpoint};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;
use tokio::time::timeout;
use bytes::Bytes;

use crate::backend::{FrameOutput, VideoBackend};


struct ReassemblyBuf {
    chunks:   Vec<Option<Vec<u8>>>, // слоты по chunk_idx
    received: u16,
    total:    u16,
}
 
impl ReassemblyBuf {
    fn new(total_chunks: u16) -> Self {
        Self {
            chunks:   vec![None; total_chunks as usize],
            received: 0,
            total:    total_chunks,
        }
    }
 
    /// Вставить чанк. Возвращает true если кадр собран полностью.
    fn insert(&mut self, idx: u16, data: Vec<u8>) -> bool {
        let slot = &mut self.chunks[idx as usize];
        if slot.is_none() {
            *slot = Some(data);
            self.received += 1;
        }
        self.received == self.total
    }
 
    /// Склеить все чанки в единый буфер (только после insert вернул true).
    fn assemble(self) -> Vec<u8> {
        self.chunks
            .into_iter()
            .map(|c| c.expect("Logic error: assembling incomplete frame")) // Тут должен быть Some
            .flatten()
            .collect()
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Точка входа
// ─────────────────────────────────────────────────────────────────────────────

/// Запустить QUIC-клиент и подключиться к sender'у на `sender_addr`.
///
/// При потере соединения автоматически переподключается.
///
/// - `backend`   — платформо-специфичный декодер.
/// - `sender_addr` — адрес QUIC-сервера (sender): "192.168.1.5:4433"
/// - `frame_tx`  — `Some(tx)` на десктопе, `None` на Android.
pub async fn run_quic_receiver<B: VideoBackend>(
    backend:     Arc<Mutex<B>>,
    sender_addr: SocketAddr,
    frame_tx:    Option<mpsc::SyncSender<crate::backend::YuvFrame>>,
) -> Result<(), Box<dyn Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();
 
    loop {
        eprintln!("🔌 Подключаемся к {}...", sender_addr);
 
        let endpoint = match build_quic_client_endpoint() {
            Ok(ep) => ep,
            Err(e) => {
                eprintln!("❌ Endpoint error: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
 
        let conn = match endpoint.connect(sender_addr, "localhost") {
            Ok(c) => match c.await {
                Ok(c)  => c,
                Err(e) => {
                    eprintln!("❌ Connect failed: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("❌ connect() error: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
 
        eprintln!("✅ Подключились к {}!", conn.remote_address());
 
        receive_datagrams(conn, backend.clone(), frame_tx.clone()).await;
 
        eprintln!("⚠️  Соединение разорвано, переподключаемся через 2с...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Обработка одного QUIC-стрима
// ─────────────────────────────────────────────────────────────────────────────

async fn receive_datagrams<B: VideoBackend>(
    conn:     quinn::Connection,
    backend:  Arc<Mutex<B>>,
    frame_tx: Option<mpsc::SyncSender<crate::backend::YuvFrame>>,
) {
    // HashMap<frame_id → буфер сборки>
    // Держим не более MAX_BUFFERED_FRAMES кадров одновременно.
    // Если получаем кадр с frame_id намного впереди — старые выбрасываем.
    const MAX_BUFFERED_FRAMES: usize = 8;
 
    let mut reassembly: HashMap<u64, ReassemblyBuf> = HashMap::new();
 
    loop {
        // read_datagram() — неблокирующий await, возвращает целый датаграмм
        let bytes: Bytes = match conn.read_datagram().await {
            Ok(b)  => b,
            Err(e) => {
                eprintln!("⚠️  read_datagram: {e}");
                break; // соединение закрыто
            }
        };
 
        // ── Десериализуем чанк ────────────────────────────────────────────
        let chunk: DatagramChunk = match postcard::from_bytes(&bytes) {
            Ok(c)  => c,
            Err(e) => {
                log::warn!("[QUIC] Bad datagram: {e}");
                continue;
            }
        };
 
        let frame_id = chunk.frame_id;
        // ── Выбрасываем устаревшие незавершённые кадры ────────────────────
        //
        // Если пришёл кадр с frame_id = N, все буферы с id < N бесполезны:
        // их чанки потеряны, и декодировать их незачем.
        if reassembly.len() >= MAX_BUFFERED_FRAMES {
            let oldest = reassembly.keys().copied().min().unwrap_or(0);
            reassembly.remove(&oldest);
            log::debug!("[Reassembly] Evicted stale frame #{}", oldest);
        }
        reassembly.retain(|&id, _| id >= frame_id.saturating_sub(MAX_BUFFERED_FRAMES as u64));
 
        // ── Вставляем чанк в буфер ────────────────────────────────────────
        let buf = reassembly
            .entry(frame_id)
            .or_insert_with(|| ReassemblyBuf::new(chunk.total_chunks));
 
        // Защита от чанка с неверным total_chunks (мало ли — сеть)
        if chunk.chunk_idx >= buf.total || chunk.total_chunks != buf.total {
            log::warn!("[Reassembly] Bad chunk idx {}/{} for frame #{}", chunk.chunk_idx, chunk.total_chunks, frame_id);
            continue;
        }
 
        let complete = buf.insert(chunk.chunk_idx, chunk.data);
 
        if !complete {
            continue; // ждём остальные чанки
        }
        // ── Кадр собран — вынимаем из map и собираем ─────────────────────
        let serialized = reassembly.remove(&frame_id).unwrap().assemble();
 
        let packet: VideoPacket = match postcard::from_bytes(&serialized) {
            Ok(p)  => p,
            Err(e) => {
                log::error!("[Reassembly] Deserialize frame #{}: {e}", frame_id);
                continue;
            }
        };
 
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        let latency = now.saturating_sub(packet.timestamp);
        if packet.frame_id % 100 == 0 {
            log::info!("[Latency] Frame #{} | Network + Queue: {}ms", packet.frame_id, latency);
        }
 
        // ── Передаём в декодер ────────────────────────────────────────────
        {
            let mut b = backend.lock().unwrap();
 
            match b.push_encoded(&packet.payload, packet.frame_id) {
                Ok(())  => {}
                Err(crate::backend::BackendError::BufferFull) => {
                    log::warn!("[Decoder] Buffer full, dropping frame #{}", packet.frame_id);
                    continue;
                }
                Err(e) => {
                    log::error!("[Decoder] push_encoded: {e}");
                    continue;
                }
            }
 
            loop {
                match b.poll_output() {
                    Ok(FrameOutput::Yuv(frame)) => {
                        if let Some(ref tx) = frame_tx {
                            let _ = tx.try_send(frame);
                        }
                    }
                    Ok(FrameOutput::DirectToSurface) => {}
                    Ok(FrameOutput::Pending) | Err(_) => break,
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QUIC client endpoint (skip-verify для LAN / self-signed)
// ─────────────────────────────────────────────────────────────────────────────

fn build_quic_client_endpoint() -> Result<Endpoint, Box<dyn Error>> {
    // 1. Настройка TLS (уже есть)
    let mut crypto = rustls::ClientConfig::builder_with_provider(
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
    .with_no_client_auth();

    crypto.alpn_protocols = vec![b"video-stream".to_vec()];


    
    // Если тестируешь через VPN/Tailscale, иногда полезно ограничить MTU
    // transport_config.initial_mtu(1200); 

    let quic_crypto = QuicClientConfig::try_from(crypto)?;
    
    // 3. Собираем ClientConfig, объединяя крипту и транспорт
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));

    // 2. Создаем транспортный конфиг
    let mut transport = quinn::TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    client_config.transport_config(Arc::new(transport));

    // 4. Создаем эндпоинт
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    Ok(endpoint)
}

/// TLS-верификатор, принимающий любые сертификаты.
/// Используется только в разработке / LAN!
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity:    &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name:   &ServerName<'_>,
        _ocsp_response: &[u8],
        _now:           rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
