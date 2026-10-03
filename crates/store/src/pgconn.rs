//! One self-healing Postgres connection (Go: `database/sql` + pgx stdlib).
//!
//! DSN semantics follow libpq/pgx: `sslmode` defaults to `prefer`; `prefer` and `require` encrypt
//! without verifying the server certificate; `verify-ca` checks the chain only and `verify-full`
//! also the hostname, against `sslrootcert` when given (else the system roots). `sslcert` /
//! `sslkey` supply a client certificate. pgx-only keys are stripped and unknown keys are sent as
//! server runtime parameters (`search_path=x` becomes `options=-c search_path=x`).

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::{Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Error text for a driver error. `tokio_postgres::Error` prints only its kind (for example
/// "db error"), so the cause chain is appended; server errors read like pgx's
/// `ERROR: message (SQLSTATE code)`.
pub(crate) trait ErrText {
    fn err_text(&self) -> String;
}

impl ErrText for String {
    fn err_text(&self) -> String {
        self.clone()
    }
}

impl ErrText for tokio_postgres::Error {
    fn err_text(&self) -> String {
        if let Some(db) = self.as_db_error() {
            return format!("{}: {} (SQLSTATE {})", db.severity(), db.message(), db.code().code());
        }
        let mut text = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }
}

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

/// How the server certificate is checked (pgx `sslmode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verify {
    /// `prefer` / `require` without a root cert: encrypt, trust anything.
    None,
    /// `verify-ca` (or `require` with `sslrootcert`): chain only, no hostname check.
    Ca,
    /// `verify-full`: chain and hostname.
    Full,
}

/// Client-side TLS settings pulled out of the DSN, which tokio-postgres cannot parse.
#[derive(Debug, Default, PartialEq, Eq)]
struct TlsOptions {
    verify: Option<Verify>,
    root_cert: Option<String>,
    client_cert: Option<String>,
    client_key: Option<String>,
}

/// Options tokio-postgres understands; everything else is either client-side (pgx handles it) or
/// a server runtime parameter.
const TOKIO_KEYS: &[&str] = &[
    "user",
    "password",
    "dbname",
    "options",
    "application_name",
    "sslmode",
    "sslnegotiation",
    "host",
    "hostaddr",
    "port",
    "connect_timeout",
    "tcp_user_timeout",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_retries",
    "target_session_attrs",
    "channel_binding",
    "load_balance_hosts",
];

/// pgx consumes these itself instead of sending them to the server.
const CLIENT_KEYS: &[&str] = &[
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslpassword",
    "sslsni",
    "sslcrl",
    "sslinline",
    "krbsrvname",
    "krbspn",
    "gsslib",
    "gssencmode",
    "service",
    "servicefile",
    "min_read_buffer_size",
];

/// Keyword/value DSN tokens (libpq quoting: single quotes, backslash escapes).
fn parse_keywords(dsn: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut chars = dsn.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Ok(out);
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') {
            return Err("invalid dsn".to_string());
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err("unterminated quoted string in connection info string".to_string()),
                    Some('\\') => value.extend(chars.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        out.push((key, value));
    }
}

fn quote_keyword_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn escape_option_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\ ")
}

/// Rewrites a pgx-style DSN into one tokio-postgres accepts: client-side TLS settings are pulled
/// into [`TlsOptions`], `verify-*` modes become `require`, and unknown keys (pgx sends them as
/// server runtime parameters such as `search_path`) move into `options=-c key=value`.
fn prepare_dsn(dsn: &str) -> Result<(String, TlsOptions), String> {
    let trimmed = dsn.trim();
    let is_url = trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://");
    let (head, pairs): (&str, Vec<(String, String)>) = if is_url {
        let (head, query) = trimmed.split_once('?').unwrap_or((trimmed, ""));
        let pairs = url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        (head, pairs)
    } else {
        ("", parse_keywords(trimmed)?)
    };

    let mut tls = TlsOptions::default();
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut runtime: Vec<(String, String)> = Vec::new();
    let mut sslmode = String::new();
    for (key, value) in pairs {
        match key.as_str() {
            "sslmode" => sslmode = value,
            "sslrootcert" => tls.root_cert = Some(value).filter(|v| !v.is_empty()),
            "sslcert" => tls.client_cert = Some(value).filter(|v| !v.is_empty()),
            "sslkey" => tls.client_key = Some(value).filter(|v| !v.is_empty()),
            k if CLIENT_KEYS.contains(&k) => {}
            k if TOKIO_KEYS.contains(&k) => kept.push((key, value)),
            _ => runtime.push((key, value)),
        }
    }
    let tokio_mode = match sslmode.as_str() {
        "" => None,
        "disable" => Some("disable"),
        "allow" | "prefer" => Some("prefer"),
        "require" => {
            tls.verify = Some(if tls.root_cert.is_some() { Verify::Ca } else { Verify::None });
            Some("require")
        }
        "verify-ca" => {
            tls.verify = Some(Verify::Ca);
            Some("require")
        }
        "verify-full" => {
            tls.verify = Some(Verify::Full);
            Some("require")
        }
        other => return Err(format!("sslmode is invalid: {other}")),
    };
    if let Some(mode) = tokio_mode {
        kept.push(("sslmode".to_string(), mode.to_string()));
    }
    if !runtime.is_empty() {
        let extra: Vec<String> = runtime.iter().map(|(k, v)| format!("-c {k}={}", escape_option_value(v))).collect();
        let extra = extra.join(" ");
        match kept.iter_mut().find(|(k, _)| k == "options") {
            Some((_, existing)) => *existing = format!("{existing} {extra}"),
            None => kept.push(("options".to_string(), extra)),
        }
    }

    let rebuilt = if is_url {
        let mut out = head.to_string();
        for (i, (k, v)) in kept.iter().enumerate() {
            out.push(if i == 0 { '?' } else { '&' });
            out.push_str(&percent_encoding::utf8_percent_encode(k, percent_encoding::NON_ALPHANUMERIC).to_string());
            out.push('=');
            out.push_str(&percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string());
        }
        out
    } else {
        kept.iter().map(|(k, v)| format!("{k}={}", quote_keyword_value(v))).collect::<Vec<_>>().join(" ")
    };
    Ok((rebuilt, tls))
}

