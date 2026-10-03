//! Redis-protocol usage output (Go: `internal/api/redis_queue_protocol.go`): a tiny RESP server
//! on the API port. After `AUTH <management key>` clients `SUBSCRIBE usage|errors` for a live
//! feed or `LPOP`/`RPOP usage [count]` to drain the retention buffer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use cpa_config::Config;
use cpa_home::queue::{self, Subscription};
use cpa_home::resp::{self, RespError};
use cpa_management::ManagementState;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

const CHANNEL_USAGE: &str = "usage";
const CHANNEL_ERRORS: &str = "errors";
const BANNED_PREFIX: &str = "IP banned due to too many failed attempts";

/// Everything a RESP connection needs from the rest of the server.
#[derive(Clone)]
pub struct RedisProtocol {
    /// Live config (Home mode disables the output).
    pub config: watch::Receiver<Arc<Config>>,
    /// Management key check and failed-attempt accounting shared with the HTTP API.
    pub management: ManagementState,
    /// Go: `managementRoutesEnabled`; connections close while it is off.
    pub routes_enabled: Arc<AtomicBool>,
}

/// Client address as the management gate sees it: canonical IP text and whether it is loopback.
fn resolve_remote_ip(peer: SocketAddr) -> (String, bool) {
    let host = peer.ip().to_canonical().to_string();
    let local = host == "127.0.0.1" || host == "::1";
    (host, local)
}

fn parse_auth_password(args: &[Bytes]) -> Option<&Bytes> {
    match args.len() {
        2 => Some(&args[1]),
        3 => Some(&args[2]),
        _ => None,
    }
}

fn parse_pop_count(args: &[Bytes]) -> Option<(i64, bool)> {
    match args.len() {
        2 => Some((1, false)),
        3 => {
            let text = String::from_utf8_lossy(&args[2]);
            Some((text.trim().parse::<i64>().unwrap_or(0), true))
        }
        _ => None,
    }
}

fn text(b: &Bytes) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn subscribe(channel: &str) -> Option<Subscription> {
    match channel.trim().to_lowercase().as_str() {
        CHANNEL_USAGE => Some(queue::subscribe_usage()),
        CHANNEL_ERRORS => Some(queue::subscribe_errors()),
        _ => None,
    }
}

fn pop_items(channel: &str, count: usize) -> Option<Vec<Vec<u8>>> {
    match channel.trim().to_lowercase().as_str() {
        CHANNEL_USAGE => Some(queue::pop_oldest(count)),
        _ => None,
    }
}

async fn flush<W: AsyncWrite + Unpin>(w: &mut W, out: &mut Vec<u8>) -> bool {
    let result = w.write_all(out).await;
    out.clear();
    let result = match result {
        Ok(()) => w.flush().await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        tracing::error!("redis protocol flush error: {e}");
        return false;
    }
    true
}

