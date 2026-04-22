use super::*;

// ─────────────────────────────────────────────────────────────────────────────
// Client endpoint construction
// ─────────────────────────────────────────────────────────────────────────────
pub(crate) fn build_quic_client_endpoint() -> Result<Endpoint, Box<dyn Error>> {
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

    let mut endpoint_config = quinn::EndpointConfig::default();

    let mut endpoint = Endpoint::new(
        endpoint_config,
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