/// Chain verification only: a name mismatch is accepted (pgx `verify-ca`).
#[derive(Debug)]
struct CaOnlyVerifier(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for CaOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Trust anchors: only `sslrootcert` when given (as pgx), else the system roots.
fn root_store(root_cert: Option<&str>) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    match root_cert {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path)
                .map_err(|e| format!("unable to read CA file: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("unable to read CA file: {e}"))?;
            let (added, _) = roots.add_parsable_certificates(certs);
            if added == 0 {
                return Err("unable to add CA to cert pool".to_string());
            }
        }
        None => {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
        }
    }
    Ok(roots)
}

fn tls_connector(opts: &TlsOptions) -> Result<MakeRustlsConnect, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let verify = opts.verify.unwrap_or(Verify::None);
    let builder = match verify {
        Verify::None => builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider))),
        Verify::Full => builder.with_root_certificates(root_store(opts.root_cert.as_deref())?),
        Verify::Ca => {
            let inner = WebPkiServerVerifier::builder_with_provider(
                Arc::new(root_store(opts.root_cert.as_deref())?),
                provider,
            )
            .build()
            .map_err(|e| e.to_string())?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(CaOnlyVerifier(inner)))
        }
    };
    let config = match (&opts.client_cert, &opts.client_key) {
        (Some(cert), Some(key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .map_err(|e| format!("unable to read cert: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("unable to read cert: {e}"))?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("unable to read key: {e}"))?;
            builder.with_client_auth_cert(chain, key).map_err(|e| format!("unable to load client certificate: {e}"))?
        }
        _ => builder.with_no_client_auth(),
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
        let (dsn, tls) = prepare_dsn(dsn)?;
        let mut config = Config::from_str(&dsn).map_err(|e| e.err_text())?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(Duration::from_secs(30));
        }
        Ok(Self { config, tls: tls_connector(&tls)?, client: None })
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
        let (dsn, tls) = prepare_dsn("postgres://u@h/db?sslmode=verify-full").unwrap();
        assert!(dsn.contains("sslmode=require"));
        assert_eq!(tls.verify, Some(Verify::Full));
        let (_, tls) = prepare_dsn("host=h sslmode=verify-ca sslrootcert=/ca.pem").unwrap();
        assert_eq!((tls.verify, tls.root_cert.as_deref()), (Some(Verify::Ca), Some("/ca.pem")));
        // `require` with a root cert verifies the chain, like libpq.
        let (_, tls) = prepare_dsn("host=h sslmode=require sslrootcert=/ca.pem").unwrap();
        assert_eq!(tls.verify, Some(Verify::Ca));
        let (_, tls) = prepare_dsn("host=h sslmode=require").unwrap();
        assert_eq!(tls.verify, Some(Verify::None));
        assert!(prepare_dsn("host=h sslmode=bogus").is_err());
    }

    #[test]
    fn pgx_only_and_runtime_keys_are_rewritten() {
        let (dsn, tls) =
            prepare_dsn("postgres://u:p@h:5432/db?sslmode=require&sslcert=/c.pem&sslkey=/k.pem&search_path=app,public").unwrap();
        assert_eq!((tls.client_cert.as_deref(), tls.client_key.as_deref()), (Some("/c.pem"), Some("/k.pem")));
        let config = Config::from_str(&dsn).unwrap();
        assert_eq!(config.get_options(), Some("-c search_path=app,public"));
        assert_eq!(config.get_dbname(), Some("db"));

        let (dsn, _) = prepare_dsn("host=h dbname=db options='-c x=1' application_name='my app' sslinline=true timezone=UTC").unwrap();
        let config = Config::from_str(&dsn).unwrap();
        assert_eq!(config.get_options(), Some("-c x=1 -c timezone=UTC"));
        assert_eq!(config.get_application_name(), Some("my app"));
    }
}
