//! HTTP and Redis protocol on one port (Go: `internal/api/protocol_multiplexer.go`,
//! `mux_listener.go`, `buffered_conn.go`).
//!
//! The accept loop gives every connection its own task that (for TLS) completes the handshake and
//! then peeks the first byte: RESP prefixes go to the usage output, everything else is replayed
//! to the HTTP server through [`MuxListener`]. A connection that sends nothing within 10 seconds
//! is closed, so an idle client cannot hold the accept path.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use cpa_home::resp::is_resp_prefix;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::redis_protocol::RedisProtocol;

/// Time a new connection has for the TLS handshake and its first byte.
const SNIFF_DEADLINE: Duration = Duration::from_secs(10);
/// Connections waiting for the HTTP server (Go: the mux listener buffer).
const HTTP_QUEUE: usize = 1024;

enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// A client connection with the sniffed first byte pushed back (Go: `bufferedConn`).
pub struct MuxIo {
    pending: Option<u8>,
    transport: Transport,
}

impl AsyncRead for MuxIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if buf.remaining() > 0
            && let Some(b) = self.pending.take()
        {
            buf.put_slice(&[b]);
            return Poll::Ready(Ok(()));
        }
        match &mut self.transport {
            Transport::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MuxIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match &mut self.transport {
            Transport::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.transport {
            Transport::Plain(s) => Pin::new(s).poll_flush(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.transport {
            Transport::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The HTTP side of the multiplexer: yields the connections the router decided are HTTP.
pub struct MuxListener {
    ready: mpsc::Receiver<(MuxIo, SocketAddr)>,
    local_addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

impl Drop for MuxListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl Listener for MuxListener {
    type Io = MuxIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // The sender lives in the accept task, which only ends when this listener is dropped.
        self.ready.recv().await.expect("mux accept task ended")
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

/// Starts the accept loop over `listener` (TLS when `tls` is set). `redis` serves RESP clients;
/// without it they are disconnected.
pub fn start(listener: TcpListener, tls: Option<TlsAcceptor>, redis: Option<Arc<RedisProtocol>>) -> io::Result<MuxListener> {
    let local_addr = listener.local_addr()?;
    let (tx, ready) = mpsc::channel(HTTP_QUEUE);
    let accept_task = tokio::spawn(async move {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            // One task per connection so slow or idle clients cannot block the accept loop.
            let (tls, tx, redis) = (tls.clone(), tx.clone(), redis.clone());
            tokio::spawn(route_connection(tcp, peer, tls, tx, redis));
        }
    });
    Ok(MuxListener { ready, local_addr, accept_task })
}

/// Protocol detection for one connection (Go: `routeMuxConnection`).
async fn route_connection(
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Option<TlsAcceptor>,
    http: mpsc::Sender<(MuxIo, SocketAddr)>,
    redis: Option<Arc<RedisProtocol>>,
) {
    let sniffed = tokio::time::timeout(SNIFF_DEADLINE, async {
        let transport = match tls {
            None => Transport::Plain(tcp),
            Some(acceptor) => {
                let stream = match acceptor.accept(tcp).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("tls handshake with {peer} failed: {e}");
                        return None;
                    }
                };
                let alpn = stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                let transport = Transport::Tls(Box::new(stream));
                if matches!(alpn.as_deref(), Some(b"h2" | b"http/1.1")) {
                    return Some((MuxIo { pending: None, transport }, None));
                }
                transport
            }
        };
        let mut io = MuxIo { pending: None, transport };
        let first = io.read_u8().await.ok()?;
        io.pending = Some(first);
        Some((io, Some(first)))
    })
    .await;
    let Ok(Some((io, first))) = sniffed else {
        return;
    };
    if let Some(first) = first
        && is_resp_prefix(first)
    {
        if let Some(redis) = redis {
            redis.handle(io, peer).await;
        }
        return;
    }
    let _ = http.send((io, peer)).await;
}
