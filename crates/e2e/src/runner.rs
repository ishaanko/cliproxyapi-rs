//! Runs one scenario end to end: script the mock, start the server, send the steps, collect
//! the client observations and the upstream log, normalize everything.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client::{Body, Client, Observed, ObsBody, Step};
use crate::config::{ConfigSpec, Layout};
use crate::mock::LoggedRequest;
use crate::normalize::{Normalizer, sort_model_listing};
use crate::scenario::{Capture, RUST_ONLY_ORIGIN, Scenario, StepCapture, UpstreamCapture};
use crate::server::ServerProc;

const WS_SETTLE_MS: u64 = 500;
const LOG_SETTLE_MS: u64 = 400;

pub struct RunOpts {
    pub server_bin: PathBuf,
    pub mock_port: u16,
    pub server_port: u16,
    pub work_dir: PathBuf,
    pub layout: Layout,
}

pub async fn run_scenario(opts: &RunOpts, s: &Scenario) -> Result<Capture> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let mock = format!("http://127.0.0.1:{}/__control", opts.mock_port);
    http.post(format!("{mock}/script"))
        .json(&s.script)
        .send()
        .await?
        .error_for_status()
        .context("install mock script")?;

    let mut spec = ConfigSpec::baseline(opts.mock_port);
    (s.profile)(&mut spec);
    let dir = opts.work_dir.join(&s.id);
    let server = ServerProc::start(&opts.server_bin, &spec, opts.layout, &dir, opts.server_port).await?;
    let client = Client::new(server.port)?;
    // The Go reference applies a second config reload shortly after startup that replaces the
    // executors and closes live websocket sessions; give it time before opening sockets.
    let uses_ws = s.steps.iter().any(|st| matches!(st, Step::Ws(_)));
    if uses_ws {
        tokio::time::sleep(std::time::Duration::from_millis(WS_SETTLE_MS)).await;
    }

    let mut observed: Vec<(Value, Observed)> = vec![];
    for step in &s.steps {
        if let Step::Pause(ms) = step {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            continue;
        }
        let resp = match client.run(&with_work_dir(step, &dir)).await {
            Ok(o) => o,
            // Connection-level failures are part of the observable behavior.
            Err(e) => Observed { status: 0, headers: Default::default(), body: ObsBody::Text { value: format!("<client error: {}>", error_kind(&e)) } },
        };
        observed.push((step_summary(step), resp));
    }
    if uses_ws {
        // After the client disconnects the server may still be failing over upstream.
        tokio::time::sleep(std::time::Duration::from_millis(WS_SETTLE_MS)).await;
    }
    let log: Vec<LoggedRequest> = http.get(format!("{mock}/log")).send().await?.json().await.context("read mock log")?;
    let server_port = server.port;
    let log_dir = server.dir.join("logs");
    if s.capture_logs {
        // Request logs are written after the response body completes.
        tokio::time::sleep(std::time::Duration::from_millis(LOG_SETTLE_MS)).await;
    }
    server.stop().await;

    let mut n = Normalizer::new(opts.mock_port, server_port, &opts.work_dir.to_string_lossy());
    for (name, content) in &spec.auth_files {
        let auth_type = serde_json::from_str::<Value>(content).ok().and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
        n.mask_auth_file(&auth_type, &dir.join("auth").join(name).to_string_lossy(), name);
    }
    let steps = observed
        .into_iter()
        .map(|(mut request, mut response)| {
            n.value(&mut request);
            response.headers = n.headers(&response.headers);
            // Cooldown remaining time (and its jittered Retry-After) depends on the clock.
            if matches!(&response.body, ObsBody::Json { value, .. } if value.to_string().contains("model_cooldown"))
                && let Some(h) = response.headers.get_mut("retry-after")
            {
                *h = "<cooldown>".into();
            }
            normalize_body(&mut n, &mut response.body);
            if is_model_list(&request)
                && let ObsBody::Json { value, .. } = &mut response.body
            {
                sort_model_listing(value);
            }
            StepCapture { request, response }
        })
        .collect();
    let upstream = log
        .into_iter()
        .map(|l| {
            let mut body = l.body;
            n.value(&mut body);
            let mut ws_frames = l.ws_frames;
            ws_frames.iter_mut().for_each(|f| n.value(f));
            UpstreamCapture {
                family: l.family,
                method: l.method,
                path: n.string(&l.path),
                query: n.string(&l.query),
                credential: l.credential,
                headers: n.request_headers(&l.headers),
                body,
                ws_frames,
                closed_early: l.closed_early,
            }
        })
        .collect();
    let request_logs = if s.capture_logs { read_request_logs(&log_dir).iter().map(|t| n.request_log(t)).collect() } else { vec![] };
    let origin = s.rust_only.then(|| RUST_ONLY_ORIGIN.to_string());
    Ok(Capture { id: s.id.clone(), desc: s.desc.clone(), origin, steps, upstream, request_logs, volatile: vec![] })
}

