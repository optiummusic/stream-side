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
use std::sync::atomic::{AtomicI64, Ordering};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{sync::watch, task::JoinHandle, time::{self, Instant}};
use tokio::sync::mpsc;
use bytes::Bytes;
use common::{CLOCK_OFFSET, ControlPacket, DatagramChunk, FrameTrace, TYPE_AUDIO, TYPE_CONTROL, TYPE_VIDEO, VideoPacket, fec::FrameAssembler};
use quinn::{Endpoint, SendStream};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::DigitallySignedStruct;

use crate::{JITTER_TARGET_MS, JitterBuffer, JitterEntry, backend::{FrameOutput, PushStatus, VideoBackend}, platform::AppProxy, types::DecodedFrame};
// ─────────────────────────────────────────────────────────────────────────────
// Datagram reassembly buffer
// ─────────────────────────────────────────────────────────────────────────────
// Коэффициент сглаживания: 0.1 значит, что новый замер влияет на 10%, 
// а старое значение сохраняется на 90%.
const OFFSET_ALPHA: f64 = 0.01;
struct VideoState {
    waiting_for_key: bool,
    expected_frame_id: Option<u64>,
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

pub async fn run_quic_receiver<B: VideoBackend + Send + 'static>(
    backend: Arc<Mutex<B>>,
    sender_addr: SocketAddr,
    frame_tx: Option<mpsc::Sender<DecodedFrame>>,
    trace_rx: watch::Receiver<Option<(u64, FrameTrace)>>,
    proxy: AppProxy
) -> Result<(), Box<dyn Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();

    // Создаём endpoint один раз и переиспользуем его для реконнектов.
    let endpoint = build_quic_client_endpoint()?;

    loop {
        log::info!("[QUIC] Connecting to {sender_addr}...");

        let conn = match connect_quic(&endpoint, sender_addr).await {
            Ok(conn) => conn,
            Err(e) => {
                log::warn!("[QUIC] connect failed: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        log::info!("[QUIC] Connected to {}", conn.remote_address());

        if let Err(e) = send_identity_and_wait_ack(&conn).await {
            log::warn!("[QUIC] handshake failed: {e}");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let (control_tx, control_rx) = mpsc::channel::<ControlPacket>(100);
        let proxy_clone = proxy.clone();
        let mut task_handles = Vec::<JoinHandle<()>>::new();
        let proxy_clone = proxy.clone();

        task_handles.push(spawn_decoder_poll_task(
            backend.clone(),
            frame_tx.clone(),
            control_tx.clone(),
            proxy_clone
        ));

        task_handles.push(spawn_control_writer_task(
            conn.clone(),
            control_rx,
        ));

        task_handles.push(spawn_trace_feedback_task(
            trace_rx.clone(),
            control_tx.clone(),
        ));

        task_handles.push(spawn_ping_task(conn.clone()));

        // Основной цикл приёма датаграмм.
        // Когда он завершается — считаем соединение потерянным.
        let (idr_needed_tx, _idr_needed_rx) = watch::channel(false);
        receive_datagrams(conn.clone(), backend.clone(), control_tx.clone(), idr_needed_tx).await;

        // Останавливаем все фоновые задачи этого соединения.
        for handle in task_handles {
            handle.abort();
        }

        log::warn!("[QUIC] Connection lost, reconnecting in 2 s...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn connect_quic(
    endpoint: &quinn::Endpoint,
    sender_addr: SocketAddr,
) -> Result<quinn::Connection, Box<dyn Error>> {
    let connecting = endpoint.connect(sender_addr, "localhost")?;
    let conn = connecting.await?;
    Ok(conn)
}

async fn send_identity_and_wait_ack(conn: &quinn::Connection) -> Result<(), Box<dyn Error>> {
    let (send, mut recv) = conn.open_bi().await?;
    send_identity(send).await;

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;

    let len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;

    match postcard::from_bytes::<ControlPacket>(&data) {
        Ok(ControlPacket::StartStreaming) => {
            log::info!("[QUIC] Server ACK → start streaming");
            Ok(())
        }
        Ok(other) => {
            Err(format!("unexpected ACK packet: {:?}", other).into())
        }
        Err(e) => {
            Err(format!("ACK decode error: {e}").into())
        }
    }
}

fn spawn_control_writer_task(
    conn: quinn::Connection,
    mut control_rx: mpsc::Receiver<ControlPacket>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(packet) = control_rx.recv().await {
            if let Ok(bytes) = postcard::to_stdvec(&packet) {
                // Шлем как датаграмму, чтобы не зависеть от Window Updates
                let dgram = DatagramChunk::encode(
                    0, 0, 1, 0, 1, 0, 
                    bytes.len() as u16, 
                    TYPE_CONTROL, 
                    0, 
                    &bytes
                );
                
                if let Err(e) = conn.send_datagram(dgram) {
                    log::warn!("[QUIC] Failed to send control dgram: {e}");
                    // Не выходим из цикла! Просто идем дальше.
                }
            }
        }
    })
}

fn spawn_trace_feedback_task(
    trace_rx: watch::Receiver<Option<(u64, FrameTrace)>>,
    control_tx: mpsc::Sender<ControlPacket>,
) -> JoinHandle<()> {
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

fn spawn_ping_task(conn: quinn::Connection) -> JoinHandle<()> {
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

fn spawn_decoder_poll_task<B: VideoBackend + Send + 'static>(
    backend: Arc<Mutex<B>>,
    frame_tx: Option<mpsc::Sender<DecodedFrame>>,
    control_tx: mpsc::Sender<ControlPacket>,
    proxy: AppProxy,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(3));
        const MAX_DRAIN_PER_TICK: usize = 32;

        loop {
            interval.tick().await;

            let backend = backend.clone();
            let poll_result = tokio::task::spawn_blocking(move || {
                let mut frames_to_send = Vec::new();
                let mut dropped_ages = Vec::new();

                let mut guard = match backend.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        return Err("backend mutex poisoned".to_string());
                    }
                };

                let mut drained = 0usize;

                loop {
                    if drained >= MAX_DRAIN_PER_TICK {
                        break;
                    }

                    match guard.poll_output() {
                        Ok(FrameOutput::Pending) => break,

                        Ok(FrameOutput::Dropped { age }) => {
                            dropped_ages.push(age);
                        }

                        Ok(frame) => {
                            frames_to_send.push(frame);
                            drained += 1;
                        }

                        Err(e) => {
                            return Err(format!("poll_output error: {e:?}"));
                        }
                    }
                }

                Ok::<_, String>((frames_to_send, dropped_ages))
            })
            .await;

            let (frames_to_send, dropped_ages) = match poll_result {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    log::error!("[Decoder] poll task error: {e}");
                    break;
                }
                Err(e) => {
                    log::error!("[Decoder] poll task join error: {e}");
                    break;
                }
            };

            for age in dropped_ages {
                let _ = control_tx.try_send(ControlPacket::Communication {
                    message: format!(
                        "DROPPED ON POLL, age: {:.1}ms",
                        age.unwrap_or(0.0)
                    ),
                });
            }

            if let Some(tx) = &frame_tx {
                for frame in frames_to_send {
                    let res = match frame {
                        FrameOutput::Yuv(f) => tx.try_send(DecodedFrame::Yuv(f)),

                        #[cfg(unix)]
                        FrameOutput::DmaBuf(f) => tx.try_send(DecodedFrame::DmaBuf(f)),

                        _ => Ok(()),
                    };
                    
                    #[cfg(not(target_os = "android"))]
                    if res.is_ok() {
                        if let Some(p) = &proxy {
                            let _ = p.send_event(crate::UserEvent::NewFrame);
                        }
                    }

                    if res.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

async fn send_identity(mut send: SendStream) {
    let identify = make_client_identity();
    let bytes = postcard::to_stdvec(&identify).unwrap();
    if let Err(e) = send.write_all(&(bytes.len() as u32).to_le_bytes()).await {
        log::error!("write len failed: {e}");
        return;
    }

    if let Err(e) = send.write_all(&bytes).await {
        log::error!("write body failed: {e}");
        return;
    }
    let _ = send.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-connection datagram loop
// ─────────────────────────────────────────────────────────────────────────────

pub async fn send_offset_update(conn: &quinn::Connection, rtt_us: u64) -> Result<(), Box<dyn std::error::Error>> {
    let offset = CLOCK_OFFSET.load(Ordering::Relaxed);
    let packet = ControlPacket::OffsetUpdate { offset_us: offset, rtt_us };

    let bin = postcard::to_stdvec(&packet)?;
    let dgram = DatagramChunk::encode(0, 0, 1, 0, 1, 0, bin.len() as u16, TYPE_CONTROL, 0, &bin);
    conn.send_datagram(dgram)?;

    Ok(())
}

async fn receive_datagrams<B: VideoBackend>(
    conn:     quinn::Connection,
    backend:  Arc<Mutex<B>>,
    control_tx: mpsc::Sender<ControlPacket>,
    idr_needed_tx: watch::Sender<bool>,
) {
    let idr_needed_rx = idr_needed_tx.subscribe();
    let mut video_state = VideoState {
        waiting_for_key: true,
        expected_frame_id: None,
    };
    let mut jitter_buf = JitterBuffer::new(JITTER_TARGET_MS);
    let mut assembler = FrameAssembler::new();
    
    let sleep_until = Instant::now() + Duration::from_secs(3600);
    let sleep = time::sleep_until(sleep_until);
    tokio::pin!(sleep);
    loop {
        // ── Sleep duration until the next buffered frame is due ───────────────
        // If the buffer is empty we park the timer for 1 hour; it will be
        // cancelled the moment the datagram arm fires.
        let next_deadline = match jitter_buf.time_to_next() {
            Some(d) => Instant::now() + d,
            None    => Instant::now() + Duration::from_secs(3600),
        };
        sleep.as_mut().reset(next_deadline);

        
        tokio::select! {
            // ── 1. Incoming datagram ─────────────────────────────────────────
                        // ── 2. Jitter-buffer drain timer ─────────────────────────────────
            // Fires when the earliest buffered frame has waited long enough.
            // All frames whose deadline has now passed are released at once.
            _ = &mut sleep => {
                let ready = jitter_buf.drain_ready();
                if ready.is_empty() { continue; }
 
                log::trace!("[JitterBuf] releasing {} frame(s)", ready.len());
 
                for packet in ready {
                    push_frame_to_backend(
                        packet,
                        &mut video_state,
                        &backend,
                        &control_tx,
                        &idr_needed_tx,
                        &idr_needed_rx,
                    ).await;
                }
            }

            raw = conn.read_datagram() => {
                let raw = match raw {
                    Ok(b)  => b,
                    Err(e) => { log::error!("QUIC READ ERROR: {:?}", e); break; }
                };
 
                log::trace!("Got packet: {} bytes", raw.len());
 
                let chunk = match DatagramChunk::decode(raw) {
                    Some(c) => c,
                    None    => { log::warn!("Failed to decode chunk header"); continue; }
                };
 
                match chunk.packet_type {
                    TYPE_VIDEO => {
                        // Reassemble; if a frame completed, enqueue it in the
                        // jitter buffer instead of pushing to the backend directly.
                        if let Some(packet) = assembler.insert(&chunk) {
                            jitter_buf.push(packet);
                        }
                    }
                    TYPE_AUDIO => {
                        // Audio bypasses the jitter buffer (separate sync path).
                        // handle_audio_frame(chunk.data);
                    }
                    TYPE_CONTROL => {
                        if let Ok(ctrl) = postcard::from_bytes::<ControlPacket>(&chunk.data) {
                            if let Some((_, rtt_us)) = process_control_feedback(ctrl) {
                                let conn_clone = conn.clone();
                                tokio::spawn(async move {
                                    let _ = send_offset_update(&conn_clone, rtt_us).await;
                                });
                            }
                        }
                    }
                    _ => log::warn!("Unknown packet type"),
                }
            }
        }
        
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend push — called from the jitter-buffer drain path
// ─────────────────────────────────────────────────────────────────────────────
 
/// Submit one decoded packet to the backend decoder.
///
/// The in-order / IDR-wait checks that were previously inside
/// `handle_video_chunk` are now applied here, *after* the jitter buffer has
/// had a chance to reorder out-of-order arrivals.
async fn push_frame_to_backend<B: VideoBackend + Send + 'static>(
    mut packet: VideoPacket,
    state:      &mut VideoState,
    backend:    &Arc<Mutex<B>>,
    control_tx: &mpsc::Sender<ControlPacket>,
    idr_needed_tx: &watch::Sender<bool>, // To set to false on I-frame
    idr_needed_rx: &watch::Receiver<bool>,
) {
    // ── In-order / IDR gate ───────────────────────────────────────────────
    if packet.is_key {
        state.waiting_for_key   = false;
        state.expected_frame_id = Some(packet.frame_id);
        let _ = idr_needed_tx.send(false);
    } else if state.waiting_for_key || Some(packet.frame_id) != state.expected_frame_id {
        if state.waiting_for_key {
            return; 
        }
        state.waiting_for_key = true;
        let _ = idr_needed_tx.send(true);
        log::warn!("Frame lost! Expected {:?}, got {}. Spawning IDR solicitor.", state.expected_frame_id, packet.frame_id);

        let tx = control_tx.clone();
        let mut rx = idr_needed_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            // Loop as long as the watch value is 'true'
            while *rx.borrow() {
                if tx.send(ControlPacket::RequestKeyFrame).await.is_err() {
                    break;
                }

                tokio::select! {
                    _ = interval.tick() => {},
                    // Wake up immediately if the value changes to false
                    _ = rx.changed() => {
                        if !*rx.borrow() { break; }
                    }
                }
            }
            log::info!("[Video] IDR solicitor task satisfied and exiting.");
        });
        return;
    }
 
    state.expected_frame_id = Some(packet.frame_id + 1);
 
    let frame_id    = packet.frame_id;
    let payload     = packet.payload;
    let trace       = packet.trace.take();
    let backend_arc = backend.clone();
    let ctrl        = control_tx.clone();
    
 
    let push_result = tokio::task::spawn_blocking(move || {
        let mut backend_lock = backend_arc.lock().unwrap();
        backend_lock.push_encoded(&payload, frame_id, trace)
    })
    .await;
 
    match push_result {
        Ok(Ok(PushStatus::Dropped { age })) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!(
                    "DROPPED ON PUSH#{}, age: {:.1}ms",
                    frame_id,
                    age.unwrap_or(0.0)
                ),
            });
        }
        Ok(Ok(PushStatus::Accepted)) => {}
        Ok(Err(e)) => {
            let _ = ctrl.try_send(ControlPacket::Communication {
                message: format!("ERROR ON BACKEND: {e}"),
            });
        }
        Err(e) => log::error!("[Video] push task join error: {e}"),
    }
}

fn process_control_feedback(ctrl: ControlPacket) -> Option<(i64, u64)> {
    if let ControlPacket::Pong { client_time_us, server_time_us } = ctrl {
        let t2 = FrameTrace::now_us();
        let rtt = t2.saturating_sub(client_time_us);

        let new_raw_offset = (server_time_us as i64) - (client_time_us + rtt / 2) as i64;
        let current_offset = CLOCK_OFFSET.load(Ordering::Relaxed);

        let filtered_offset = if current_offset == 0 {
            new_raw_offset
        } else {
            (current_offset as f64 + OFFSET_ALPHA * (new_raw_offset - current_offset) as f64) as i64
        };

        CLOCK_OFFSET.store(filtered_offset, Ordering::Relaxed);
        log::debug!("[Sync] RTT: {}ms, Offset: {}us", rtt as f64 / 1000.0, filtered_offset);
        Some((filtered_offset, rtt))
    } else {
        None
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
    t.initial_mtu(1200);

    // Keep-alive: same period as server so both sides detect dead connections
    // within a consistent window.
    t.keep_alive_interval(Some(Duration::from_millis(500)));

    t.max_idle_timeout(Some(
        Duration::from_secs(8)
            .try_into()
            .expect("idle timeout"),
    ));

    client_cfg.transport_config(Arc::new(t));

    // FIX KERNEL UDP SOCKET CONGESTION
    // RUN sudo sysctl -w net.core.rmem_max=16777216 and sudo sysctl -w net.core.rmem_max=16777216
    let addr: SocketAddr = "0.0.0.0:0".parse()?;
    let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_recv_buffer_size(16 * 1024 * 1024)?; // rmem
    socket.set_send_buffer_size(8 * 1024 * 1024)?; // wmem
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();

    let mut endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        std_socket,
        Arc::new(quinn::TokioRuntime), // Если используешь tokio
    )?;
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

fn make_client_identity() -> ControlPacket {
    let os = if cfg!(target_os = "android") {
        "Android"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Unknown"
    }
    .to_string();

    let (model, name) = if cfg!(target_os = "android") {
        android_identity()
    } else if cfg!(target_os = "windows") {
        windows_identity()
    } else if cfg!(target_os = "macos") {
        macos_identity()
    } else if cfg!(target_os = "linux") {
        linux_identity()
    } else {
        unknown_identity()
    };

    // Если ControlPacket пока содержит только model/os,
    // склеиваем model + name в одно поле.
    let model = if name.is_empty() {
        model
    } else {
        format!("{model} ({name})")
    };

    ControlPacket::Identify { model, os }
}

fn android_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .or_else(|_| std::env::var("ANDROID_MODEL"))
        .unwrap_or_else(|_| "Android device".to_string());

    let name = std::env::var("CLIENT_NAME")
        .or_else(|_| std::env::var("ANDROID_DEVICE_NAME"))
        .unwrap_or_else(|_| "Android".to_string());

    (model, name)
}

fn windows_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| format!("{} {}", std::env::consts::ARCH, "Windows"));

    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Windows-PC".to_string());

    (model, name)
}

fn macos_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| "Mac".to_string());

    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Mac".to_string());

    (model, name)
}

fn linux_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| format!("Linux {}", std::env::consts::ARCH));

    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Linux-host".to_string());

    (model, name)
}

fn unknown_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL").unwrap_or_else(|_| "Unknown device".to_string());
    let name = std::env::var("CLIENT_NAME").unwrap_or_else(|_| "Unknown".to_string());
    (model, name)
}