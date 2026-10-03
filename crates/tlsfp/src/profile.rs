//! ClientHello profiles on BoringSSL (Go: the uTLS specs in helps/utls_client.go and
//! auth/claude/utls_transport.go).
//!
//! * [`Profile::Chrome`]: `HelloChrome_Auto` (Chrome 133): GREASE, randomized extension order,
//!   X25519MLKEM768 key share, ALPS (new codepoint), brotli certificate compression, ECH GREASE,
//!   ALPN `h2, http/1.1`.
//! * [`Profile::ClaudeInference`]: the Claude Code 2.1.220 (Node/BoringSSL) hello used for
//!   api.anthropic.com: fixed extension order, ALPN `http/1.1`, status_request, SCT, padding.
//! * [`Profile::ClaudeOAuth`]: the compact Axios hello used for the OAuth control plane: same
//!   ciphers and groups, no ALPN, status_request, SCT or padding.
//!
//! The Claude profiles pin the extension order with `set_extension_order` and send the exact
//! cipher vector of the Go spec (`set_raw_cipher_list`), so an unresumed hello is byte-for-byte
//! stable. A cached TLS session adds `pre_shared_key` last, like uTLS with `OmitEmptyPsk`.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use rama_boring::ssl::{
    CertificateCompressionAlgorithm, CertificateCompressor, ConnectConfiguration, NameType,
    SslConnector, SslCurve, SslMethod, SslSession, SslSessionCacheMode, SslSignatureAlgorithm,
    SslVerifyMode, SslVersion,
};
use rama_boring::x509::X509;
use rama_boring::x509::store::{X509Store, X509StoreBuilder};
use rama_boring_tokio::SslStream;
use tokio::io::{AsyncRead, AsyncWrite};

/// Which ClientHello to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    Chrome,
    ClaudeInference,
    ClaudeOAuth,
}

/// Failure while building a connector or handshaking.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{0}")]
    Setup(String),
    /// Handshake failure; the caller adds its own prefix (`utls: TLS handshake: ...`).
    #[error("{0}")]
    Handshake(String),
}

// TLS extension ids used for the pinned order.
const EXT_SNI: u16 = 0;
const EXT_STATUS_REQUEST: u16 = 5;
const EXT_GROUPS: u16 = 10;
const EXT_EC_POINTS: u16 = 11;
const EXT_SIGALGS: u16 = 13;
const EXT_ALPN: u16 = 16;
const EXT_SCT: u16 = 18;
const EXT_PADDING: u16 = 21;
const EXT_EMS: u16 = 23;
const EXT_SESSION_TICKET: u16 = 35;
const EXT_PSK: u16 = 41;
const EXT_SUPPORTED_VERSIONS: u16 = 43;
const EXT_PSK_MODES: u16 = 45;
const EXT_KEY_SHARE: u16 = 51;
const EXT_RENEGOTIATION: u16 = 0xff01;

/// Cipher vector of the Claude Code profiles (identical for inference and OAuth).
const CLAUDE_CIPHERS: [u16; 17] = [
    0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc009, 0xc013, 0xc00a,
    0xc014, 0x009c, 0x009d, 0x002f, 0x0035,
];

/// Chrome 133 cipher vector (the leading GREASE value is added by BoringSSL).
const CHROME_CIPHERS: [u16; 15] = [
    0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc013, 0xc014, 0x009c,
    0x009d, 0x002f, 0x0035,
];

const CLAUDE_INFERENCE_ORDER: [u16; 15] = [
    EXT_SNI,
    EXT_EMS,
    EXT_RENEGOTIATION,
    EXT_GROUPS,
    EXT_EC_POINTS,
    EXT_SESSION_TICKET,
    EXT_ALPN,
    EXT_STATUS_REQUEST,
    EXT_SIGALGS,
    EXT_SCT,
    EXT_KEY_SHARE,
    EXT_PSK_MODES,
    EXT_SUPPORTED_VERSIONS,
    EXT_PADDING,
    EXT_PSK,
];

const CLAUDE_OAUTH_ORDER: [u16; 11] = [
    EXT_SNI,
    EXT_EMS,
    EXT_RENEGOTIATION,
    EXT_GROUPS,
    EXT_EC_POINTS,
    EXT_SESSION_TICKET,
    EXT_SIGALGS,
    EXT_KEY_SHARE,
    EXT_PSK_MODES,
    EXT_SUPPORTED_VERSIONS,
    EXT_PSK,
];

/// Brotli certificate decompression, advertised by the Chrome profile (RFC 8879).
struct BrotliCerts;

