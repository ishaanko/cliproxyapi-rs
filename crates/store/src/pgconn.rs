//! One self-healing Postgres connection (Go: `database/sql` + pgx stdlib).
//!
//! DSN semantics follow libpq/pgx: `sslmode` defaults to `prefer`; `prefer` and `require` encrypt
//! without verifying the server certificate, `verify-ca` / `verify-full` verify it against the
//! system roots.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::{Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Accepts any server certificate (libpq `sslmode=prefer|require` behavior).
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Splits `sslmode=verify-ca|verify-full` out of the DSN (tokio-postgres only knows
/// disable/prefer/require): returns the DSN with it rewritten to `require` and whether the server
/// certificate must be verified.
fn split_verify_mode(dsn: &str) -> (String, bool) {
    for mode in ["verify-full", "verify-ca"] {
        let needle = format!("sslmode={mode}");
        if dsn.contains(&needle) {
            return (dsn.replace(&needle, "sslmode=require"), true);
        }
    }
    (dsn.to_string(), false)
}

fn tls_connector(verify: bool) -> Result<MakeRustlsConnect, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = if verify {
        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth()
    };
    Ok(MakeRustlsConnect::new(config))
}

pub(crate) struct Conn {
    config: Config,
    tls: MakeRustlsConnect,
    client: Option<Client>,
}

impl Conn {
    pub(crate) fn new(dsn: &str) -> Result<Self, String> {
        let (dsn, verify) = split_verify_mode(dsn);
        let mut config = Config::from_str(&dsn).map_err(|e| e.to_string())?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(Duration::from_secs(30));
        }
        Ok(Self { config, tls: tls_connector(verify)?, client: None })
    }

    /// The live client, reconnecting when the previous connection has died.
    pub(crate) async fn client(&mut self) -> Result<&mut Client, tokio_postgres::Error> {
        if self.client.as_ref().is_none_or(Client::is_closed) {
            let (client, connection) = self.config.connect(self.tls.clone()).await?;
            tokio::spawn(async move {
                if let Err(err) = connection.await {
                    tracing::debug!("postgres connection closed: {err}");
                }
            });
            self.client = Some(client);
        }
        match self.client.as_mut() {
            Some(client) => Ok(client),
            None => unreachable!("client was just set"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_modes_are_split_out() {
        assert_eq!(
            split_verify_mode("postgres://u@h/db?sslmode=verify-full"),
            ("postgres://u@h/db?sslmode=require".to_string(), true)
        );
        assert_eq!(split_verify_mode("host=h sslmode=require").1, false);
    }
}
