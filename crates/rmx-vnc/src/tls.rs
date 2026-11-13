//! TLS for VeNCrypt, on `rustls` with the `ring` provider.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::{CertificatePolicy, CertificatePrompt, ConnectError};

/// Runs the TLS handshake on `stream` and applies `policy` to the certificate it proved, before
/// anything else crosses the connection.
pub(crate) async fn upgrade<S>(
    stream: S,
    server_name: &str,
    policy: &CertificatePolicy,
) -> Result<Box<TlsStream<S>>, ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AnyCertificate { provider: provider.clone() });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    // The name is only sent, never checked: a pin says which server this is.
    let domain = ServerName::try_from(server_name.to_owned())
        .unwrap_or_else(|_| ServerName::try_from("localhost").expect("a valid name"));
    let mut stream =
        TlsConnector::from(Arc::new(config)).connect(domain, stream).await.map_err(tls_error)?;
    stream.flush().await?;

    let der = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| ConnectError::Failed("the server sent no certificate".to_owned()))?
        .as_ref()
        .to_vec();
    if !accepts(policy, server_name, der).await {
        return Err(ConnectError::CertificateRejected);
    }
    Ok(Box::new(stream))
}

fn tls_error(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Failed(format!("TLS failed: {error}"))
}

/// SHA-256 of a certificate's DER encoding, which is what is pinned.
pub(crate) fn fingerprint(der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

async fn accepts(policy: &CertificatePolicy, server_name: &str, der: Vec<u8>) -> bool {
    let sha256 = fingerprint(&der);
    match policy {
        CertificatePolicy::AcceptAny => true,
        CertificatePolicy::Pinned(expected) => *expected == sha256,
        CertificatePolicy::Ask(prompts) => {
            let (answer, reply) = oneshot::channel();
            let prompt =
                CertificatePrompt { server_name: server_name.to_owned(), der, sha256, answer };
            prompts.send(prompt).await.is_ok() && reply.await.unwrap_or(false)
        },
    }
}

/// Leaves the certificate decision to [`CertificatePolicy`], but still checks that the server
/// holds the private key of the certificate it presented.
#[derive(Debug)]
struct AnyCertificate {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AnyCertificate {
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
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
