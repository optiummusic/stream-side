//! Sender entry point (Linux).
//!
//! # Usage
//!
//! ```sh
//! # Listen on all interfaces, default port
//! cargo run --bin sender
//!
//! # Custom address
//! cargo run --bin sender -- 0.0.0.0:9999
//! ```
//!
//! На wlroots-композиторах (Sway, Hyprland) автоматически используется
//! `zwlr_screencopy_manager_v1`. На остальных — XDG Portal + PipeWire.

use std::{env, net::SocketAddr, sync::{Arc, Mutex}, time::Duration};
use sender::{
    capture::{LinuxSender, VideoSender},
    network::{CongestionConfig, CongestionController, QuicServer},
};

#[tokio::main]
async fn main() {
    env_logger::init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // ── Address ──────────────────────────────────────────────────────────────

    let listen_addr: SocketAddr = env::args()
        .nth(1)
        .as_deref()
        .unwrap_or("0.0.0.0:4433")
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("Invalid address: {e}. Falling back to 0.0.0.0:4433");
            "0.0.0.0:4433".parse().unwrap()
        });

    log::info!("Starting QUIC server on {listen_addr}");
    log::info!("Receivers must connect to <your-ip>:{}", listen_addr.port());

    // ── Transport ────────────────────────────────────────────────────────────
    let (idr_tx, idr_rx) = tokio::sync::watch::channel(false);
    let (bitrate_tx, bitrate_rx) = tokio::sync::watch::channel(10_000_000u64);

    let config = CongestionConfig {
        min_bitrate: 500_000,          // 0.1 Мбит (минимум, чтобы не упал канал)
        max_bitrate: 50_000_000,       // 50 Мбит
        rtt_threshold_ms: 60.0,        // Порог задержки
        step_up: 500_000,              // +0.5 Мбит при хорошей связи
        backoff_congested: 0.8,        // -20% битрейта при высоком RTT
        backoff_lossy: 0.7,            // -50% битрейта при потерях (агрессивно!)
        update_interval: Duration::from_millis(500),    // Частота обычных проверок
        recovery_duration: Duration::from_millis(3500), // Сколько игнорим RTT после потерь
        base_lock_duration: Duration::from_secs(5),     // Начальный блок повышения
    };

    let shared_controller = Arc::new(Mutex::new(CongestionController::new(10_000_000, config)));

    let server = Arc::new(QuicServer::new(listen_addr, idr_tx, bitrate_tx, shared_controller).await);
    let sink   = server.frame_sink();
    // ── Capture + encode ─────────────────────────────────────────────────────
    //
    // LinuxSender автоматически выбирает бэкенд:
    //   • wlroots-compositor  → WlrootsSender  (zwlr_screencopy, низкая latency)
    //   • иначе               → LinuxPipeWireSender (XDG Portal + PipeWire)

    let sender = LinuxSender::new(1, 1, idr_rx, bitrate_rx);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received Ctrl-C, shutting down");
        }
        res = sender.run(sink) => {
            match res {
                Ok(())   => log::info!("Sender finished cleanly"),
                Err(e)   => log::error!("Sender error: {e}"),
            }
        }
    }
}