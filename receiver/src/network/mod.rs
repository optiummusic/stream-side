mod endpoint;
mod connection;
mod identity;
mod tasks;
mod utils;

pub(crate) use std::{
    error::Error,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
pub(crate) use std::sync::atomic::{Ordering};
pub(crate) use socket2::{Domain, Protocol, Socket, Type};
pub(crate) use tokio::{sync::watch, task::JoinHandle, time::{self, Instant}};
pub(crate) use tokio::sync::mpsc;
pub(crate) use common::{CLOCK_OFFSET, ControlPacket, DatagramChunk, FrameTrace, TYPE_AUDIO, TYPE_CONTROL, TYPE_VIDEO, VideoPacket};
pub(crate) use quinn::{Endpoint, SendStream};
pub(crate) use quinn::crypto::rustls::QuicClientConfig;
pub(crate) use rustls::pki_types::{CertificateDer, ServerName};
pub(crate) use rustls::DigitallySignedStruct;
pub(crate) use crate::{JITTER_TARGET_MS, JitterBuffer, backend::{FrameOutput, PushStatus, VideoBackend}, platform::AppProxy, types::DecodedFrame};

pub(crate) use endpoint::*;
pub(crate) use utils::*;
pub(crate) use identity::*;
pub(crate) use connection::*;
pub(crate) use tasks::*;

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
        let (idr_needed_tx, idr_needed_rx) = watch::channel(false);
        task_handles.push(spawn_idr_solicitor(
            control_tx.clone(), 
            idr_needed_rx
        ));
        receive_datagrams(conn.clone(), backend.clone(), control_tx.clone(), idr_needed_tx).await;

        // Останавливаем все фоновые задачи этого соединения.
        for handle in task_handles {
            handle.abort();
        }

        log::warn!("[QUIC] Connection lost, reconnecting in 2 s...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}