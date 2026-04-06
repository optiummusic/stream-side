//! QUIC transport layer — sender side.
//!
//! # Architecture
//!
//! ```text
//!   VideoSender (encode thread)
//!       │  try_send((Vec<u8>, bool))   — O(1), non-blocking
//!       ▼
//!   mpsc channel (capacity = 4 frames)
//!       │
//!       ▼
//!   serialiser task                   — wraps payload in VideoPacket, ONE copy for all clients
//!       │  broadcast::send(Arc<Bytes>)  — O(1) per client, zero-copy fan-out
//!       ▼
//!   broadcast channel
//!       ├──▶ client task A  ──▶  QUIC datagrams → receiver A
//!       ├──▶ client task B  ──▶  QUIC datagrams → receiver B
//!       └──▶ ...
//! ```
//!
//! # Key design decisions
//!
//! | Decision | Rationale |
//! |---|---|
//! | QUIC datagrams instead of streams | UDP-like delivery; no head-of-line blocking |
//! | Fixed binary chunk header (12 B) | Replaces postcard varint on the per-datagram hot path |
//! | `broadcast` channel | Auto-drops stale frames when a slow client lags; ideal for live video |
//! | `Arc<Bytes>` fan-out | All clients share the same serialised buffer — zero copy after the first |
//! | `keep_alive_interval` | Detects dead LAN peers quickly without waiting for idle timeout |

use std::{net::SocketAddr, sync::Arc, time::Duration};
use bytes::Bytes;
use quinn::{Endpoint, ServerConfig};
use quinn::crypto::rustls::QuicServerConfig;
use tokio::sync::{broadcast, mpsc};
use common::{DatagramChunk, VideoPacket};
use std::time::{SystemTime, UNIX_EPOCH};

// ─────────────────────────────────────────────────────────────────────────────
// Public type
// ─────────────────────────────────────────────────────────────────────────────

pub struct QuicServer {
    /// Cloneable handle for pushing encoded frames into the transport pipeline.
    frame_tx: mpsc::Sender<(Vec<u8>, bool)>,
}

impl QuicServer {
    /// Start the QUIC server bound to `listen_addr`.
    ///
    /// Returns immediately; all async tasks run in the background on the
    /// current Tokio runtime.
    ///
    /// Use [`frame_sink`] to obtain a channel for delivering encoded frames.
    pub async fn new(listen_addr: SocketAddr) -> Self {
        let (frame_tx, mut frame_rx) = mpsc::channel::<(Vec<u8>, bool)>(4);

        // broadcast capacity = 64 frames  (~1 second of 60 fps with headroom).
        // watch is not used here because we need `is_key` alongside the data.
        let (bcast_tx, _) = broadcast::channel::<Arc<(u64, Bytes, bool)>>(64);
        let server_bcast  = bcast_tx.clone();

        // ── Serialiser task ──────────────────────────────────────────────────
        // Converts raw NAL slices into `VideoPacket`, serialises with postcard
        // once, and broadcasts the resulting `Arc<Bytes>` to all client tasks.
        tokio::spawn(async move {
            let mut frame_id = 0u64;

            while let Some((payload, is_key)) = frame_rx.recv().await {
                frame_id += 1;

                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let packet = VideoPacket { timestamp, frame_id, payload, is_key };

                match postcard::to_allocvec(&packet) {
                    Ok(bin) => {
                        // Wrap in Arc so clients share the allocation without copying.
                        let _ = server_bcast.send(Arc::new((
                            frame_id,
                            Bytes::from(bin),
                            is_key,
                        )));
                    }
                    Err(e) => log::error!("[QuicServer] serialise error: {e}"),
                }
            }
        });

        // ── Accept loop ──────────────────────────────────────────────────────
        let endpoint = build_server_endpoint(listen_addr);

        tokio::spawn(async move {
            while let Some(connecting) = endpoint.accept().await {
                let client_rx = bcast_tx.subscribe();

                tokio::spawn(async move {
                    match connecting.await {
                        Ok(conn) => {
                            log::info!("[QUIC] Client connected: {}", conn.remote_address());
                            send_datagrams_to_client(conn, client_rx).await;
                        }
                        Err(e) => log::warn!("[QUIC] Handshake failed: {e}"),
                    }
                });
            }
        });

        Self { frame_tx }
    }

    /// Return a cloneable sender for delivering encoded `(nal_bytes, is_key)` frames.
    ///
    /// Pass this to [`VideoSender::run`] to connect the capture/encode pipeline
    /// to the transport layer.
    pub fn frame_sink(&self) -> mpsc::Sender<(Vec<u8>, bool)> {
        self.frame_tx.clone()
    }