impl CertificateCompressor for BrotliCerts {
    const ALGORITHM: CertificateCompressionAlgorithm = CertificateCompressionAlgorithm::BROTLI;
    const CAN_COMPRESS: bool = false;
    const CAN_DECOMPRESS: bool = true;

    fn decompress<W: io::Write>(&self, input: &[u8], output: &mut W) -> io::Result<()> {
        let mut reader = brotli::Decompressor::new(input, 4096);
        io::copy(&mut reader, output).map(|_| ())
    }
}

/// System roots (Go: the default `RootCAs`; honors `SSL_CERT_FILE`/`SSL_CERT_DIR`). Parsed once.
static ROOT_STORE: LazyLock<Result<X509Store, String>> = LazyLock::new(|| build_store(&[]));

/// System roots plus `extra` DER certificates.
fn build_store(extra: &[Vec<u8>]) -> Result<X509Store, String> {
    let mut builder = X509StoreBuilder::new().map_err(|e| e.to_string())?;
    let mut added = 0usize;
    for der in rustls_native_certs::load_native_certs().certs {
        if let Ok(cert) = X509::from_der(der.as_ref())
            && builder.add_cert(cert).is_ok()
        {
            added += 1;
        }
    }
    if added == 0 {
        tracing::warn!("tlsfp: no system root certificates found");
    }
    for der in extra {
        let cert = X509::from_der(der).map_err(|e| e.to_string())?;
        builder.add_cert(cert).map_err(|e| e.to_string())?;
    }
    Ok(builder.build())
}

/// Installs the system root store (shared and parsed once unless `extra` roots are requested).
pub(crate) fn install_roots(b: &mut rama_boring::ssl::SslContextBuilder, extra: &[Vec<u8>]) -> Result<(), TlsError> {
    let err = |e: &str| TlsError::Setup(format!("tlsfp: root store: {e}"));
    if extra.is_empty() {
        match &*ROOT_STORE {
            Ok(store) => b.set_cert_store_ref(store),
            Err(e) => return Err(err(e)),
        }
    } else {
        b.set_cert_store(build_store(extra).map_err(|e| err(&e))?);
    }
    Ok(())
}

/// Bounded LRU of resumable sessions keyed by server name (Go: `tls.NewLRUClientSessionCache`).
/// Owned by one [`TlsConnector`], so every cached session comes from that connector's context.
struct SessionCache {
    capacity: usize,
    entries: Mutex<VecDeque<(String, SslSession)>>,
}

impl SessionCache {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self { capacity: capacity.max(1), entries: Mutex::new(VecDeque::new()) })
    }

    fn put(&self, host: &str, session: SslSession) {
        let mut entries = self.entries.lock();
        entries.retain(|(h, _)| h != host);
        entries.push_back((host.to_string(), session));
        while entries.len() > self.capacity {
            entries.pop_front();
        }
    }

    fn get(&self, host: &str) -> Option<SslSession> {
        let mut entries = self.entries.lock();
        let pos = entries.iter().position(|(h, _)| h == host)?;
        let entry = entries.remove(pos)?;
        let session = entry.1.clone();
        entries.push_back(entry);
        Some(session)
    }
}

/// A configured BoringSSL context for one profile plus the session cache it feeds. Cheap to
/// clone (clones share both); every connection draws its own GREASE values and extension
/// permutation.
#[derive(Clone)]
pub struct TlsConnector {
    profile: Profile,
    connector: SslConnector,
    sessions: Option<Arc<SessionCache>>,
}

impl std::fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConnector").field("profile", &self.profile).finish()
    }
}

fn setup<T, E: std::fmt::Display>(what: &str, r: Result<T, E>) -> Result<T, TlsError> {
    r.map_err(|e| TlsError::Setup(format!("tlsfp: {what}: {e}")))
}

