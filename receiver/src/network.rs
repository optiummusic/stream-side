//! QUIC receiver — client-side transport loop.
//!
//! # Architecture
//!
//! ```text
//! run_quic_receiver(sender_addr)
//!     │
//!     ├── build_quic_client_endpoint()   ← built ONCE, reused across reconnects
//!     │
//!     └── reconnect loop
//!             │
//!             ├── endpoint.connect(sender_addr)
//!             │
//!             └── receive_datagrams(conn, backend, frame_tx)
//!                     │
//!                     ├── conn.read_datagram() → DatagramChunk::decode (zero-copy)
//!                     ├── ReassemblyBuf per frame_id (HashMap)
//!                     ├── on complete → postcard::from_bytes → VideoPacket
//!                     └── backend.push_encoded() + backend.poll_output()
//! ```
//!
//! # Reconnect behaviour
//! If the connection drops, the loop sleeps 2 s and reconnects automatically.
//! The QUIC `Endpoint` itself is created once and reused — TLS session tickets
//! are cached by rustls, making subsequent connections cheaper (0-RTT eligible).

use std::{
    collections::HashMap,
    error::Error,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::mpsc;
use bytes::Bytes;
use common::{DatagramChunk, VideoPacket, FrameTrace};
use quinn::Endpoint;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;

use crate::backend::{FrameOutput, VideoBackend};

// ─────────────────────────────────────────────────────────────────────────────
// Datagram reassembly buffer
// ─────────────────────────────────────────────────────────────────────────────

struct ReassemblyBuf {
    /// Slot per chunk index; `None` until the chunk arrives.
    chunks:   Vec<Option<Bytes>>,
    received: u16,
    total:    u16,
    is_key:       bool,   // ← НОВОЕ: чтобы не эвиктировать IDR
    first_us:     u64,
}

impl ReassemblyBuf {
    fn new(total_chunks: u16, is_key: bool) -> Self {
        Self {
            chunks:   vec![None; total_chunks as usize],
            received: 0,
            total:    total_chunks,
            is_key,
            first_us: FrameTrace::now_us(),
        }
    }

    /// Insert a chunk.  Returns `true` when all chunks have arrived.
    fn insert(&mut self, idx: u16, data: Bytes) -> bool {
        let slot = &mut self.chunks[idx as usize];
        if slot.is_none() {
            *slot = Some(data);
            self.received += 1;
        }
        self.received == self.total
    }

    /// Concatenate all chunks into a contiguous buffer.
    ///
    /// Must only be called when `insert` returned `true`.
    fn assemble(self) -> Vec<u8> {
        let total_len: usize = self.chunks.iter()
            .filter_map(|c| c.as_ref())
            .map(|b| b.len())
            .sum();

        let mut out = Vec::with_capacity(total_len);
        for chunk in self.chunks {
            out.extend_from_slice(&chunk.expect("assemble: incomplete chunk"));
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Start the QUIC receiver and connect to the sender at `sender_addr`.
///
/// Automatically reconnects on connection loss.
///
/// - `backend`     — platform-specific HEVC decoder + renderer.
/// - `sender_addr` — address of the QUIC server (sender): `"192.168.1.5:4433"`.
/// - `frame_tx`    — `Some(tx)` on desktop (YUV frames → render thread),
///                   `None` on Android (frames decoded directly to Surface).
///
/// # Errors
/// Returns only if endpoint construction fails (TLS config error, port bind
/// failure).  Connection errors and decode errors are logged and retried.
pub async fn run_quic_receiver<B: VideoBackend>(
    backend:     Arc<Mutex<B>>,
    sender_addr: SocketAddr,
    frame_tx:    Option<mpsc::Sender<crate::backend::YuvFrame>>,
) -> Result<(), Box<dyn Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();

    // Build the endpoint ONCE.  Reusing it across reconnects preserves the
    // TLS session cache, which allows 0-RTT on subsequent connections and
    // avoids re-parsing the certificate on every attempt.
    let endpoint = build_quic_client_endpoint()?;
    loop {
        log::info!("[QUIC] Connecting to {sender_addr}...");

        let conn = match endpoint.connect(sender_addr, "localhost") {
            Ok(c) => match c.await {
                Ok(c)  => c,
                Err(e) => {
                    log::warn!("[QUIC] Connect failed: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            Err(e) => {
                log::warn!("[QUIC] endpoint.connect error: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        log::info!("[QUIC] Connected to {}", conn.remote_address());
        
        receive_datagrams(conn, backend.clone(), frame_tx.clone()).await;
        log::warn!("[QUIC] Connection lost, reconnecting in 2 s...");
        tokio::time::sleep(Duration::from_secs(2)).await;

    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-connection datagram loop
// ─────────────────────────────────────────────────────────────────────────────

async fn receive_datagrams<B: VideoBackend>(
    conn:     quinn::Connection,
    backend:  Arc<Mutex<B>>,
    frame_tx: Option<mpsc::Sender<crate::backend::YuvFrame>>,
) {
    let mut waiting_for_key = false;
    let mut expected_frame_id: Option<u64> = None;
    const MAX_BUFFERED_FRAMES: usize = 8;

    let mut reassembly: HashMap<u64, ReassemblyBuf> = HashMap::new();

    loop {
        let raw: Bytes = match conn.read_datagram().await {
            Ok(b)  => b,
            Err(e) => {
                log::info!("[QUIC] read_datagram: {e}");
                break;
            }
        };

        let chunk = match DatagramChunk::decode(raw) {
            Some(c) => c,
            None    => continue,
        };

        let frame_id = chunk.frame_id;

        if chunk.is_key && chunk.chunk_idx == 0 {
            for (id, buf) in reassembly.drain() {
                let missing = buf.total - buf.received;
                if missing > 0 {
                    log::debug!("[Reassembly] Clearing incomplete frame #{} for new IDR (missing {} chunks)", id, missing);
                }
            }
        }

        reassembly.retain(|&id, buf| {
            let keep = buf.is_key || id >= frame_id.saturating_sub(MAX_BUFFERED_FRAMES as u64);
            if !keep {
                waiting_for_key = true;
            }
            keep
        });

        if reassembly.len() >= MAX_BUFFERED_FRAMES {
            let evict_id = reassembly.iter().filter(|(_, buf)| !buf.is_key).map(|(&id, _)| id).min()
                .or_else(|| reassembly.keys().copied().min());
            if let Some(id) = evict_id {
                reassembly.remove(&id);
            }
        }

        let buf = reassembly.entry(frame_id).or_insert_with(|| ReassemblyBuf::new(chunk.total_chunks, chunk.is_key));

        if chunk.chunk_idx >= buf.total || chunk.total_chunks != buf.total { continue; }

        // Если не все куски собраны — уходим на следующую итерацию
        if !buf.insert(chunk.chunk_idx, chunk.data) { continue; }

        // === КАДР СОБРАН ===
        let buf = reassembly.remove(&frame_id).unwrap();
        let first_us = buf.first_us;
        let serialised = buf.assemble();

        let mut packet: VideoPacket = match postcard::from_bytes(&serialised) {
            Ok(p)  => p,
            Err(_) => continue,
        };
        
        packet.trace.as_mut().map(|t| {
            t.receive_us     = first_us;
            t.reassembled_us = FrameTrace::now_us();
        });

        // === ИНЛАЙН ДЕКОДИРОВАНИЕ ===
        // Берем лок, закидываем кадр, забираем YUV и СРАЗУ отдаем лок.
        let mut backend_lock = backend.lock().unwrap();

        if packet.is_key {
            waiting_for_key = false;
            expected_frame_id = Some(packet.frame_id);
        } else if waiting_for_key {
            continue;
        } else if packet.frame_id != expected_frame_id.unwrap_or_default() {
            log::warn!("[Decoder] Sequence broken. Dropping frames until next IDR.");
            waiting_for_key = true;
            continue;
        }
        expected_frame_id = Some(packet.frame_id + 1);

        match backend_lock.push_encoded(&packet.payload, packet.frame_id, packet.trace.take()) {
            Ok(()) => {}
            Err(crate::backend::BackendError::BufferFull) => {
                waiting_for_key = true;
                continue;
            }
            Err(_) => {
                waiting_for_key = true;
                continue;
            }
        }

        loop {
            match backend_lock.poll_output() {
                Ok(FrameOutput::Yuv(frame)) => {
                    if let Some(ref tx) = frame_tx {
                        let _ = tx.try_send(frame);
                    }
                }
                Ok(FrameOutput::DirectToSurface) => {}
                Ok(FrameOutput::Pending) | Err(_) => break,
            }
        }
        drop(backend_lock); // Важно освободить Mutex ДО того, как уйдем на conn.read_datagram().await!
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client endpoint construction
// ─────────────────────────────────────────────────────────────────────────────

fn build_quic_client_endpoint() -> Result<Endpoint, Box<dyn Error>> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
    .with_no_client_auth();

    crypto.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicClientConfig::try_from(crypto)?;
    let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_crypto));

    // ── Transport — mirror the server settings for symmetrical behaviour ──────
    let mut t = quinn::TransportConfig::default();

    t.datagram_receive_buffer_size(Some(16 * 1024 * 1024));

    // Match the server's initial MTU probe so the first handshake uses the
    // optimal path MTU immediately rather than starting at 1200.
    t.initial_mtu(1400);

    // Keep-alive: same period as server so both sides detect dead connections
    // within a consistent window.
    t.keep_alive_interval(Some(Duration::from_millis(500)));

    t.max_idle_timeout(Some(
        Duration::from_secs(8)
            .try_into()
            .expect("idle timeout"),
    ));

    client_cfg.transport_config(Arc::new(t));

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_cfg);
    Ok(endpoint)
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS skip-verify (LAN / self-signed, NOT for production over untrusted networks)
// ─────────────────────────────────────────────────────────────────────────────

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