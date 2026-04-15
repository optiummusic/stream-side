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

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use bytes::Bytes;
use quinn::{Endpoint, ServerConfig};
use quinn::crypto::rustls::QuicServerConfig;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, watch};
use common::{ControlPacket, DatagramChunk, FrameTrace, TYPE_CONTROL, TYPE_VIDEO, VideoPacket, VideoSlice};
use crate::{ClientIdentity, ConnectionInfo, FramePacer, SerializedFrame};
use crate::encode::EncodedFrame;

// ─────────────────────────────────────────────────────────────────────────────
// Public type
// ─────────────────────────────────────────────────────────────────────────────

pub struct QuicServer {
    /// Cloneable handle for pushing encoded frames into the transport pipeline.
    frame_tx: mpsc::Sender<EncodedFrame>,
    idr_tx: tokio::sync::watch::Sender<bool>,
}

impl QuicServer {
    /// Start the QUIC server bound to `listen_addr`.
    ///
    /// Returns immediately; all async tasks run in the background on the
    /// current Tokio runtime.
    ///
    /// Use [`frame_sink`] to obtain a channel for delivering encoded frames.
    pub async fn new(listen_addr: SocketAddr, idr_tx: tokio::sync::watch::Sender<bool>) -> Self {
        let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(32);

        // broadcast capacity = 64 frames  (~1 second of 60 fps with headroom).
        // watch is not used here because we need `is_key` alongside the data.
        let (bcast_tx, _) = broadcast::channel::<Arc<SerializedFrame>>(64);
        let server_bcast  = bcast_tx.clone();

        // ── Serialiser task ──────────────────────────────────────────────────
        // Converts raw NAL slices into `VideoPacket`, serialises with postcard
        // once, and broadcasts the resulting `Arc<Bytes>` to all client tasks.

        tokio::spawn(async move {
            run_serialiser_task(frame_rx, server_bcast).await;
            // let mut frame_id = 0u64;

            // while let Some(mut frame ) = frame_rx.recv().await {
            //     frame_id += 1;
            //     frame.frame_id = frame_id;

            //     if let Some(t) = frame.trace.as_mut() {
            //         t.serialize_us = FrameTrace::now_us(); 
            //     }
            //     let shared_frame = Arc::new(frame);

            //     // log::info!(
            //     //     "[SEND RAW] id={} size={} key={}",
            //     //     packet.frame_id,
            //     //     packet.payload.len(),
            //     //     packet.is_key
            //     // );

            //     if let Err(_) = server_bcast.send(shared_frame) {
            //         // Если нет активных подписчиков, broadcast вернет ошибку, это нормально
            //     }
            // }
        });

        // ── Accept loop ──────────────────────────────────────────────────────
        let idr_tx_clone = idr_tx.clone();
        let endpoint = build_server_endpoint(listen_addr);
        tokio::spawn(async move {
            while let Some(connecting) = endpoint.accept().await {
                let client_rx = bcast_tx.subscribe();
                let idr_tx_init = idr_tx_clone.clone();
                tokio::spawn(async move {
                    match connecting.await {
                        Ok(conn) => {
                            log::info!("[QUIC] Client connected: {}", conn.remote_address());
                            let (identity_tx, identity_rx) = watch::channel(ClientIdentity::default());
                            let identity_tx_for_accept = identity_tx.clone();

                            let idr_tx_conn = idr_tx_init.clone();
                            let ird_tx_loop = idr_tx_init.clone();
                            let info = Arc::new(ConnectionInfo {
                                remote: conn.remote_address().to_string(),
                                label: RwLock::new(conn.remote_address().to_string()),
                                ready: AtomicBool::new(false),
                            });
                            let info_bi = info.clone();
                            let info_uni = info.clone();
                            let info_main   = info.clone();
                            let clock_offset = Arc::new(AtomicI64::new(0));
                            let clock_off_main = clock_offset.clone();

                            //BI STREAM ACCEPT
                            let conn_bi = conn.clone();
                            let conn_uni = conn.clone();
                            tokio::spawn(async move {
                                loop {
                                    match conn_bi.accept_bi().await {
                                        Ok((mut send, mut recv)) => {
                                            let identity_tx = identity_tx_for_accept.clone();
                                            let info_for_task = info_bi.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) = handle_bi_stream(&mut send, &mut recv, identity_tx, info_for_task).await {
                                                    log::warn!("bi stream error: {e}");
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            log::info!("accept_bi ended: {e}");
                                            break;
                                        }
                                    }
                                }
                            });

                            tokio::spawn(async move {
                                loop {
                                    match conn_uni.accept_uni().await {
                                        Ok(recv) => {
                                            let idr_tx_uni = idr_tx_conn.clone();
                                            let info_for_task = info_uni.clone();
                                            let clock_offset_task = clock_offset.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) = handle_uni_stream(recv, info_for_task, clock_offset_task, idr_tx_uni).await {
                                                    log::warn!("bi stream error: {e}");
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            log::info!("accept_bi ended: {e}");
                                            break;
                                        }
                                    }
                                }
                            });
                            send_loop_to_client(conn, client_rx, info_main, &clock_off_main, ird_tx_loop).await;
                        }
                        Err(e) => log::warn!("[QUIC] Handshake failed: {e}"),
                    }
                });
            }
        });

        Self { frame_tx, idr_tx }
    }