    /// Convenience method — push one encoded frame directly.
    ///
    /// Drops silently if the internal channel is full (back-pressure for live
    /// video; we never want to stall the encoder waiting for the network).
    pub fn send(&self, data: Vec<u8>, is_key: bool) {
        match self.frame_tx.try_send((data, is_key)) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::debug!("[QuicServer] frame channel full, dropping");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::warn!("[QuicServer] frame channel closed");
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-client send loop
// ─────────────────────────────────────────────────────────────────────────────

async fn send_datagrams_to_client(
    conn:    quinn::Connection,
    mut rx:  broadcast::Receiver<Arc<(u64, Bytes, bool)>>,
) {
    // Wait for the first IDR frame before sending anything.
    // This prevents the receiver from trying to decode a partial GOP.
    let mut started = false;

    // Cache the usable datagram payload size for this connection.
    // MTU negotiation completes during the handshake; after that the value is
    // stable for the lifetime of the connection on a non-changing path.
    //
    // 12 bytes: DatagramChunk fixed header (see DatagramChunk::encode).
    // We subtract a further 8 bytes of QUIC framing margin.
    let max_chunk_data = conn
        .max_datagram_size()
        .unwrap_or(1200)
        .saturating_sub(DatagramChunk::HEADER_LEN + 8);

    let remote = conn.remote_address();

    loop {
        let msg = match rx.recv().await {
            Ok(m) => m,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("[QUIC] Client {remote} lagged {n} frames, resetting to next IDR");
                started = false;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        let (frame_id, ref data, is_key) = *msg;

        if !started {
            if !is_key { continue; }
            started = true;
            log::info!("[QUIC] Streaming to {remote} from IDR #{frame_id}");
        }

        let total_chunks = ((data.len() + max_chunk_data - 1) / max_chunk_data) as u16;

        for (idx, offset) in (0..data.len()).step_by(max_chunk_data).enumerate() {
            let end   = (offset + max_chunk_data).min(data.len());
            // Zero-copy Bytes slice — no allocation for the payload.
            let slice = data.slice(offset..end);

            let dgram = DatagramChunk::encode(
                frame_id,
                idx as u16,
                total_chunks,
                &slice,
            );

            match conn.send_datagram(dgram) {
                Ok(_) => {}
                Err(quinn::SendDatagramError::TooLarge) => {
                    // Should not happen after subtracting header + margin, but
                    // if it does, skip this chunk rather than killing the stream.
                    log::debug!("[QUIC] Datagram too large for frame #{frame_id} chunk {idx}");
                }
                Err(e) => {
                    log::info!("[QUIC] Client {remote} disconnected: {e}");
                    return;
                }
            }
        }
    }

    log::info!("[QUIC] Client {remote} stream closed");
}

// ─────────────────────────────────────────────────────────────────────────────
// Endpoint construction
// ─────────────────────────────────────────────────────────────────────────────

fn build_server_endpoint(addr: SocketAddr) -> Endpoint {
    // ── TLS — self-signed certificate (LAN / development) ────────────────────
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("rcgen certificate generation failed");

    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        cert.signing_key.serialize_der().into(),
    );
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)
        .expect("TLS ServerConfig failed");
    tls.alpn_protocols = vec![b"video-stream".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls)
        .expect("QuicServerConfig failed");
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_crypto));

    // ── Transport tuning ─────────────────────────────────────────────────────
    let mut t = quinn::TransportConfig::default();

    // Datagram buffers — 16 MB handles burst of ~80 uncompressed HEVC frames
    // before the kernel starts dropping.
    t.datagram_receive_buffer_size(Some(16 * 1024 * 1024));
    t.datagram_send_buffer_size(8 * 1024 * 1024);

    // Initial MTU probe value for Ethernet LAN.
    //
    // Path MTU = 1500 (Ethernet) - 20 (IP) - 8 (UDP) - ~20 (QUIC) = ~1452.
    // We probe at 1400 to stay safe across VPN tunnels and 802.11 frames.
    // Quinn will discover the actual maximum via PMTUD automatically.
    t.initial_mtu(1400);

    // Keep-alive: prevents NAT table expiry and detects dead connections within
    // ~1.5 × keep_alive_interval, long before `max_idle_timeout` fires.
    t.keep_alive_interval(Some(Duration::from_millis(500)));

    // Declare the connection dead after 8 s of silence.
    // Avoids zombie connections from receivers that crashed without a FIN.
    t.max_idle_timeout(Some(
        Duration::from_secs(8)
            .try_into()
            .expect("idle timeout"),
    ));

    server_cfg.transport_config(Arc::new(t));

    Endpoint::server(server_cfg, addr).expect("QUIC endpoint bind failed")
}
