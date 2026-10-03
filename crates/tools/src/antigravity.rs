//! `fetch_antigravity_models`: fetches the Antigravity model list with stored credentials and
//! writes it to a JSON file (Go: cmd/fetch_antigravity_models).

use std::path::Path;
use std::time::Duration;

use cpa_auth::types::Auth;
use cpa_json::J;
use serde::Serialize;

use crate::common::{go_marshal, init_logging, list_auths, meta_string, setup};
use crate::flags::{self, FlagDef, Kind};

const BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const SANDBOX_BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const BASE_URL_PROD: &str = "https://cloudcode-pa.googleapis.com";
const MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const MAX_FETCH_ATTEMPTS_PER_ENDPOINT: usize = 2;

const FLAGS: &[FlagDef] = &[
    FlagDef { name: "auths-dir", kind: Kind::Str, default: "", usage: "Directory containing auth JSON files (overrides config auth-dir)" },
    FlagDef { name: "config", kind: Kind::Str, default: "", usage: "Configure File Path" },
    FlagDef { name: "output", kind: Kind::Str, default: "antigravity_models.json", usage: "Output JSON file path" },
    FlagDef { name: "pretty", kind: Kind::Bool, default: "true", usage: "Pretty-print the output JSON" },
];

/// The fetched model list with fetch metadata. `models` is `null` when nothing was fetched.
#[derive(Debug, Serialize)]
struct ModelOutput {
    models: Option<Vec<ModelEntry>>,
}

/// The fields kept for static model definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub owned_by: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub display_name: String,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub context_length: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub max_completion_tokens: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

pub fn default_fetch_base_urls() -> Vec<String> {
    vec![BASE_URL_DAILY.into(), BASE_URL_PROD.into(), SANDBOX_BASE_URL_DAILY.into()]
}

