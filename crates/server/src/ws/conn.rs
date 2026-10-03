//! The client socket of a Responses websocket session. Normally the session loop reads it
//! between turns. With `codex.response-steering` a dedicated reader task is the only reader and
//! queues frames for whichever side needs them: the loop between turns, the Codex duplex stream
//! during one (Go: `readResponsesWebsocketInput`).

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use cpa_runtime::executor::{WebsocketFrame, WebsocketInput};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Bounded queue: clients are back-pressured instead of retaining unlimited input.
const INPUT_QUEUE: usize = 16;

enum Kind {
    Direct(Box<WebSocket>),
    Duplex { sink: SplitSink<WebSocket, Message>, input: WebsocketInput, reader: JoinHandle<()> },
    /// Torn down by [`Conn::close`]: writes fail, reads see the end of the stream.
    Closed,
}

pub struct Conn(Kind);

impl Conn {
    pub fn direct(socket: WebSocket) -> Self {
        Conn(Kind::Direct(Box::new(socket)))
    }

    /// Splits the socket and starts the single reader; the returned input is also handed to
    /// executors so a steering stream can consume client frames during a turn.
    pub fn duplex(socket: WebSocket) -> Self {
        let (sink, mut stream) = socket.split();
        let (tx, rx) = mpsc::channel::<WebsocketFrame>(INPUT_QUEUE);
        let reader = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                let payload = match frame {
                    Ok(Message::Text(text)) => text.as_str().as_bytes().to_vec(),
                    Ok(Message::Binary(bytes)) => bytes.to_vec(),
                    Ok(Message::Close(_)) | Err(_) => return,
                    Ok(_) => continue,
                };
                if tx.send(Ok(payload)).await.is_err() {
                    return;
                }
            }
        });
        Conn(Kind::Duplex { sink, input: WebsocketInput::new(rx), reader })
    }

    /// The shared frame queue in duplex mode.
    pub fn input(&self) -> Option<WebsocketInput> {
        match &self.0 {
            Kind::Direct(_) | Kind::Closed => None,
            Kind::Duplex { input, .. } => Some(input.clone()),
        }
    }

    /// Drops the connection without a close frame (Go: `conn.Close()`).
    pub fn close(&mut self) {
        if let Kind::Duplex { reader, .. } = &self.0 {
            reader.abort();
        }
        self.0 = Kind::Closed;
    }


    pub async fn send(&mut self, msg: Message) -> Result<(), axum::Error> {
        match &mut self.0 {
            Kind::Direct(socket) => socket.send(msg).await,
            Kind::Duplex { sink, .. } => sink.send(msg).await,
            Kind::Closed => Err(axum::Error::new(std::io::Error::other("use of closed network connection"))),
        }
    }

    /// Queues `msg` without flushing; [`Conn::flush`] puts it on the wire.
    pub async fn feed(&mut self, msg: Message) -> Result<(), axum::Error> {
        match &mut self.0 {
            Kind::Direct(socket) => socket.feed(msg).await,
            Kind::Duplex { sink, .. } => sink.feed(msg).await,
            Kind::Closed => Err(axum::Error::new(std::io::Error::other("use of closed network connection"))),
        }
    }

    pub async fn flush(&mut self) -> Result<(), axum::Error> {
        match &mut self.0 {
            Kind::Direct(socket) => SinkExt::flush(socket.as_mut()).await,
            Kind::Duplex { sink, .. } => sink.flush().await,
            Kind::Closed => Ok(()),
        }
    }

    /// Next client frame; in duplex mode text and binary frames arrive as binary, a reader that
    /// ended (close or error) as `None`.
    pub async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
        match &mut self.0 {
            Kind::Direct(socket) => socket.recv().await,
            Kind::Closed => None,
            Kind::Duplex { input, .. } => match input.recv().await? {
                Ok(payload) => Some(Ok(Message::Binary(Bytes::from(payload)))),
                Err(err) => Some(Err(axum::Error::new(err))),
            },
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close();
    }
}

/// Upstream-disconnect subscriptions of one client session (Go: `UpstreamDisconnectChan` of the
/// Codex and xAI executors). Fires at most once per subscription.
pub struct Disconnects {
    receivers: Vec<(&'static str, tokio::sync::watch::Receiver<Option<String>>)>,
    disarmed: bool,
}

impl Disconnects {
    pub fn new(receivers: Vec<(&'static str, tokio::sync::watch::Receiver<Option<String>>)>) -> Self {
        Disconnects { receivers, disarmed: false }
    }

    /// Stops reacting to disconnects (a Codex steering stream owns the socket's closure).
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// Resolves with `(provider, error text)` when an upstream connection of the session dropped;
    /// pending forever when there is nothing to wait for.
    pub async fn fired(&mut self) -> (&'static str, String) {
        if self.disarmed || self.receivers.is_empty() {
            return std::future::pending().await;
        }
        let waits = self.receivers.iter_mut().map(|(provider, rx)| {
            Box::pin(async move {
                let text = snapshot(rx.wait_for(Option::is_some).await);
                match text {
                    Some(text) => (*provider, text),
                    None => std::future::pending().await,
                }
            })
        });
        futures_util::future::select_all(waits).await.0
    }
}

/// The notification text, taken so the watch guard never lives across an await point.
fn snapshot(result: Result<tokio::sync::watch::Ref<'_, Option<String>>, tokio::sync::watch::error::RecvError>) -> Option<String> {
    result.ok().and_then(|value| value.clone())
}