    /// Return a cloneable sender for delivering encoded `(nal_bytes, is_key)` frames.
    ///
    /// Pass this to [`VideoSender::run`] to connect the capture/encode pipeline
    /// to the transport layer.
    pub fn frame_sink(&self) -> mpsc::Sender<EncodedFrame> {
        self.frame_tx.clone()
    }

    /// Convenience method — push one encoded frame directly.
    ///
    /// Drops silently if the internal channel is full (back-pressure for live
    /// video; we never want to stall the encoder waiting for the network).
    pub fn send(&self, frame: EncodedFrame) {
        match self.frame_tx.try_send(frame) {
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
async fn run_serialiser_task(
    mut frame_rx: mpsc::Receiver<EncodedFrame>,
    broadcast_tx: broadcast::Sender<Arc<SerializedFrame>>,
) {
    let mut frame_id = 0u64;
    
    // Calculate a safe max chunk size for the broadcast.
    // Quinn's initial_mtu is 1200, so we use that as our baseline constraint.
    let max_dgram = 1200;
    let max_chunk_data = max_dgram - DatagramChunk::HEADER_LEN - 8;

    while let Some(mut frame) = frame_rx.recv().await {
        frame_id += 1;
        frame.frame_id = frame_id;

        if let Some(t) = frame.trace.as_mut() {
            t.serialize_us = FrameTrace::now_us(); 
        }

        let total_slices = frame.slices.len() as u8;
        if total_slices == 0 {
            continue;
        }

        let mut datagrams = Vec::new();

        for (s_idx, (slice_data, is_critical)) in frame.slices.iter().enumerate() {
            // 1. Calculate flags
            let mut flags = if frame.is_key { 1 } else { 0 };
            if *is_critical { flags |= 2; }

            // 2. Wrap payload
            let slice = VideoSlice {
                frame_id: frame.frame_id,
                slice_idx: s_idx as u8,
                total_slices,
                is_key: frame.is_key,
                payload: slice_data.to_vec(),
                trace: if s_idx == 0 { frame.trace.clone() } else { None },
            };

            // 3. Serialize using postcard
            let serialized = postcard::to_allocvec(&slice).unwrap_or_default();

            // 4. FEC Encoding
            let chunks = common::fec::FecEncoder::encode_slice(
                frame.frame_id,
                s_idx as u8,
                total_slices,
                &serialized, 
                max_chunk_data,
                flags
            );

            // 5. Convert to Bytes instantly
            for chunk in chunks {
                datagrams.push(chunk.to_bytes());
            }
        }

        // 6. Broadcast the pre-computed datagrams
        let shared_frame = Arc::new(SerializedFrame {
            frame_id: frame.frame_id,
            is_key: frame.is_key,
            datagrams,
        });

        if let Err(_) = broadcast_tx.send(shared_frame) {
            // No active subscribers, safely ignore.
        }
    }
}

async fn send_loop_to_client(
    conn: quinn::Connection,
    mut video_rx: broadcast::Receiver<Arc<SerializedFrame>>,
    info: Arc<ConnectionInfo>,
    clock_offset: &AtomicI64,
    idr_tx: watch::Sender<bool>,
) {
    let mut started = false;
    let mut requested_initial_idr = false;
    let remote = conn.remote_address();
    
    // Заранее считаем лимиты
    let max_dgram = conn.max_datagram_size().unwrap_or(1200);
    let max_chunk_data = max_dgram.saturating_sub(DatagramChunk::HEADER_LEN + 8);

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
                            let wait = pacer.consume(DatagramChunk::HEADER_LEN + dgram.len());
                            if !wait.is_zero() {
                                tokio::time::sleep(wait).await;
                            }

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

async fn handle_bi_stream(
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

async fn handle_uni_stream(
    mut recv: quinn::RecvStream,
    info: Arc<ConnectionInfo>,
    clock_offset: Arc<AtomicI64>,
    idr_tx: watch::Sender<bool>,
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
        _ => {}
        }
    }
    Ok(())
}

fn client_to_server_us(t: u64, clock_offset: &AtomicI64) -> u64 {
    let off = clock_offset.load(Ordering::Relaxed);
    if off >= 0 {
        t.saturating_add(off as u64)
    } else {
        t.saturating_sub((-off) as u64)
    }
}

async fn handle_control(conn: &quinn::Connection, data: Bytes, info: &ConnectionInfo, clock_offset: &AtomicI64, idr_tx: watch::Sender<bool>,) {
    if let Ok(packet) = postcard::from_bytes::<ControlPacket>(&data) {
        match packet {
            ControlPacket::Ping { client_time_us } => {
                let pong = ControlPacket::Pong {
                    client_time_us,
                    server_time_us: FrameTrace::now_us(),
                };
                if let Ok(bin) = postcard::to_stdvec(&pong) {
                    // Отправляем ответ как TYPE_CONTROL
                    let dgram = DatagramChunk::encode(
                        0,                  // frame_id (для контроля можно 0)
                        0,                  // slice_idx
                        1,                  // total_slices
                        0,                  // shard_idx
                        1,                  // k
                        0,                  // m
                        bin.len() as u16,   // payload_len
                        TYPE_CONTROL,       // packet_type
                        0,                  // flags
                        &bin                // data
                    );
                    let _ = conn.send_datagram(dgram);
                }
            },
            ControlPacket::Pong{..} => (),
            ControlPacket::OffsetUpdate { offset_us, rtt_us } => {
                clock_offset.store(offset_us, Ordering::Relaxed);
                log::info!("[{}] RTT: {:.1}ms", info.label().await, rtt_us as f64 / 1000.0);
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
                let _ = idr_tx.send(true);
                let _ = idr_tx.send(false);
            }
            _ => ()
        }
    }
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

    t.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default()));
    // Datagram buffers — 16 MB handles burst of ~80 uncompressed HEVC frames
    // before the kernel starts dropping.
    t.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    t.datagram_send_buffer_size(8 * 1024 * 1024);
    t.send_fairness(true);
    // Initial MTU probe value for Ethernet LAN.
    //
    // Path MTU = 1500 (Ethernet) - 20 (IP) - 8 (UDP) - ~20 (QUIC) = ~1452.
    // We probe at 1400 to stay safe across VPN tunnels and 802.11 frames.
    // Quinn will discover the actual maximum via PMTUD automatically.
    t.initial_mtu(1200);

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

    // FIX SOCKET CONGESTION
    let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .expect("Failed to create socket");
    socket.set_send_buffer_size(16 * 1024 * 1024).ok(); 
    socket.set_recv_buffer_size(2 * 1024 * 1024).ok();
    
    socket.set_reuse_address(true).ok();
    socket.bind(&addr.into()).expect("Failed to bind server socket");

    let std_socket: std::net::UdpSocket = socket.into();

    server_cfg.transport_config(Arc::new(t));

    Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_cfg),
        std_socket,
        Arc::new(quinn::TokioRuntime),
    ).expect("QUIC endpoint bind failed")
}