impl TlsConnector {
    /// Builds the context for `profile`. `session_capacity` enables TLS session resumption with
    /// an LRU of that size (the Claude profiles; the Chrome profile keeps no cache, like Go).
    pub fn new(profile: Profile, session_capacity: Option<usize>, extra_roots: &[Vec<u8>]) -> Result<Self, TlsError> {
        let sessions = session_capacity.map(SessionCache::new);
        let mut b = setup("new context", SslConnector::no_default_verify_builder(SslMethod::tls_client()))?;
        install_roots(&mut b, extra_roots)?;
        b.set_verify(SslVerifyMode::PEER);
        setup("min version", b.set_min_proto_version(Some(SslVersion::TLS1_2)))?;
        setup("max version", b.set_max_proto_version(Some(SslVersion::TLS1_3)))?;
        match profile {
            Profile::Chrome => {
                b.set_grease_enabled(true);
                b.set_permute_extensions(true);
                setup("ciphers", b.set_raw_cipher_list(&CHROME_CIPHERS))?;
                setup(
                    "curves",
                    b.set_curves(&[
                        SslCurve::X25519_MLKEM768,
                        SslCurve::X25519,
                        SslCurve::SECP256R1,
                        SslCurve::SECP384R1,
                    ]),
                )?;
                setup("sigalgs", b.set_verify_algorithm_prefs(&chrome_sigalgs()))?;
                setup("alpn", b.set_alpn_protos(b"\x02h2\x08http/1.1"))?;
                b.enable_ocsp_stapling();
                b.enable_signed_cert_timestamps();
                setup("brotli certs", b.add_certificate_compression_algorithm(BrotliCerts))?;
            }
            Profile::ClaudeInference | Profile::ClaudeOAuth => {
                b.set_grease_enabled(false);
                b.set_permute_extensions(false);
                setup("ciphers", b.set_raw_cipher_list(&CLAUDE_CIPHERS))?;
                setup("curves", b.set_curves(&[SslCurve::X25519, SslCurve::SECP256R1, SslCurve::SECP384R1]))?;
                setup("sigalgs", b.set_verify_algorithm_prefs(&claude_sigalgs()))?;
                if profile == Profile::ClaudeInference {
                    setup("alpn", b.set_alpn_protos(b"\x08http/1.1"))?;
                    b.enable_ocsp_stapling();
                    b.enable_signed_cert_timestamps();
                    setup("extension order", b.set_extension_order(&CLAUDE_INFERENCE_ORDER))?;
                } else {
                    setup("extension order", b.set_extension_order(&CLAUDE_OAUTH_ORDER))?;
                }
            }
        }
        if let Some(cache) = &sessions {
            b.set_session_cache_mode(SslSessionCacheMode::CLIENT | SslSessionCacheMode::NO_INTERNAL);
            let cache = Arc::clone(cache);
            b.set_new_session_callback(move |ssl, session| {
                if let Some(host) = ssl.servername(NameType::HOST_NAME) {
                    cache.put(host, session);
                }
            });
        }
        Ok(Self { profile, connector: b.build(), sessions })
    }

    fn configure(&self, host: &str) -> Result<ConnectConfiguration, TlsError> {
        let mut cfg = setup("configure", self.connector.configure())?;
        if self.profile == Profile::Chrome {
            cfg.set_enable_ech_grease(true);
            cfg.set_alps_use_new_codepoint(true);
            setup("alps", cfg.add_application_settings(b"h2"))?;
        }
        if let Some(cache) = &self.sessions
            && let Some(session) = cache.get(host)
        {
            // SAFETY: the cache is private to this connector and only fed by the new-session
            // callback of this connector's context, so the session comes from this very context,
            // which is the requirement of SSL_set_session.
            #[allow(unsafe_code)]
            let _ = unsafe { cfg.set_session(&session) };
        }
        Ok(cfg)
    }

    /// Handshakes over `stream`, verifying the certificate for `host` (also sent as SNI unless it
    /// is an IP literal). Dropping the future aborts the handshake.
    pub async fn connect<S>(&self, host: &str, stream: S) -> Result<SslStream<S>, TlsError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let cfg = self.configure(host)?;
        rama_boring_tokio::connect(cfg, Some(host), stream)
            .await
            .map_err(|e| TlsError::Handshake(e.to_string()))
    }
}

fn chrome_sigalgs() -> [SslSignatureAlgorithm; 8] {
    [
        SslSignatureAlgorithm::ECDSA_SECP256R1_SHA256,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA256,
        SslSignatureAlgorithm::RSA_PKCS1_SHA256,
        SslSignatureAlgorithm::ECDSA_SECP384R1_SHA384,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA384,
        SslSignatureAlgorithm::RSA_PKCS1_SHA384,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA512,
        SslSignatureAlgorithm::RSA_PKCS1_SHA512,
    ]
}

fn claude_sigalgs() -> [SslSignatureAlgorithm; 9] {
    [
        SslSignatureAlgorithm::ECDSA_SECP256R1_SHA256,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA256,
        SslSignatureAlgorithm::RSA_PKCS1_SHA256,
        SslSignatureAlgorithm::ECDSA_SECP384R1_SHA384,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA384,
        SslSignatureAlgorithm::RSA_PKCS1_SHA384,
        SslSignatureAlgorithm::RSA_PSS_RSAE_SHA512,
        SslSignatureAlgorithm::RSA_PKCS1_SHA512,
        SslSignatureAlgorithm::RSA_PKCS1_SHA1,
    ]
}
