//! `fetch_codex_models`: fetches the Codex client model catalog with stored credentials and saves
//! the upstream payload to a JSON file (Go: cmd/fetch_codex_models).

use std::time::Duration;

use cpa_auth::codex::{CodexAuth, apply_refresh_to_auth};
use cpa_auth::store::{FileTokenStore, SaveOptions, Store};
use cpa_auth::types::Auth;
use serde_json::Value;

use crate::common::{init_logging, list_auths, meta_string_trimmed, pretty_json, setup};
use crate::flags::{self, FlagDef, Kind};

const MODELS_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const MODELS_PATH: &str = "/models";
const DEFAULT_CLIENT_VERSION: &str = "0.159.0";
const DEFAULT_USER_AGENT: &str = "codex_cli_rs/0.159.0 (Mac OS 26.3.1; arm64) iTerm.app/3.6.9";
const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
const ACCESS_TOKEN_REFRESH_LEEWAY: Duration = Duration::from_secs(30);

const FLAGS: &[FlagDef] = &[
    FlagDef { name: "auths-dir", kind: Kind::Str, default: "", usage: "Directory containing auth JSON files (overrides config auth-dir)" },
    FlagDef { name: "config", kind: Kind::Str, default: "", usage: "Configure File Path" },
    FlagDef { name: "output", kind: Kind::Str, default: "codex_client_models.json", usage: "Output JSON file path" },
    FlagDef { name: "client-version", kind: Kind::Str, default: DEFAULT_CLIENT_VERSION, usage: "Codex client_version query value" },
    FlagDef { name: "pretty", kind: Kind::Bool, default: "true", usage: "Pretty-print the output JSON" },
];

/// Program entry; returns the process exit code.
pub async fn run(args: Vec<String>) -> i32 {
    init_logging();
    let parsed = match flags::parse("fetch_codex_models", FLAGS, &args) {
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
    let env = setup(
        &parsed.string("auths-dir"),
        parsed.was_set("auths-dir"),
        &parsed.string("config"),
        &parsed.string("output"),
    )?;

    println!("Scanning auth files in: {}", env.auths_dir);
    let store = FileTokenStore::with_dir(&env.auths_dir);
    let mut auths = list_auths(&env.auths_dir)?;
    if auths.is_empty() {
        return Err(format!("error: no auth files found in {}", env.auths_dir));
    }

    let Some(index) = find_codex_auth(&auths) else {
        return Err(format!("error: no enabled codex auth found in {}", env.auths_dir));
    };
    let chosen = &mut auths[index];
    println!("Using auth: id={} label={}", chosen.id, chosen.label);

    let (access_token, refreshed) = ensure_access_token(&store, chosen)
        .await
        .map_err(|e| format!("error: failed to prepare codex access token: {e}"))?;
    if refreshed {
        println!("Refreshed Codex access token.");
    }

    println!("Fetching Codex model list from upstream...");
    let (mut raw, count) = fetch_models(chosen, &access_token, &parsed.string("client-version"), MODELS_BASE_URL)
        .await
        .map_err(|e| format!("error: failed to fetch codex models: {e}"))?;
    println!("Fetched {count} models.");

    if parsed.boolean("pretty") {
        raw = pretty_json(&raw).map_err(|e| format!("error: failed to format JSON: {e}"))?;
    }
    std::fs::write(&env.output_path, &raw)
        .map_err(|e| format!("error: failed to write output file {}: {e}", env.output_path.display()))?;
    println!("Model list saved to: {}", env.output_path.display());
    Ok(())
}

/// The first enabled codex credential that has an access or refresh token.
pub fn find_codex_auth(auths: &[Auth]) -> Option<usize> {
    auths.iter().position(|auth| {
        !auth.disabled
            && auth.provider.trim().eq_ignore_ascii_case("codex")
            && !(meta_string_trimmed(&auth.metadata, "access_token").is_empty()
                && meta_string_trimmed(&auth.metadata, "refresh_token").is_empty())
    })
}

/// Returns a usable access token and whether it was refreshed (the refreshed credential is saved
/// back to the store).
async fn ensure_access_token(store: &FileTokenStore, auth: &mut Auth) -> Result<(String, bool), String> {
    let access_token = meta_string_trimmed(&auth.metadata, "access_token");
    if !access_token.is_empty() {
        let leeway = chrono::Duration::from_std(ACCESS_TOKEN_REFRESH_LEEWAY).unwrap_or_default();
        match auth.expiration_time() {
            Some(expires_at) if chrono::Utc::now() + leeway >= expires_at => {}
            _ => return Ok((access_token, false)),
        }
    }

    let refresh_token = meta_string_trimmed(&auth.metadata, "refresh_token");
    if refresh_token.is_empty() {
        if !access_token.is_empty() {
            return Ok((access_token, false));
        }
        return Err("missing access_token and refresh_token".into());
    }

    let svc = CodexAuth::new(&auth.proxy_url).map_err(|e| e.to_string())?;
    let token_data = svc.refresh_tokens_with_retry(&refresh_token, 3).await.map_err(|e| e.to_string())?;
    if token_data.access_token.trim().is_empty() {
        return Err("refresh response did not include access_token".into());
    }
    apply_refresh_to_auth(auth, &token_data);
    store
        .save(auth, SaveOptions::default())
        .map_err(|e| format!("failed to save refreshed auth: {e}"))?;
    Ok((token_data.access_token, true))
}

/// GETs the model catalog from `<base_url>/models` and returns the raw body and the model count.
pub async fn fetch_models(auth: &Auth, access_token: &str, client_version: &str, base_url: &str) -> Result<(Vec<u8>, usize), String> {
    let models_url = codex_models_url(client_version, base_url);

    let mut headers = reqwest::header::HeaderMap::new();
    let mut put = |name: &'static str, value: &str| {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
            headers.insert(reqwest::header::HeaderName::from_static(name), v);
        }
    };
    put("connection", "close");
    put("accept", "application/json");
    put("authorization", &format!("Bearer {access_token}"));
    put("originator", DEFAULT_ORIGINATOR);
    put("user-agent", DEFAULT_USER_AGENT);
    let account_id = meta_string_trimmed(&auth.metadata, "account_id");
    if !account_id.is_empty() {
        put("chatgpt-account-id", &account_id);
    }
    let attrs = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    cpa_core::util::apply_custom_headers_from_attrs(&mut headers, &attrs, None, None);

    let client = cpa_auth::http::build_client(&auth.proxy_url, None).unwrap_or_else(|_| reqwest::Client::new());
    let response = client.get(&models_url).headers(headers).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "models request failed with status {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&body).trim()
        ));
    }
    let count = count_models(&body)?;
    Ok((body.to_vec(), count))
}

