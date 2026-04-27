// ─────────────────────────────────────────────────────────────────────────────
// Endpoint construction
// ─────────────────────────────────────────────────────────────────────────────
use super::*;

use quinn::congestion::{Controller, ControllerFactory};
use std::sync::Arc;
use std::time::Instant;

/// No-op congestion controller — окно всегда максимальное, pacing отключён.
/// Подходит для LAN где у нас свой CC на уровне битрейта энкодера.
#[derive(Debug)]
struct NoopController {
    window: u64,
}

impl Controller for NoopController {
    fn on_congestion_event(
        &mut self, _now: Instant, _sent: Instant, _is_persistent_congestion: bool, _lost_bytes: u64,
    ) {}

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(NoopController { window: self.window })
    }

    fn initial_window(&self) -> u64 {
        self.window
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[derive(Debug)]
struct NoopControllerFactory;

impl ControllerFactory for NoopControllerFactory {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(NoopController {
            window: 32 * 1024 * 1024, // 32MB — фиксированное окно
        })
    }
}

pub(crate) fn build_server_endpoint(addr: SocketAddr) -> Endpoint {
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
        // Для LAN стриминга — большое фиксированное окно, без агрессивного CC
    let mut cfg = quinn::congestion::CubicConfig::default();
    cfg.initial_window(1 * 1024 * 1024);      // 10MB начальное окно
    t.congestion_controller_factory(Arc::new(NoopControllerFactory));


    // Datagram buffers — 16 MB handles burst of ~80 uncompressed HEVC frames
    // before the kernel starts dropping.
    t.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    t.datagram_send_buffer_size(16 * 1024 * 1024);
    t.send_fairness(false);
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