/// Request-log files under `dir`, oldest first (the application log `main.log` is not one).
fn read_request_logs(dir: &std::path::Path) -> Vec<String> {
    let mut files: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".log") && !e.file_name().to_string_lossy().starts_with("main"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, std::fs::read_to_string(e.path()).ok()?)))
        .collect();
    files.sort_by_key(|f| f.0);
    files.into_iter().map(|f| f.1).collect()
}

/// The model-list endpoints return Go-map-ordered arrays; their order carries no meaning.
fn is_model_list(request: &Value) -> bool {
    let path = request["path"].as_str().unwrap_or_default().split('?').next().unwrap_or_default();
    request["method"] == "GET" && matches!(path, "/v1/models" | "/v1beta/models")
}

fn error_kind(e: &anyhow::Error) -> &'static str {
    let text = format!("{e:#}");
    if text.contains("timed out") {
        "timeout"
    } else if text.contains("connection closed") || text.contains("reset") || text.contains("eof") || text.contains("error sending request") {
        "connection"
    } else {
        "other"
    }
}

fn normalize_body(n: &mut Normalizer, body: &mut ObsBody) {
    match body {
        ObsBody::Empty => {}
        ObsBody::Json { value, .. } => n.value(value),
        ObsBody::Text { value } => *value = n.string(value),
        ObsBody::Sse { events } => events.iter_mut().for_each(|e| n.value(e)),
        ObsBody::Ws { frames, close } => {
            frames.iter_mut().for_each(|f| n.value(f));
            if let Some(c) = close {
                *c = n.string(c);
            }
        }
    }
}

/// Compact description of a client request, stored in the golden for readability.
fn step_summary(step: &Step) -> Value {
    match step {
        Step::Http(r) => {
            let body = match &r.body {
                Body::None => Value::Null,
                Body::Json(v) => v.clone(),
                Body::Text(t) | Body::Typed(_, t) => Value::String(t.clone()),
                Body::Raw { content_type, bytes } => json!({"content_type": content_type, "text": String::from_utf8_lossy(bytes)}),
            };
            json!({"method": r.method, "path": r.path, "auth": format!("{:?}", r.auth), "headers": r.headers, "body": body})
        }
        Step::Ws(r) => json!({"method": "WS", "path": r.path, "auth": format!("{:?}", r.auth), "headers": r.headers, "messages": r.messages}),
        Step::Resp(r) => json!({"method": "RESP", "acts": format!("{:?}", r.acts)}),
        Step::Pause(ms) => json!({"pause_ms": ms}),
    }
}

/// Substitutes `{{AUTH_DIR}}` in typed request bodies with the scenario's auth directory, for
/// whole-config replacements that must keep the server's own credential directory.
fn with_work_dir(step: &Step, dir: &std::path::Path) -> Step {
    const TOKEN: &str = "{{AUTH_DIR}}";
    match step {
        Step::Http(req) => {
            if let Body::Typed(content_type, text) = &req.body
                && text.contains(TOKEN)
            {
                let mut req = req.clone();
                req.body = Body::Typed(content_type, text.replace(TOKEN, &dir.join("auth").to_string_lossy()));
                return Step::Http(req);
            }
            step.clone()
        }
        other => other.clone(),
    }
}
