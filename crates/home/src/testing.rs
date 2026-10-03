//! A scriptable mock Home RESP server for tests (mirrors the command-log servers in the Go
//! tests). Not used in production code.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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

type KvEntries = std::collections::BTreeMap<String, (Vec<u8>, Option<Duration>)>;

/// In-memory KV behind a [`MockHome`]: `GET`, `SET` (`EX`/`PX`/`NX`/`XX`), `DEL`, `EXPIRE`, `TTL`
/// and `CAS`, enough for the cache and helper tests of the crates built on the Home client.
/// Values are stored as written; expiry is recorded (see [`FakeKv::ttl`]) but never enforced.
#[derive(Clone, Default)]
pub struct FakeKv {
    entries: Arc<Mutex<KvEntries>>,
}

impl FakeKv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().get(key).map(|(v, _)| v.clone())
    }

    /// The expiry last set for `key` (`None` when absent or without expiry).
    pub fn ttl(&self, key: &str) -> Option<Duration> {
        self.entries.lock().get(key).and_then(|(_, ttl)| *ttl)
    }

    pub fn put(&self, key: &str, value: impl Into<Vec<u8>>) {
        self.entries.lock().insert(key.to_string(), (value.into(), None));
    }

    pub fn keys(&self) -> Vec<String> {
        self.entries.lock().keys().cloned().collect()
    }

    /// Handler for [`MockHome::start`]: `MockHome::start(move |a| kv.handle(a))`.
    pub fn handle(&self, args: &[String]) -> Reply {
        let name = args.first().map(|a| a.to_ascii_uppercase()).unwrap_or_default();
        let mut entries = self.entries.lock();
        match (name.as_str(), &args[1.min(args.len())..]) {
            ("GET", [key]) => entries.get(key).map_or_else(nil, |(v, _)| bulk(v)),
            ("SET", [key, value, rest @ ..]) => {
                let (mut ttl, mut nx, mut xx) = (None, false, false);
                let mut it = rest.iter();
                while let Some(opt) = it.next() {
                    match opt.to_ascii_uppercase().as_str() {
                        "EX" => ttl = it.next().and_then(|s| s.parse().ok()).map(Duration::from_secs),
                        "PX" => ttl = it.next().and_then(|s| s.parse().ok()).map(Duration::from_millis),
                        "NX" => nx = true,
                        "XX" => xx = true,
                        _ => return err("ERR syntax error"),
                    }
                }
                let exists = entries.contains_key(key);
                if (nx && exists) || (xx && !exists) {
                    return nil();
                }
                entries.insert(key.clone(), (value.clone().into_bytes(), ttl));
                ok()
            }
            ("DEL", keys) => int(keys.iter().filter(|k| entries.remove(*k).is_some()).count() as i64),
            ("EXPIRE", [key, secs]) => match (entries.get_mut(key), secs.parse::<u64>()) {
                (Some(entry), Ok(secs)) => {
                    entry.1 = Some(Duration::from_secs(secs));
                    int(1)
                }
                _ => int(0),
            },
            ("TTL", [key]) => match entries.get(key) {
                None => int(-2),
                Some((_, None)) => int(-1),
                Some((_, Some(ttl))) => int(ttl.as_secs() as i64),
            },
            // CAS key expected-exists expected new [PX ms]
            ("CAS", [key, exists, expected, new, rest @ ..]) => {
                let current = entries.get(key).map(|(v, _)| v.as_slice());
                let matches = match (exists.as_str(), current) {
                    ("0", None) => true,
                    ("1", Some(current)) => current == expected.as_bytes(),
                    _ => false,
                };
                if !matches {
                    return int(0);
                }
                let ttl = match rest {
                    [px, ms] if px.eq_ignore_ascii_case("PX") => ms.parse().ok().map(Duration::from_millis),
                    _ => None,
                };
                entries.insert(key.clone(), (new.clone().into_bytes(), ttl));
                int(1)
            }
            ("PING", _) => Reply::Bytes(b"+PONG\r\n".to_vec()),
            _ => err("ERR unknown command"),
        }
    }
}

/// Starts a mock Home backed by a fresh [`FakeKv`] and installs a client for it as the
/// process-wide Home client (`kv::set_current`) with the heartbeat forced on. Tests using this
/// must live in their own integration-test binary and run serially.
pub async fn install_fake_home() -> (MockHome, FakeKv, Arc<crate::Client>) {
    let kv = FakeKv::new();
    let handler_kv = kv.clone();
    let mock = MockHome::start(move |args| handler_kv.handle(args)).await;
    let cfg = cpa_config::HomeConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port: i64::from(mock.port()),
        ..Default::default()
    };
    let client = Arc::new(crate::Client::new(cfg));
    client.set_test_operation_timeout(Duration::from_secs(2));
    client.set_heartbeat_ok_for_tests(true);
    crate::kv::set_current(client.clone());
    (mock, kv, client)
}
