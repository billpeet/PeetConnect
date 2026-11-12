//! TLS configuration. Trust comes from Tailscale, not from these certificates.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::{Result, TransportError, SERVER_NAME};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Server side: a fresh self-signed certificate for this process lifetime.
pub(crate) fn server_config() -> Result<quinn::crypto::rustls::QuicServerConfig> {
    let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    let cert: CertificateDer<'static> = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| TransportError::Tls(e.to_string()))?;
    config.alpn_protocols = vec![peet_proto::ALPN.to_vec()];
    quinn::crypto::rustls::QuicServerConfig::try_from(config)
        .map_err(|e| TransportError::Tls(e.to_string()))
}

/// Client side: accept any server certificate. The peer address is a Tailscale address, so
/// the WireGuard layer has already authenticated the machine we are talking to.
pub(crate) fn client_config() -> Result<quinn::crypto::rustls::QuicClientConfig> {
    let provider = provider();
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAny { provider }))
        .with_no_client_auth();
    config.alpn_protocols = vec![peet_proto::ALPN.to_vec()];
    quinn::crypto::rustls::QuicClientConfig::try_from(config)
        .map_err(|e| TransportError::Tls(e.to_string()))
}

#[derive(Debug)]
struct TrustAny {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for TrustAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