/// Program entry; returns the process exit code.
pub async fn run(args: Vec<String>) -> i32 {
    init_logging();
    let parsed = match flags::parse("fetch_antigravity_models", FLAGS, &args) {
        Ok(p) => p,
        Err(flags::Exit(code)) => return code,
    };
    match run_inner(&parsed).await {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

async fn run_inner(parsed: &flags::Flags) -> Result<(), String> {
    let mut env = setup(
        &parsed.string("auths-dir"),
        parsed.was_set("auths-dir"),
        &parsed.string("config"),
        &parsed.string("output"),
    )?;
    if std::fs::metadata(&env.auths_dir).is_err() && !env.auths_dir_overridden {
        let local = env.wd.join("auths");
        if local.is_dir() {
            env.auths_dir = local.to_string_lossy().into_owned();
        }
    }
    if let Ok(real) = std::fs::canonicalize(&env.auths_dir) {
        env.auths_dir = real.to_string_lossy().into_owned();
    }

    println!("Scanning auth files in: {}", env.auths_dir);
    let auths = list_auths(&env.auths_dir)?;
    if auths.is_empty() {
        return Err(format!("error: no auth files found in {}", env.auths_dir));
    }

    let is_ag = |a: &Auth| !a.disabled && a.provider.trim().eq_ignore_ascii_case("antigravity");
    let mut ag_auths: Vec<&Auth> = auths.iter().filter(|a| is_ag(a) && !a.id.contains(".back")).collect();
    if ag_auths.is_empty() {
        ag_auths = auths.iter().filter(|a| is_ag(a)).collect();
    }
    if ag_auths.is_empty() {
        return Err(format!("error: no enabled antigravity auth found in {}", env.auths_dir));
    }

    let mut models: Vec<ModelEntry> = Vec::new();
    for chosen in ag_auths {
        println!("Using auth: id={} label={}", chosen.id, chosen.label);
        println!("Fetching Antigravity model list from upstream...");
        models = tokio::time::timeout(Duration::from_secs(30), fetch_models(chosen))
            .await
            .unwrap_or_default();
        if !models.is_empty() {
            println!("Fetched {} models.", models.len());
            break;
        }
        eprintln!("warning: no models returned from this auth, trying next...");
    }
    if models.is_empty() {
        eprintln!("warning: no models returned from any auth (API may be unavailable or tokens expired)");
    }

    let out = ModelOutput { models: (!models.is_empty()).then_some(models) };
    let raw = go_marshal(&out, parsed.boolean("pretty")).map_err(|e| format!("error: failed to marshal JSON: {e}"))?;
    write_output(&env.output_path, &raw)?;
    println!("Model list saved to: {}", env.output_path.display());
    Ok(())
}

fn write_output(path: &Path, raw: &[u8]) -> Result<(), String> {
    std::fs::write(path, raw).map_err(|e| format!("error: failed to write output file {}: {e}", path.display()))
}

async fn fetch_models(auth: &Auth) -> Vec<ModelEntry> {
    fetch_models_from_base_urls(auth, &default_fetch_base_urls(), None).await
}

/// Tries each base URL in turn (two attempts each) and returns the models of the first usable
/// response; empty when none worked. `client` replaces the proxy-aware default (tests).
pub async fn fetch_models_from_base_urls(auth: &Auth, base_urls: &[String], client: Option<&reqwest::Client>) -> Vec<ModelEntry> {
    let access_token = meta_string(&auth.metadata, "access_token");
    if access_token.is_empty() {
        eprintln!("error: no access token found in auth");
        return Vec::new();
    }

    let default_client;
    let client = match client {
        Some(c) => c,
        None => {
            default_client = cpa_auth::http::build_client(&auth.proxy_url, Some(Duration::from_secs(30)))
                .unwrap_or_else(|_| reqwest::Client::new());
            &default_client
        }
    };

    for base_url in base_urls {
        let models_url = format!("{base_url}{MODELS_PATH}");
        let project = auth.metadata.get("project_id").and_then(|v| v.as_str()).map(str::trim).filter(|p| !p.is_empty());
        // The payload is formatted without escaping, exactly like the Go tool.
        let payload = match project {
            Some(pid) => format!(r#"{{"project": "{pid}"}}"#),
            None => "{}".to_string(),
        };

        for _attempt in 1..=MAX_FETCH_ATTEMPTS_PER_ENDPOINT {
            let request = client
                .post(&models_url)
                .header("Connection", "close")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {access_token}"))
                .header("User-Agent", cpa_core::misc::antigravity_user_agent())
                .body(payload.clone());
            let Ok(response) = request.send().await else { continue };
            let status = response.status();
            let Ok(body) = response.bytes().await else { continue };
            if !status.is_success() {
                continue;
            }

            let doc = cpa_json::parse(&body);
            let result = doc.g("models");
            if !result.exists() {
                continue;
            }
            return parse_models(&result);
        }
    }
    Vec::new()
}

/// The `models` object of the response as entries, in document order (Go ranges over a map, so
/// its order is random).
fn parse_models(result: &cpa_json::Res<'_>) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    for (original_name, data) in result.entries() {
        let model_id = original_name.trim();
        if model_id.is_empty() {
            continue;
        }
        // Skip internal/experimental models
        if matches!(
            model_id,
            "chat_20706" | "chat_23310" | "tab_flash_lite_preview" | "tab_jump_flash_lite_preview" | "gemini-2.5-flash-thinking" | "gemini-2.5-pro"
        ) {
            continue;
        }
        let mut display_name = data.get("displayName").str();
        if display_name.is_empty() {
            display_name = model_id.to_string();
        }
        let max_tok = data.get("maxTokens").int();
        let max_out = data.get("maxOutputTokens").int();
        models.push(ModelEntry {
            id: model_id.to_string(),
            object: "model".into(),
            owned_by: "antigravity".into(),
            kind: "antigravity".into(),
            display_name: display_name.clone(),
            name: model_id.to_string(),
            description: display_name,
            context_length: max_tok.max(0),
            max_completion_tokens: max_out.max(0),
        });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn auth(meta: serde_json::Value) -> Auth {
        let mut a = Auth::new("a", "antigravity");
        a.metadata = meta.as_object().cloned().unwrap_or_default();
        a
    }

    #[test]
    fn default_base_urls() {
        assert_eq!(default_fetch_base_urls(), [BASE_URL_DAILY, BASE_URL_PROD, SANDBOX_BASE_URL_DAILY]);
    }

    #[tokio::test]
    async fn retries_within_an_endpoint_before_falling_back() {
        let calls1 = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::new(AtomicUsize::new(0));
        let c1 = calls1.clone();
        let server1 = serve(Router::new().route(
            MODELS_PATH,
            post(move || {
                let n = c1.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n == 1 {
                        return (StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"temporary server error"}"#.to_string());
                    }
                    (
                        StatusCode::OK,
                        r#"{"models":{"gemini-3.6-flash":{"displayName":"Gemini 3.6 Flash","maxTokens":1048576,"maxOutputTokens":8192}}}"#.to_string(),
                    )
                }
            }),
        ))
        .await;
        let c2 = calls2.clone();
        let server2 = serve(Router::new().route(
            MODELS_PATH,
            post(move || {
                c2.fetch_add(1, Ordering::SeqCst);
                async { r#"{"models":{"gemini-1.5-pro":{"displayName":"Gemini 1.5 Pro"}}}"# }
            }),
        ))
        .await;

        let a = auth(serde_json::json!({"access_token": "test-token", "project_id": "test-project"}));
        let client = reqwest::Client::new();
        let models = fetch_models_from_base_urls(&a, &[server1, server2], Some(&client)).await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.6-flash");
        assert_eq!((models[0].context_length, models[0].max_completion_tokens), (1048576, 8192));
        assert_eq!(calls1.load(Ordering::SeqCst), 2);
        assert_eq!(calls2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn falls_back_after_two_failed_attempts() {
        let calls1 = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::new(AtomicUsize::new(0));
        let c1 = calls1.clone();
        let server1 = serve(Router::new().route(
            MODELS_PATH,
            post(move || {
                c1.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"unavailable"}"#) }
            }),
        ))
        .await;
        let c2 = calls2.clone();
        let server2 = serve(Router::new().route(
            MODELS_PATH,
            post(move || {
                c2.fetch_add(1, Ordering::SeqCst);
                async { r#"{"models":{"gemini-2.5-flash":{"displayName":"Gemini 2.5 Flash"}}}"# }
            }),
        ))
        .await;

        let a = auth(serde_json::json!({"access_token": "test-token"}));
        let client = reqwest::Client::new();
        let models = fetch_models_from_base_urls(&a, &[server1, server2], Some(&client)).await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-2.5-flash");
        assert_eq!(calls1.load(Ordering::SeqCst), 2);
        assert_eq!(calls2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn model_entries_skip_internal_models_and_default_names() {
        let doc = cpa_json::parse(br#"{"models":{"gemini-2.5-pro":{},"chat_20706":{},"x":{"maxTokens":0},"y":{"displayName":"Why","maxTokens":"12"}}}"#);
        let out = parse_models(&doc.g("models"));
        assert_eq!(out.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(out[0].display_name, "x");
        assert_eq!(out[0].context_length, 0);
        assert_eq!((out[1].display_name.as_str(), out[1].context_length), ("Why", 12));
        // omitempty drops the zero token limits
        let json = go_marshal(&out[0], false).unwrap();
        assert!(!String::from_utf8(json).unwrap().contains("context_length"));
    }

    #[test]
    fn empty_output_marshals_models_as_null() {
        let out = ModelOutput { models: None };
        assert_eq!(String::from_utf8(go_marshal(&out, true).unwrap()).unwrap(), "{\n  \"models\": null\n}");
    }
}
