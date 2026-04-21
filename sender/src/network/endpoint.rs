// ─────────────────────────────────────────────────────────────────────────────
// Endpoint construction
// ─────────────────────────────────────────────────────────────────────────────
use super::*;

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
    t.initial_mtu(9000);

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