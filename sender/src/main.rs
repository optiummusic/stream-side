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

use std::{env, net::SocketAddr, sync::{Arc, Mutex, mpsc}, time::Duration};
use common::AudioFrame;
use sender::{
    Senders, Watchers, capture::{AudioSender, LinuxAudioSender, LinuxSender, VideoSender}, network::{CongestionConfig, CongestionController, QuicServer}
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
    let (fps_tx, fps_rx) = tokio::sync::watch::channel(Some(120));

    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<AudioFrame>(64);
    let (audio_bcast_tx, _) = tokio::sync::broadcast::channel::<Arc<AudioFrame>>(64);

    let bcast_tx_clone = audio_bcast_tx.clone();
    tokio::spawn(async move {
        while let Some(frame) = audio_rx.recv().await {
            let _ = bcast_tx_clone.send(Arc::new(frame));
        }
    });

    let watchers = Watchers {
        idr_rx,         // Вместо idr_rx: idr_rx
        bitrate_rx, 
        capture_fps_rx: fps_rx,
    };

    let senders = Senders {
        idr_tx,
        bitrate_tx,
        capture_fps_tx: fps_tx,
        audio_bcast_tx,
    };

    let config = CongestionConfig {
        min_bitrate: 2_000_000,          // 0.1 Мбит (минимум, чтобы не упал канал)
        max_bitrate: 10_000_000,       // 50 Мбит
        rtt_threshold_ms: 60.0,        // Порог задержки
        step_up: 150_000,              // +0.5 Мбит при хорошей связи
        backoff_congested: 0.8,        // -20% битрейта при высоком RTT
        backoff_lossy: 0.7,            // -50% битрейта при потерях (агрессивно!)
        update_interval: Duration::from_millis(500),    // Частота обычных проверок
        recovery_duration: Duration::from_millis(100), // Сколько игнорим RTT после потерь
        base_lock_duration: Duration::from_secs(4),     // Начальный блок повышения
        min_fps: 20,
        max_fps: 140,
        fps_step_down: 2,
        fps_step_up: 10,
    };

    let shared_controller = Arc::new(Mutex::new(CongestionController::new(5_000_000, config)));

    let server = Arc::new(QuicServer::new(listen_addr,shared_controller, senders).await);
    let sink   = server.frame_sink();
    // ── Capture + encode ─────────────────────────────────────────────────────
    //
    // LinuxSender автоматически выбирает бэкенд:
    //   • wlroots-compositor  → WlrootsSender  (zwlr_screencopy, низкая latency)
    //   • иначе               → LinuxPipeWireSender (XDG Portal + PipeWire)

    let video_sender = LinuxSender::new(1, 1, watchers);
    let audio_sender = LinuxAudioSender::new();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received Ctrl-C, shutting down");
        }
        res = video_sender.run(sink) => {
        if let Err(e) = res { log::error!("Video Sender error: {e}"); }
        }
        // Запуск аудио-потока (передаем audio_mpsc_tx для AudioFrame)
        res = audio_sender.run(audio_tx) => {
            if let Err(e) = res { log::error!("Audio Sender error: {e}"); }
        }
    }
}