//! A scriptable mock Home RESP server for tests (mirrors the command-log servers in the Go
//! tests). Not used in production code.

use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::resp::read_command;

/// What the mock does with a command.
pub enum Reply {
    /// Writes these bytes.
    Bytes(Vec<u8>),
    /// Writes nothing (the client sees a read timeout).
    Silent,
    /// Closes the connection without replying.
    Close,
}

impl From<Vec<u8>> for Reply {
    fn from(b: Vec<u8>) -> Self {
        Reply::Bytes(b)
    }
}

pub fn bulk(s: impl AsRef<[u8]>) -> Reply {
    let s = s.as_ref();
    let mut out = format!("${}\r\n", s.len()).into_bytes();
    out.extend_from_slice(s);
    out.extend_from_slice(b"\r\n");
    Reply::Bytes(out)
}

pub fn nil() -> Reply {
    Reply::Bytes(b"$-1\r\n".to_vec())
}

pub fn ok() -> Reply {
    Reply::Bytes(b"+OK\r\n".to_vec())
}

pub fn int(n: i64) -> Reply {
    Reply::Bytes(format!(":{n}\r\n").into_bytes())
}

pub fn err(message: &str) -> Reply {
    Reply::Bytes(format!("-{message}\r\n").into_bytes())
}

pub fn raw(s: &str) -> Reply {
    Reply::Bytes(s.as_bytes().to_vec())
}

/// `["subscribe", channel, count]`.
pub fn subscribe_ack(channel: &str, count: i64) -> Reply {
    Reply::Bytes(format!("*3\r\n$9\r\nsubscribe\r\n${}\r\n{channel}\r\n:{count}\r\n", channel.len()).into_bytes())
}

/// `["message", channel, payload]`.
pub fn message_frame(channel: &str, payload: &str) -> Vec<u8> {
    format!("*3\r\n$7\r\nmessage\r\n${}\r\n{channel}\r\n${}\r\n{payload}\r\n", channel.len(), payload.len()).into_bytes()
}

type Handler = dyn Fn(&[String]) -> Reply + Send + Sync;

/// Running mock server. Dropping it stops the accept loop.
pub struct MockHome {
    pub addr: SocketAddr,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    writers: Arc<Mutex<Vec<mpsc::UnboundedSender<Vec<u8>>>>>,
    task: JoinHandle<()>,
}

impl Drop for MockHome {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockHome {
    pub async fn start(handler: impl Fn(&[String]) -> Reply + Send + Sync + 'static) -> MockHome {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock home");
        let addr = listener.local_addr().expect("mock home addr");
        let handler: Arc<Handler> = Arc::new(handler);
        let commands: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let writers: Arc<Mutex<Vec<mpsc::UnboundedSender<Vec<u8>>>>> = Arc::default();
        let (cmds, ws) = (commands.clone(), writers.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (handler, cmds) = (handler.clone(), cmds.clone());
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                ws.lock().push(tx.clone());
                tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read);
                    // One writer task keeps replies and test pushes ordered on the socket.
                    let writer = tokio::spawn(async move {
                        while let Some(bytes) = rx.recv().await {
                            if write.write_all(&bytes).await.is_err() {
                                return;
                            }
                        }
                    });
                    while let Ok(args) = read_command(&mut reader).await {
                        let args: Vec<String> = args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
                        cmds.lock().push(args.clone());
                        match handler(&args) {
                            Reply::Bytes(bytes) => {
                                let _ = tx.send(bytes);
                            }
                            Reply::Silent => {}
                            Reply::Close => break,
                        }
                    }
                    // Dropping both halves closes the socket.
                    writer.abort();
                });
            }
        });
        MockHome { addr, commands, writers, task }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Every command received so far.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().clone()
    }

    pub fn count(&self, name: &str, key: Option<&str>) -> usize {
        self.commands()
            .iter()
            .filter(|c| {
                c.first().is_some_and(|n| n.eq_ignore_ascii_case(name))
                    && key.is_none_or(|k| c.get(1).is_some_and(|v| v == k))
            })
            .count()
    }

    /// Writes raw bytes to every open connection (pub/sub pushes).
    pub fn push_all(&self, bytes: &[u8]) {
        for w in self.writers.lock().iter() {
            let _ = w.send(bytes.to_vec());
        }
    }
}
