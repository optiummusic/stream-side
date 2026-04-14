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
//! Receivers (desktop / Android) connect to `<machine-ip>:<port>`.

use std::{env, net::SocketAddr, sync::{Arc, atomic::AtomicU64}};
use eframe::egui;
use sender::{
    capture::{VideoSender, linux::LinuxPipeWireSender},
    quic::QuicServer, ui::{self, UserEvent},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Install the rustls crypto provider exactly once.
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
    let server = Arc::new(QuicServer::new(listen_addr, idr_tx).await);
    let sink   = server.frame_sink();

    // ── Capture + encode ─────────────────────────────────────────────────────
    //
    // `LinuxPipeWireSender` is the concrete VideoSender implementation for Linux.
    // Swap it for `WindowsSender` or `AndroidSender` on other platforms without
    // touching any other code.
    let shared_bitrate = Arc::new(AtomicU64::new(5_000_000));
    let bitrate_for_ui = Arc::clone(&shared_bitrate);
    let sender = LinuxPipeWireSender::new(1, 1, idr_rx, Arc::clone(&shared_bitrate));
    tokio::spawn(async move {
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
    });
    // Spawn UI
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 200.0])
            .with_title("Sender Control Panel"),
        ..Default::default()
    };

    log::info!("Starting UI...");
    
    eframe::run_native(
        "sender_ui",
        native_options,
        Box::new(|_cc| Ok(Box::new(ui::App::new(bitrate_for_ui)))),
    ).map_err(|e| format!("eframe error: {e}"))?;

    Ok(())
}
