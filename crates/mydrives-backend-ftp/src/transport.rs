//! How a control connection is opened: plain, explicit FTPS or implicit FTPS.
//!
//! suppaftp's stream type is generic over the TLS layer, so the backend is
//! written once against [`Transport`] and compiled twice: once for plain TCP
//! and once for rustls. Nothing above this module mentions TLS.

use std::sync::Arc;
use std::time::Duration;

use suppaftp::tokio::{
    AsyncNoTlsStream, AsyncRustlsConnector, AsyncRustlsStream, ImplAsyncFtpStream, TokioTlsStream,
};
use suppaftp::{FtpError, FtpResult};

use crate::{FtpSecurity, FtpTarget};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The TLS layer a connection runs on, and how to reach the server over it.
pub trait Transport: TokioTlsStream + Sized + Send + Sync + 'static {
    /// Opens a control connection and brings it to the point where `USER` may
    /// be sent - for FTPS that means the session is already encrypted.
    fn open(
        target: &FtpTarget,
    ) -> impl std::future::Future<Output = FtpResult<ImplAsyncFtpStream<Self>>> + Send;
}

/// Plain TCP connect with a deadline: the server may accept and then never
/// greet, which `connect` alone would wait out forever.
///
/// The address is resolved here, before dialling, so that a failure can say
/// *what was actually dialled*. "Operation timed out" on its own leaves the
/// user guessing whether the name resolved, which address was tried and how
/// long it waited - all of which decide whether the next thing to check is the
/// address, the port forwarding or the server.
async fn connect_tcp<T>(target: &FtpTarget) -> FtpResult<ImplAsyncFtpStream<T>>
where
    T: TokioTlsStream + Send + Sync + 'static,
{
    let addr = format!("{}:{}", target.host, target.port);
    let started = std::time::Instant::now();

    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr)
        .await
        .map_err(|e| {
            FtpError::ConnectionError(std::io::Error::new(
                e.kind(),
                format!("'{}' 주소를 찾지 못했습니다: {e}", target.host),
            ))
        })?
        .collect();
    let dialled = resolved
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if resolved.is_empty() {
        return Err(FtpError::ConnectionError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("'{}' 주소가 비어 있습니다", target.host),
        )));
    }
    tracing::debug!(%addr, %dialled, "FTP 제어 연결 시도");

    let attempt =
        tokio::time::timeout(CONNECT_TIMEOUT, ImplAsyncFtpStream::<T>::connect(addr.clone()));
    let outcome = match attempt.await {
        Ok(result) => result,
        Err(_) => {
            return Err(FtpError::ConnectionError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{} ({dialled}) 가 {}초 안에 응답하지 않았습니다",
                    addr,
                    CONNECT_TIMEOUT.as_secs()
                ),
            )))
        }
    };
    outcome.map_err(|e| match e {
        FtpError::ConnectionError(io) => FtpError::ConnectionError(std::io::Error::new(
            io.kind(),
            format!(
                "{addr} ({dialled}) 에 연결하지 못했습니다 — {io} ({:.1}초 만에)",
                started.elapsed().as_secs_f32()
            ),
        )),
        other => other,
    })
}

impl Transport for AsyncNoTlsStream {
    async fn open(target: &FtpTarget) -> FtpResult<ImplAsyncFtpStream<Self>> {
        connect_tcp::<AsyncNoTlsStream>(target).await
    }
}

impl Transport for AsyncRustlsStream {
    async fn open(target: &FtpTarget) -> FtpResult<ImplAsyncFtpStream<Self>> {
        let connector = AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(
            client_config(target.accept_invalid_certs)?,
        ));
        match target.security {
            // Implicit FTPS: TLS before the greeting, on a port of its own.
            FtpSecurity::Implicit => {
                let addr = format!("{}:{}", target.host, target.port);
                tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    ImplAsyncFtpStream::<AsyncRustlsStream>::connect_secure_implicit(
                        addr,
                        connector,
                        &target.host,
                    ),
                )
                .await
                .map_err(|_| {
                    FtpError::ConnectionError(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "implicit FTPS 연결이 시간 내에 응답하지 않았습니다",
                    ))
                })?
            }
            // Explicit FTPS (the usual one): connect in the clear, then AUTH TLS.
            _ => {
                let plain = connect_tcp::<AsyncRustlsStream>(target).await?;
                plain.into_secure(connector, &target.host).await
            }
        }
    }
}

/// rustls client configuration.
///
/// The crypto provider is passed in rather than taken from the process-wide
/// default: the agent links rustls more than once (the updater's HTTP client
/// too), and a global default that is installed twice panics.
///
/// NAS boxes ship a self-signed certificate for FTPS and most people never
/// replace it, so refusing those outright would mean "FTPS does not work" on
/// the devices this is most used with. The exception is per drive
/// (`accept_invalid_certs`) and is logged on every connect.
fn client_config(accept_invalid_certs: bool) -> FtpResult<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| FtpError::SecureError(e.to_string()))?;

    if accept_invalid_certs {
        tracing::warn!("FTPS: 서버 인증서를 검증하지 않습니다 (accept_invalid_certs = true)");
        return Ok(Arc::new(
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(danger::NoVerify))
                .with_no_client_auth(),
        ));
    }

    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(FtpError::SecureError(
            "이 시스템의 루트 인증서를 읽지 못했습니다".to_string(),
        ));
    }
    Ok(Arc::new(
        builder.with_root_certificates(roots).with_no_client_auth(),
    ))
}

mod danger {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    /// Accepts any certificate. Only reachable through `accept_invalid_certs`.
    #[derive(Debug)]
    pub struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}
