//! TLS 1.3 configuration. Agents use self-signed certificates; the viewer checks
//! the certificate's SHA-256 fingerprint against the one it pinned for that host.

use anyhow::{anyhow, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub const SERVER_NAME: &str = "rdagent";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

pub fn acceptor(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<TlsAcceptor> {
    let key = PrivateKeyDer::try_from(key_der).map_err(|e| anyhow!("bad private key: {e}"))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)], key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub fn connector() -> Result<TlsConnector> {
    let verifier = PinnedByCaller { algorithms: provider().signature_verification_algorithms };
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

pub fn server_name() -> ServerName<'static> {
    ServerName::try_from(SERVER_NAME).expect("constant is a valid DNS name")
}

/// Accepts any certificate during the handshake, but still checks the handshake
/// signature so the peer must hold the certificate's private key. The caller
/// compares the fingerprint before sending anything over the connection.
#[derive(Debug)]
struct PinnedByCaller {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedByCaller {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        None
    }
}