impl RedisProtocol {
    /// Serves one RESP connection until it ends (Go: `handleRedisConnection`).
    pub async fn handle<S>(&self, stream: S, peer: SocketAddr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (client_ip, local) = resolve_remote_ip(peer);
        let (read_half, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut out: Vec<u8> = Vec::new();

        if self.config.borrow().home.enabled {
            resp::write_error(&mut out, "ERR redis usage output disabled in home mode");
            let _ = flush(&mut writer, &mut out).await;
            return;
        }

        let mut authed = false;
        loop {
            if !self.routes_enabled.load(Ordering::SeqCst) {
                return;
            }
            let args = match resp::read_command(&mut reader).await {
                Ok(args) => args,
                Err(RespError::Eof) => return,
                Err(e) => {
                    resp::write_error(&mut out, &format!("ERR {e}"));
                    let _ = flush(&mut writer, &mut out).await;
                    return;
                }
            };
            if args.is_empty() {
                resp::write_error(&mut out, "ERR empty command");
                if !flush(&mut writer, &mut out).await {
                    return;
                }
                continue;
            }
            let cmd = text(&args[0]).trim().to_uppercase();

            if cmd != "AUTH" && !authed {
                match self.management.authenticate_key(&client_ip, local, b"").await {
                    Err((403, msg)) if msg.starts_with(BANNED_PREFIX) => {
                        resp::write_error(&mut out, &format!("ERR {msg}"));
                    }
                    _ => resp::write_error(&mut out, "NOAUTH Authentication required."),
                }
                if !flush(&mut writer, &mut out).await {
                    return;
                }
                continue;
            }

            match cmd.as_str() {
                "AUTH" => {
                    let Some(password) = parse_auth_password(&args) else {
                        if let Err((403, msg)) =
                            self.management.authenticate_key(&client_ip, local, b"").await
                            && msg.starts_with(BANNED_PREFIX)
                        {
                            resp::write_error(&mut out, &format!("ERR {msg}"));
                        } else {
                            resp::write_error(&mut out, "ERR wrong number of arguments for 'auth' command");
                        }
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    };
                    match self.management.authenticate_key(&client_ip, local, password).await {
                        Ok(()) => {
                            authed = true;
                            resp::write_simple(&mut out, "OK");
                        }
                        Err((_, msg)) => resp::write_error(&mut out, &format!("ERR {msg}")),
                    }
                    if !flush(&mut writer, &mut out).await {
                        return;
                    }
                }
                "SUBSCRIBE" => {
                    if args.len() != 2 {
                        resp::write_error(&mut out, "ERR wrong number of arguments for 'subscribe' command");
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    }
                    let channel = text(&args[1]).trim().to_string();
                    let Some(subscription) = subscribe(&channel) else {
                        resp::write_error(&mut out, &format!("ERR unsupported channel '{channel}'"));
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    };
                    resp::write_pubsub_subscribe(&mut out, &channel, 1);
                    if !flush(&mut writer, &mut out).await {
                        return;
                    }
                    stream_subscription(reader, writer, &channel, subscription).await;
                    return;
                }
                "LPOP" | "RPOP" => {
                    let lower = cmd.to_lowercase();
                    let Some((count, has_count)) = parse_pop_count(&args) else {
                        resp::write_error(&mut out, &format!("ERR wrong number of arguments for '{lower}' command"));
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    };
                    if count <= 0 {
                        resp::write_error(&mut out, "ERR value is not an integer or out of range");
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    }
                    let Some(items) = pop_items(&text(&args[1]), usize::try_from(count).unwrap_or(usize::MAX)) else {
                        resp::write_error(
                            &mut out,
                            &format!("ERR unsupported channel '{}'", text(&args[1]).trim()),
                        );
                        if !flush(&mut writer, &mut out).await {
                            return;
                        }
                        continue;
                    };
                    if has_count {
                        resp::write_bulk_array(&mut out, &items);
                    } else if let Some(first) = items.first() {
                        resp::write_bulk(&mut out, first);
                    } else {
                        resp::write_nil(&mut out);
                    }
                    if !flush(&mut writer, &mut out).await {
                        return;
                    }
                }
                _ => {
                    resp::write_error(&mut out, &format!("ERR unknown command '{}'", cmd.to_lowercase()));
                    if !flush(&mut writer, &mut out).await {
                        return;
                    }
                }
            }
        }
    }
}

/// A command read while subscribed, or the error that ended the read loop.
type SubscriptionCommand = Result<Vec<Bytes>, RespError>;

/// Pushes queue messages to the client and serves `PING`/`UNSUBSCRIBE`/`QUIT` until the client
/// leaves or the queue closes the subscription (Go: `streamRedisSubscription`).
async fn stream_subscription<R, W>(
    reader: BufReader<R>,
    mut writer: W,
    channel: &str,
    mut subscription: Subscription,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (tx, mut commands) = mpsc::channel::<SubscriptionCommand>(1);
    // Command reads must not be cancelled mid-frame, so they live in their own task; dropping
    // `commands` (on return) ends it at the next send.
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match resp::read_command(&mut reader).await {
                Err(RespError::Eof) => return,
                Ok(args) => {
                    if tx.send(Ok(args)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    let mut out: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            msg = subscription.recv() => {
                let Some(msg) = msg else { break };
                resp::write_pubsub_message(&mut out, channel, &msg);
                if !flush(&mut writer, &mut out).await {
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let keep_open = handle_subscription_command(&mut out, channel, command);
                if !flush(&mut writer, &mut out).await || !keep_open {
                    break;
                }
            }
        }
    }
    reader_task.abort();
}

fn handle_subscription_command(out: &mut Vec<u8>, channel: &str, command: SubscriptionCommand) -> bool {
    let args = match command {
        Ok(args) => args,
        Err(e) => {
            resp::write_error(out, &format!("ERR {e}"));
            return false;
        }
    };
    if args.is_empty() {
        resp::write_error(out, "ERR empty command");
        return true;
    }
    let cmd = text(&args[0]).trim().to_uppercase();
    match cmd.as_str() {
        "PING" => {
            // Without an argument Go passes a nil payload, which renders as a nil bulk string.
            resp::write_array_header(out, 2);
            resp::write_bulk(out, b"pong");
            match args.get(1) {
                Some(p) => resp::write_bulk(out, p),
                None => resp::write_nil(out),
            }
            true
        }
        "UNSUBSCRIBE" => {
            resp::write_pubsub_unsubscribe(out, channel, 0);
            false
        }
        "QUIT" => {
            resp::write_simple(out, "OK");
            false
        }
        _ => {
            resp::write_error(out, &format!("ERR unknown command '{}'", cmd.to_lowercase()));
            true
        }
    }
}
