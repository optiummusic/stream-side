use quinn::{ClientConfig, Endpoint};
use std::sync::Arc;
use tokio::sync::mpsc;
use std::net::SocketAddr;
use rustls::DigitallySignedStruct;
use quinn::crypto::rustls::QuicClientConfig;
// --- Настройка TLS: Пропускаем проверку сертификатов ---
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &DigitallySignedStruct, // ИСПРАВЛЕНО: путь и лайфтайм
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &DigitallySignedStruct, // ИСПРАВЛЕНО: путь и лайфтайм
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

pub struct QuicSender {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl QuicSender {
    pub async fn new(server_addr: SocketAddr) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // 1. Конфиг клиента для Rustls 0.23+
        // Нужно явно использовать CryptoProvider (ring)
        let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("Error setting TLS versions")
            .dangerous() // ИСПРАВЛЕНО: явный переход в "опасный" режим
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        
        crypto.alpn_protocols = vec![b"video-stream".to_vec()];

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
        let quic_crypto = QuicClientConfig::try_from(crypto).unwrap();
        endpoint.set_default_client_config(ClientConfig::new(Arc::new(quic_crypto)));

        // 2. Подключение
        println!("🚀 QUIC: Подключаемся к {}...", server_addr);
        let connection = endpoint.connect(server_addr, "localhost").unwrap().await.expect("Failed to connect");
        println!("✅ QUIC: Соединение установлено!");

        // 3. Открываем один длинный однонаправленный поток
        let mut send_stream = connection.open_uni().await.unwrap();

        // 4. Воркер отправки
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                let len = (data.len() as u32).to_le_bytes();
                if let Err(e) = send_stream.write_all(&len).await {
                    eprintln!("❌ QUIC Write Len Error: {}", e);
                    break;
                }
                if let Err(e) = send_stream.write_all(&data).await {
                    eprintln!("❌ QUIC Write Data Error: {}", e);
                    break;
                }
            }
            let _ = send_stream.finish(); 
        });

        Self { tx }
    }

    pub fn send(&self, data: Vec<u8>) {
        let _ = self.tx.send(data);
    }
}