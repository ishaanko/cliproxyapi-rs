//! One connection to Home: TCP (optionally TLS) carrying RESP commands, plus the connection
//! tracking that lets the client fence all in-flight traffic at once (Go: `homeDispatchConn`
//! and go-redis's pool).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use cpa_config::HomeTlsConfig;
use parking_lot::Mutex;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::error::HomeError;
use crate::resp::{self, RespError, Value};

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Forced-close switch of one connection: lets another task abort a blocked read. A thin
/// wrapper over a [`CancellationToken`].
#[derive(Default)]
pub struct Kill(CancellationToken);

impl Kill {
    pub fn kill(&self) {
        self.0.cancel();
    }

    pub fn is_dead(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves once [`Kill::kill`] was called.
    pub async fn wait(&self) {
        self.0.cancelled().await;
    }
}

/// Every live connection of a client (Go: `Client.connections`), so Close/Abort can kill the ones
/// currently blocked in a read.
#[derive(Default)]
pub struct Tracker {
    next: AtomicU64,
    conns: Mutex<HashMap<u64, Arc<Kill>>>,
    /// Shared with the client's `dispatchFenced`: no new connection registers once set.
    fenced: Arc<AtomicBool>,
}

impl Tracker {
    pub fn new(fenced: Arc<AtomicBool>) -> Self {
        Tracker { next: AtomicU64::new(0), conns: Mutex::new(HashMap::new()), fenced }
    }