/// `QueryEscape`: unreserved bytes stay, space becomes `+`, everything else is `%XX`.
fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `<base_url>/models`, with `?client_version=` when a version is given.
pub fn codex_models_url(client_version: &str, base_url: &str) -> String {
    let url = format!("{base_url}{MODELS_PATH}");
    match client_version.trim() {
        "" => url,
        version => format!("{url}?client_version={}", query_escape(version)),
    }
}

/// Number of entries in the payload's `models` array. Deliberately loose: the dump is the
/// upstream payload, strict catalog validation belongs to `validate_codex_models`.
pub fn count_models(raw: &[u8]) -> Result<usize, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| format!("failed to parse response JSON: {e}"))?;
    let models = match &value {
        Value::Object(map) => map.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case("models")).map(|(_, v)| v),
        Value::Null => None,
        _ => {
            return Err("failed to parse response JSON: json: cannot unmarshal into Go value of type struct".into());
        }
    };
    match models {
        Some(Value::Array(items)) => Ok(items.len()),
        None | Some(Value::Null) => Err("response JSON does not contain models array".into()),
        Some(_) => Err("failed to parse response JSON: json: cannot unmarshal into Go struct field .models of type []json.RawMessage".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::HeaderMap;
    use axum::routing::get;

    #[test]
    fn models_url_carries_the_trimmed_client_version() {
        assert_eq!(
            codex_models_url(" 0.144.1 ", MODELS_BASE_URL),
            "https://chatgpt.com/backend-api/codex/models?client_version=0.144.1"
        );
        assert_eq!(codex_models_url("  ", MODELS_BASE_URL), "https://chatgpt.com/backend-api/codex/models");
        assert_eq!(codex_models_url("a b~*", "http://h"), "http://h/models?client_version=a+b~%2A");
    }

    #[test]
    fn count_models_is_loose_about_entries() {
        assert_eq!(count_models(br#"{"models":[{"slug":"a"},{"slug":"b"}]}"#).unwrap(), 2);
        // Upstream dumps may omit CPA catalog-required fields; counting must still work.
        assert_eq!(count_models(br#"{"models":[{"slug":"gpt-5.6-sol"}]}"#).unwrap(), 1);
        assert_eq!(count_models(br#"{"models":[]}"#).unwrap(), 0);
        assert!(count_models(br#"{"models":"#).is_err());
        assert!(count_models(b"{}").unwrap_err().contains("does not contain models array"));
        assert!(count_models(br#"{"models":null}"#).is_err());
    }

    #[test]
    fn picks_the_first_enabled_codex_auth_with_a_token() {
        let mut no_token = Auth::new("a", "codex");
        no_token.metadata = serde_json::json!({"access_token": "  "}).as_object().cloned().unwrap_or_default();
        let mut disabled = Auth::new("b", "codex");
        disabled.disabled = true;
        disabled.metadata = serde_json::json!({"access_token": "t"}).as_object().cloned().unwrap_or_default();
        let other = Auth::new("c", "claude");
        let mut good = Auth::new("d", " Codex ");
        good.metadata = serde_json::json!({"refresh_token": "r"}).as_object().cloned().unwrap_or_default();
        assert_eq!(find_codex_auth(&[no_token, disabled, other, good]), Some(3));
        assert_eq!(find_codex_auth(&[]), None);
    }

    #[tokio::test]
    async fn fetch_sends_codex_headers_and_reports_upstream_errors() {
        let app = Router::new()
            .route(
                "/models",
                get(|headers: HeaderMap| async move {
                    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    if h("authorization") != "Bearer tok" || h("chatgpt-account-id") != "acct" || h("originator") != "codex_cli_rs" {
                        return (axum::http::StatusCode::UNAUTHORIZED, " nope \n".to_string());
                    }
                    (axum::http::StatusCode::OK, r#"{"models":[{"slug":"x"}]}"#.to_string())
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut auth = Auth::new("a", "codex");
        auth.proxy_url = "direct".into();
        auth.metadata = serde_json::json!({"account_id": " acct "}).as_object().cloned().unwrap_or_default();
        let (raw, count) = fetch_models(&auth, "tok", "1.2.3", &base).await.unwrap();
        assert_eq!((raw.as_slice(), count), (br#"{"models":[{"slug":"x"}]}"#.as_slice(), 1));

        let err = fetch_models(&auth, "bad", "", &base).await.unwrap_err();
        assert_eq!(err, "models request failed with status 401: nope");
    }
}