    fn register(&self, kill: Arc<Kill>) -> Result<u64, HomeError> {
        let mut conns = self.conns.lock();
        if self.fenced.load(Ordering::SeqCst) {
            return Err(HomeError::DispatchFenced);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        conns.insert(id, kill);
        Ok(id)
    }

    fn unregister(&self, id: u64) {
        self.conns.lock().remove(&id);
    }

    /// Kills and forgets every tracked connection.
    pub fn kill_all(&self) {
        let conns: Vec<Arc<Kill>> = self.conns.lock().drain().map(|(_, k)| k).collect();
        for kill in conns {
            kill.kill();
        }
    }

    pub fn len(&self) -> usize {
        self.conns.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// TLS settings of a Home connection.
#[derive(Clone)]
pub struct TlsParams {
    pub config: Arc<ClientConfig>,
    pub server_name: ServerName<'static>,
}

/// Where and how to connect (Go: `redis.Options`).
#[derive(Clone)]
pub struct ConnOpts {
    pub addr: String,
    pub tls: Option<TlsParams>,
    pub dial_timeout: Duration,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
}

/// An established connection. Dropping it closes the socket.
pub struct Conn {
    io: BufReader<Box<dyn Io>>,
    kill: Arc<Kill>,
    tracked: Option<(Arc<Tracker>, u64)>,
    /// A transport error happened: the connection must not be reused.
    pub broken: bool,
    pub opts: Arc<ConnOpts>,
    /// Pool slot held while the connection is checked out.
    pub(crate) permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some((tracker, id)) = self.tracked.take() {
            tracker.unregister(id);
        }
    }
}

fn io_err(e: &RespError) -> HomeError {
    match e {
        RespError::Eof => HomeError::Io("EOF".into()),
        RespError::Protocol => HomeError::Io("protocol error".into()),
        RespError::Io(e) => HomeError::Io(e.to_string()),
    }
}

impl Conn {
    /// Dials `opts.addr` (and handshakes TLS). A tracked connection is refused while the tracker
    /// is fenced.
    pub async fn dial(opts: Arc<ConnOpts>, tracker: Option<Arc<Tracker>>) -> Result<Conn, HomeError> {
        let connect = async {
            let tcp = TcpStream::connect(&opts.addr).await?;
            let _ = tcp.set_nodelay(true);
            let io: Box<dyn Io> = match &opts.tls {
                None => Box::new(tcp),
                Some(tls) => {
                    let connector = TlsConnector::from(tls.config.clone());
                    Box::new(connector.connect(tls.server_name.clone(), tcp).await?)
                }
            };
            Ok::<_, std::io::Error>(io)
        };
        let io = match tokio::time::timeout(opts.dial_timeout, connect).await {
            Err(_) => return Err(HomeError::Timeout),
            Ok(Err(e)) => return Err(HomeError::Io(e.to_string())),
            Ok(Ok(io)) => io,
        };
        let kill = Arc::new(Kill::default());
        let tracked = match tracker {
            Some(t) => {
                let id = t.register(kill.clone())?;
                Some((t, id))
            }
            None => None,
        };
        Ok(Conn { io: BufReader::new(io), kill, tracked, broken: false, opts, permit: None })
    }

    /// go-redis `isHealthyConn`: an idle connection must have nothing to read. Pending is
    /// healthy; EOF, buffered bytes or an error mean the peer closed it or sent unsolicited data.
    pub fn is_healthy(&mut self) -> bool {
        if self.broken || self.kill.is_dead() {
            return false;
        }
        if !self.io.buffer().is_empty() {
            return false;
        }
        let mut cx = Context::from_waker(Waker::noop());
        matches!(Pin::new(&mut self.io).poll_fill_buf(&mut cx), Poll::Pending)
    }

    pub fn kill_switch(&self) -> Arc<Kill> {
        self.kill.clone()
    }

    /// Sends one command (write deadline `opts.write_timeout`).
    pub async fn send(&mut self, args: &[Vec<u8>]) -> Result<(), HomeError> {
        let frame = resp::encode_command(args);
        let timeout = self.opts.write_timeout;
        let kill = self.kill.clone();
        let io = self.io.get_mut();
        let result = tokio::select! {
            _ = kill.wait() => Err(HomeError::Io("use of closed network connection".into())),
            r = tokio::time::timeout(timeout, async {
                io.write_all(&frame).await?;
                io.flush().await
            }) => match r {
                Err(_) => Err(HomeError::Timeout),
                Ok(r) => r.map_err(HomeError::from),
            },
        };
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    /// Reads one reply within `timeout`; an error reply becomes [`HomeError::Redis`].
    pub async fn recv(&mut self, timeout: Duration) -> Result<Value, HomeError> {
        let kill = self.kill.clone();
        let io = &mut self.io;
        let result = tokio::select! {
            _ = kill.wait() => Err(HomeError::Io("use of closed network connection".into())),
            r = tokio::time::timeout(timeout, resp::read_value(io)) => match r {
                Err(_) => Err(HomeError::Timeout),
                Ok(r) => r.map_err(|e| io_err(&e)),
            },
        };
        match result {
            Ok(Value::Error(msg)) => Err(HomeError::Redis(msg)),
            Ok(v) => Ok(v),
            Err(e) => {
                self.broken = true;
                Err(e)
            }
        }
    }

    /// Request/reply round trip with the connection's default timeouts.
    pub async fn call(&mut self, args: &[Vec<u8>]) -> Result<Value, HomeError> {
        let timeout = self.opts.read_timeout;
        self.call_with_timeout(args, timeout).await
    }

    pub async fn call_with_timeout(&mut self, args: &[Vec<u8>], read_timeout: Duration) -> Result<Value, HomeError> {
        self.send(args).await?;
        self.recv(read_timeout).await
    }
}

/// Convenience for building command arguments.
pub fn arg(s: impl AsRef<[u8]>) -> Vec<u8> {
    s.as_ref().to_vec()
}

// ---------------------------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------------------------

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
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
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

fn read_pem_file(path: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Go: `newHomeTLSConfig`. `None` when TLS is disabled.
pub fn new_tls_params(cfg: &HomeTlsConfig, fallback_server_name: &str) -> Result<Option<TlsParams>, HomeError> {
    if !cfg.enable {
        return Ok(None);
    }
    let mut server_name = cfg.server_name.trim().to_string();
    if server_name.is_empty() {
        server_name = fallback_server_name.trim().to_string();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(HomeError::other)?;

    let builder = if cfg.insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = RootCertStore::empty();
        // Go: x509.SystemCertPool() plus the optional CA file.
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        let ca = cfg.ca_cert.trim();
        if !ca.is_empty() {
            let pem = read_pem_file(ca).map_err(|e| HomeError::Other(format!("home tls: read ca-cert: {e}")))?;
            let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice()).filter_map(Result::ok).collect();
            if certs.is_empty() {
                return Err(HomeError::other("home tls: ca-cert contains no PEM certificates"));
            }
            let mut added = 0;
            for cert in certs {
                if roots.add(cert).is_ok() {
                    added += 1;
                }
            }
            if added == 0 {
                return Err(HomeError::other("home tls: ca-cert contains no PEM certificates"));
            }
        }
        builder.with_root_certificates(roots)
    };

    let (cert_path, key_path) = (cfg.client_cert.trim(), cfg.client_key.trim());
    let config = if !cert_path.is_empty() || !key_path.is_empty() {
        if cert_path.is_empty() || key_path.is_empty() {
            return Err(HomeError::other("home tls: client certificate and key must be set together"));
        }
        let load = || -> Result<_, String> {
            let cert_pem = std::fs::read(cert_path).map_err(|e| e.to_string())?;
            let key_pem = std::fs::read(key_path).map_err(|e| e.to_string())?;
            let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_slice())
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "tls: failed to find any PEM data in key input".to_string())?;
            Ok((certs, key))
        };
        let (certs, key) = load().map_err(|e| HomeError::Other(format!("home tls: load client certificate: {e}")))?;
        builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| HomeError::Other(format!("home tls: load client certificate: {e}")))?
    } else {
        builder.with_no_client_auth()
    };

    let name = match server_name.parse::<IpAddr>() {
        Ok(ip) => ServerName::IpAddress(ip.into()),
        Err(_) => ServerName::try_from(server_name.clone())
            .map_err(|_| HomeError::Other(format!("home tls: invalid server name {server_name:?}")))?,
    };
    Ok(Some(TlsParams { config: Arc::new(config), server_name: name }))
}
