# CLIProxyAPI HTTP API, handlers, logging and CLI: Rust port survey

Source root: `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI` (Go module `github.com/router-for-me/CLIProxyAPI/v8`, gin + net/http). Paths below are relative to it. Behavior only; translators, executors, auth manager and config schema are covered by sibling surveys.

Contents: 1 server and middleware, 2 route table, 3 client auth, 4 shared handler pipeline (model routing, headers, body, errors, keepalive), 5 per-dialect behavior (OpenAI, Responses, Responses websocket, Claude, Gemini, images/videos), 6 realtime/live, 7 misc listeners (Redis protocol, `/v1/ws`, keep-alive), 8 amp, 9 logging, 10 management API, 11 CLI.

Notable: there is NO amp module in this tree (the `ampcode` config key is only stripped during v8 migration, `internal/config/config_yaml.go:879`). No `/api/provider/*` routes exist.

---

## 1. Server, listener and middleware (internal/api)

### 1.1 Listener and protocol multiplexer
- `Server.Start` does `net.Listen("tcp", host:port)` (default port 8317 per `resolveManagementBaseURL`; host may be empty = all interfaces). If `tls.enable`, loads `tls.cert`/`tls.key` (error if either empty), `NextProtos = ["h2","http/1.1"]`, wraps with `tls.NewListener`, HTTP/2 enabled via `http2.ConfigureServer`.
- The raw listener is NOT given to `http.Server` directly. `acceptMuxConnections` accepts each conn and spawns a goroutine `routeMuxConnection`:
  1. Set read deadline 10s (so idle conns do not leak); cleared once routed.
  2. If TLS conn: complete handshake; if negotiated ALPN is `h2` or `http/1.1` hand straight to HTTP.
  3. Otherwise `bufio.Reader.Peek(1)`. If first byte is one of `* $ + - :` (RESP prefixes) -> Redis protocol handler (section 7.1). Otherwise hand to HTTP via `muxListener.Put(bufferedConn{conn, reader})` (the peeked byte is replayed).
- `muxListener` is a channel-backed `net.Listener` (buffer 1024) feeding `http.Server.Serve`. `Stop` closes mux listener, base listener, then `http.Server.Close()` immediately (no graceful drain), then closes the codex live handler.
- Rust: one TCP accept loop, peek first byte (with TLS: peek after handshake for non-h2/http1.1 ALPN), dispatch to hyper or RESP handler.

### 1.2 Middleware order (`NewServer`)
Global engine order:
1. `GinLogrusLogger` (request id + access log line, section 9.1)
2. `GinLogrusRecovery` (panic -> log `recovered from panic` with stack, respond bare 500; `http.ErrAbortHandler` re-panicked)
3. `CPATraceIDMiddleware` (wraps writer; sets `X-CPA-TRACE-ID` header at first write if a trace id was recorded)
4. extra middleware from options
5. `RequestLoggingMiddleware` (only if `commercial-mode` is false and a logger exists; section 9.2)
6. `corsMiddleware`
7. `homeHeartbeatMiddleware` (home mode only)
8. `exampleAPIKeySafeModeMiddleware`
Then `setupRoutes`, optional router configurator, management routes (only if a management secret exists, see management section), `NoRoute = pluginManagementNoRoute`.

gin settings: release mode unless `debug`; `SetTrustedProxies(cfg.trusted-proxies)` (invalid list -> log error, disable proxy trust). `c.ClientIP()` honors forwarded headers only for trusted proxies.

### 1.3 CORS (every response, incl. errors)
```
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS
Access-Control-Allow-Headers: *
Access-Control-Expose-Headers: X-CPA-TRACE-ID, X-CPA-VERSION, X-CPA-COMMIT, X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, X-Request-Id, OpenAI-Request-Id
```
`OPTIONS` (any path) -> `204` with no body, aborts the chain (so it bypasses auth). Upstream passthrough headers named `Access-Control-*` or `X-Cpa-Trace-Id` are dropped (reserved).

### 1.4 Home heartbeat gate
If `home.enabled`: every request except paths starting `/v0/management`, `/v8/management`, `/v0/resource/plugins/` or exactly `/management.html` gets bare `503` (no body) while the home subscribe-config heartbeat is not healthy.

### 1.5 Example-API-key safe mode
Enabled at startup (cmd main) when `api-keys` contains template values (`safemode.HasExampleAPIKeys`), not in command mode, not home mode, not TUI-client mode. While active:
- `GET|HEAD /` and `/management.html` (without `?safe-mode=configure`) -> `200 text/html` warning page (`Cache-Control: no-store`; HEAD no body).
- `/management.html?safe-mode=configure` passes through to the real panel.
- Any path equal/under `/v1`, `/v1beta`, `/openai/v1`, `/backend-api/codex` -> `403`, header `X-CPA-SAFE-MODE: example-api-key`, body
  `{"error":"unsafe_example_api_key","message":"Proxy API endpoints are disabled because api-keys contains template values. Open /management.html?safe-mode=configure, update api-keys in Management, then retry."}`
- Flag re-evaluated on config reload (`exampleAPIKeySafeModeActive`).

### 1.6 Config reload effects on the server (`server_reload.go`)
`UpdateClients(cfg)` swaps `cfg`, `handlers.UpdateClients(effectiveSDKConfig)`, re-applies access providers (API keys), retry config, request-logger enable toggle (`request-log`) and `error-logs-max-files`, `ws-auth` (+ change callback: enabling ws-auth tears down existing `/v1/ws` sessions), signature-cache settings, codex live media relay config. `effectiveSDKConfig` copies `SDKConfig` and forces `RequestLog=false` when `commercial-mode`.

---

## 2. Complete route table

Auth column: `key` = `AuthMiddleware` (API key, section 3; open if no keys configured); `rt-key` = realtime auth (key OR local `ek_...` client secret); `rt-std` = key with realtime-shaped error JSON; `mgmt` = management secret; `none`.

### 2.1 Proxy surface
| Method | Path | Auth | Handler / behavior |
|---|---|---|---|
| GET,HEAD | `/healthz` | none | `200 {"status":"ok"}` (HEAD: empty 200). Successful GET/HEAD healthz are not access-logged |
| GET | `/` | none | `200 {"message":"CLI Proxy API Server","endpoints":["POST /v1/chat/completions","POST /v1/completions","GET /v1/models"]}` |
| GET | `/keep-alive` | local password | only if keep-alive option enabled (section 7.3) |
| GET | `/management.html` | none (panel itself) | control panel HTML (management section) |
| GET | `/v1/models` | key | unified model list; see 2.3 |
| POST | `/v1/chat/completions` | key | OpenAI chat (5.1) |
| POST | `/v1/completions` | key | legacy completions via chat (5.1) |
| POST | `/v1/images/generations`, `/v1/images/edits` | key | images (5.6) |
| POST | `/v1/videos`, `/v1/videos/generations`, `/v1/videos/edits`, `/v1/videos/extensions` | key | xAI video (5.6). `/v1/videos` = `XAIVideosGenerations` |
| GET | `/v1/videos/:request_id` | key | xAI video poll |
| POST | `/v1/messages` | key | Claude Messages (5.4) |
| POST | `/v1/messages/count_tokens` | key | Claude count tokens |
| GET | `/v1/responses` | key | Responses WebSocket upgrade (5.3) |
| POST | `/v1/responses` | key | Responses HTTP (5.2) |
| POST | `/v1/responses/compact` | key | Responses compaction (non-stream) |
| POST | `/v1/alpha/search` | key | Codex alpha search passthrough (2.4) |
| POST | `/v1/live` | key | Codex live (WebRTC bootstrap) (6) |
| GET | `/v1/live/:call_id` | key | live sideband websocket (6) |
| GET | `/v1/realtime` | rt-key | standard realtime websocket or sideband (6) |
| POST | `/v1/realtime`, `/v1/realtime/calls` | rt-key | WebRTC call bootstrap (same as `/v1/live`) |
| GET | `/v1/realtime/calls/:call_id` | rt-key | sideband websocket |
| POST | `/v1/realtime/client_secrets`, `/v1/realtime/sessions` | rt-std | ephemeral key issue (6) |
| POST | `/v1/realtime/transcription_sessions` | rt-std | `501 not_supported_error` |
| GET,POST | `/v1/realtime/translations` | rt-key | `501` |
| POST | `/v1/realtime/translations/client_secrets` | rt-std | `501` |
| POST | `/v1/realtime/calls/:call_id/hangup` | rt-std | forward hangup |
| POST | `/v1/realtime/calls/:call_id/{accept,reject,refer}` | rt-std | `501` (SIP) |
| POST | `/openai/v1/videos` | key | OpenAI-shaped video create (5.6) |
| GET | `/openai/v1/videos/:video_id`, `/openai/v1/videos/:video_id/content` | key | retrieve / download |
| GET | `/backend-api/codex/responses` | key | Responses websocket (Codex CLI `chatgpt_base_url`) |
| POST | `/backend-api/codex/responses`, `/backend-api/codex/responses/compact`, `/backend-api/codex/alpha/search` | key | same handlers as `/v1/...` |
| GET | `/v1beta/models` | key | Gemini model list |
| POST | `/v1beta/interactions` | key | Gemini Interactions (5.5) |
| POST,GET | `/v1beta/models/*action` | key | `POST` = generateContent/streamGenerateContent/countTokens; `GET` = single model |
| GET | `/v1/ws` (configurable; fixed `/v1/ws`) | `ws-auth` ? key : none | AI Studio wsrelay gateway (7.2) |

### 2.2 OAuth loopback callbacks (no auth, serve on main port)
Registered in `setupRoutes`. Each reads `code`, `state`, `error` (fallback `error_description`) from the query, writes the callback file into `auth-dir` via `managementHandlers.WriteOAuthCallbackFileForPendingSession(authDir, provider, state, code, err)` only when `state != ""`, and always answers `200 text/html` with `oauthCallbackSuccessHTML` (a page titled "Authentication successful" that calls `window.close()` after 5s).
- `GET /anthropic/callback` (provider key `anthropic`), `GET /codex/callback` (`codex`), `GET /antigravity/callback` (`antigravity`).
- `GET /callback` and `GET /devin/callback` (provider `devin`): `Cache-Control: no-store`; trims values; if both `code` and `error` empty -> `400 {"error":"code or error is required"}`; if the pending-session write fails -> `400 {"error":"invalid or expired OAuth callback"}`.

### 2.3 `/v1/models` dispatch (`unifiedModelsHandler`)
In order:
1. User-Agent identifies grok shell (`grokbuild.IsGrokShellUserAgent`) -> grok model list.
2. Query has key `client_version` (even empty): Codex client model catalog (`codexmodels.BuildResponseForClientWithToolCapabilities`, compact JSON, depends on `client.codex.optimize-multi-agent-v2`, `client.codex.enable-apply-patch`, version string).
3. Home mode -> home model list.
4. Anthropic request (header `Anthropic-Version` present, or User-Agent starts with `claude-cli`) -> Claude list.
5. Else OpenAI list.
Formats (all pass through `WriteModelListResponse`: JSON, `Content-Type: application/json; charset=utf-8`, plugin response interceptors may rewrite):
- OpenAI: `{"object":"list","data":[{"id","object":"model","created"?,"owned_by"?}]}` (only those 4 fields survive the filter).
- Claude: `{"data":[{id,object,owned_by,created_at(RFC3339 UTC),type:"model",display_name,max_input_tokens,max_tokens}],"has_more":false,"first_id","last_id"}`, sorted by (display_name, id). Unless `claude-code.disable-cloaking-model-list`, IDs not starting with `claude-` are rewritten to `claude-fable-5-dd-` + the ID with characters reversed; `/v1/messages` and `count_tokens` reverse this (`ResolveClaudeModelIDPrefix`, keeps `(thinking)` suffix).
- Gemini (`/v1beta/models`): `{"models":[...]}`; each `name` gets `models/` prefix, default `displayName`/`description` = name, default `supportedGenerationMethods: ["generateContent"]`.
- `GET /v1beta/models/<id>`: matches `name == action || "models/"+action`; found -> model JSON; else `404 {"error":{"message":"Not Found","type":"not_found"}}`.

### 2.4 `POST /v1/alpha/search` and `/backend-api/codex/alpha/search`
Raw passthrough to Codex search (no translator). Body max 16 MiB. Reads `id` (becomes `X-Session-ID` for selection) and `model`; strips `prompt_cache_key`, `prompt_cache_retention`; optional model-router plugin may retarget (target must be provider `codex`, else error). Credential selection via auth manager (policy `CodexAlphaSearchV1`, home dispatch aware). Upstream: OAuth creds -> `https://chatgpt.com/backend-api/codex/alpha/search`; API-key creds -> `<attributes.base_url>/alpha/search` (model rewritten to resolved upstream model; missing base_url -> 503 `Codex Alpha Search API key base URL unavailable`). Upstream headers: `Content-Type/Accept: application/json`, `Originator: codex_cli_rs`, forwards `Version`, `User-Agent`, `Session_id`, `X-Client-Request-Id`, `Chatgpt-Account-Id` from creds metadata `account_id`. Response: upstream status, `Content-Type` and body (<=32 MiB) verbatim. Errors are `{"error":"<string>"}` (plain-string shape): 503 auth manager unavailable / `Codex auth unavailable`, 400 `Failed to read search request`, 502 upstream failure or `Failed to read Codex search response`, selection errors with `Retry-After` copied.

---

## 3. Client API-key authentication (sdk/access, internal/access)

- Provider list lives in `sdkaccess.Manager`; config-key provider id `config-inline`, built from `api-keys` (trim, dedupe, drop empty). With no keys configured the provider is unregistered, `Authenticate` returns `(nil, nil)` and ALL proxy requests are allowed (no auth). Plugin/SDK providers may also be registered.
- Manager loop: each provider returns result or error code `not_handled` (skip), `no_credentials` (remember missing), `invalid_credential` (remember invalid), anything else returns immediately. After loop: invalid -> `401 Invalid API key`; else `401 Missing API key`. Error codes -> `internal_error` yields `500` with message `Authentication service error` (logged).
- Config-key provider candidates, checked in this order, first value that is an exact member of the key set wins:
  1. `Authorization` header: if value is `<word> <rest>` and word equals `bearer` (case-insensitive) use trimmed rest; if no space or other scheme, the whole header value is used (so a raw key without `Bearer` works)
  2. `X-Goog-Api-Key` header
  3. `X-Api-Key` header
  4. query `key`
  5. query `auth_token`
  If ALL five are absent/empty -> no_credentials. Present but none match -> invalid. (Plain set lookup, not constant-time.)
- On success gin context keys set: `userApiKey` = the key string (principal), `accessProvider` = provider id, `accessMetadata` = `{"source": "authorization"|"x-goog-api-key"|"x-api-key"|"query-key"|"query-auth-token"}`. `userApiKey` is used for caller-scope, usage records (`api_key`) and realtime ownership.
- Failure response (standard `AuthMiddleware`): `401 {"error":"Missing API key"}` or `{"error":"Invalid API key"}` (plain-string error, regardless of dialect). 5xx auth errors: status = error status, message as is.
- Realtime standard-auth variant (`rt-std`): `{"error":{"message":<msg>,"type":"authentication_error","param":null,"code":"invalid_api_key"}}`; if status >= 500 `type:"server_error", code:"authentication_service_error"`.
- `rt-key` first checks for a local client secret: `Authorization: Bearer ek_...` (prefix `ek_`). Match: set principal to the issuer's principal/provider (provider default `realtime-client-secret`), store session in context. Prefix present but unknown/expired -> `401 {"error":{"message":"Realtime client secret is invalid or expired","type":"invalid_request_error","param":null,"code":"invalid_realtime_client_secret"}}`. No `ek_` prefix -> falls to rt-std.
- The websocket gateway `/v1/ws` is authenticated only when `ws-auth: true`.
- Access config shape (SDK): `AccessConfig{providers:[{name,type,sdk,api-keys,config}]}`, type constant `config-api-key`.

---

## 4. Shared handler pipeline (sdk/api/handlers)

### 4.1 Handler types / protocol ids (`internal/constant`)
`gemini`, `gemini-interactions`, `codex`, `claude`, `openai`, `openai-response`, `antigravity`, `interactions`. A handler's `HandlerType()` is the SOURCE format passed to the auth manager and translators. Ad hoc types used by images/videos: `openai-image`, `openai-video`.

### 4.2 Request body handling
- `ReadRequestBody` (OpenAI chat/completions/responses/compact/images): reads whole body (`c.GetRawData`, no size limit except endpoint-specific), then decodes `Content-Encoding`: empty/`identity` -> raw; list is applied right-to-left; only `zstd` supported; unknown encoding -> error unless the raw bytes are already valid JSON (then raw is used). Decode failure -> `400 {"error":{"message":"Invalid request: <err>","type":"invalid_request_error"}}`.
- Claude `/v1/messages`, `/count_tokens`, Gemini and Interactions endpoints use plain `GetRawData` (NO content-encoding decode).
- Request log middleware separately decodes zstd for logging only (9.2).
- `stream` detection: OpenAI chat/completions/responses: stream iff JSON `stream == true` (bool true). Claude: stream iff `stream` exists and is not `false`. Gemini: by URL action (`streamGenerateContent`). Interactions: `stream` must be a bool if present else 400 `stream must be a boolean`.
- `alt` query: `GetAlt` reads `alt` then `$alt`; value `sse` -> treated as empty. Non-empty `alt` is passed to executors (e.g. `alt=json`) and changes Gemini stream framing (5.5). Compact uses internal alt `responses/compact`.

### 4.3 Model routing (`handlers_routing.go`)
Per request, in `executeWithAuthManagerFormats` / stream / count variants:
1. Model router plugin (`applyModelRouter`): if the plugin host has model routers, call `RouteModel{SourceFormat: handlerType, RequestedModel, Stream, Headers, Query, Body, Metadata}`; response `Handled` + target kind: `self`/`executor` -> run through plugin executor (503 `plugin executor routing is unavailable while Home is enabled` in home mode; 502 `plugin executor host is unavailable`); `provider` -> force that provider (lowercased) and optional `TargetModel`.
2. Otherwise `getRequestDetailsWithOptions(model)`:
   - model `auto` (or with `(suffix)`) resolved via `util.ResolveAutoModel` (skipped in home mode).
   - base model = name with trailing `(thinking-suffix)` stripped (`thinking.ParseSuffix`).
   - image-only models (`gpt-image-1.5`, `gpt-image-2`, `gpt-image-2.5-flare`, `gpt-image-2.5-sunburst`, `gpt-image-2.5`, `grok-imagine-image`, `grok-imagine-image-quality`, `grok-imagine-image-2.0`, matched on the segment after the last `/`) used on a non-image endpoint -> `503` `model <base> is only supported on /v1/images/generations and /v1/images/edits`.
   - home mode -> providers `["home"]`.
   - `util.GetProviderName(baseModel)` (registry lookup; falls back to the full suffixed name). Empty -> `400` with ERROR TEXT (already JSON): `{"error":{"message":"unknown provider for model <model>","type":"invalid_request_error","code":"model_not_found","param":"model"}}`.
3. Provider list adjusted by entry protocol: `interactions` entry prefers `gemini-interactions` provider; entry protocols `openai`, `openai-response`, `claude`, `gemini`, `interactions` may use it; all other entries exclude `gemini-interactions`.
4. `AuthManager.Execute / ExecuteStream / ExecuteCount(ctx, providers, Request{Model, Payload}, Options)`. Options include `Stream`, `Alt`, `OriginalRequest` (raw body), `SourceFormat`, `ResponseFormat`, `Headers` (client request headers), `Query`, `Metadata`. Metadata keys populated: `idempotency_key` (from `Idempotency-Key`), request path (gin route pattern), requested model, reasoning effort (extracted per dialect), `service_tier` (`auto` default), `generate` (false only if `generate:false`), pinned auth id, selected-auth callback, execution session id, caller scope (hash of `userApiKey`), disallow-free-auth flag, trace-id callback.
5. Auth-selection error enrichment: `auth_not_found` / `auth_unavailable` errors get message `<msg> (providers=<a,b>, model=<m>[; last upstream error: <summary>])`, plus `; check Claude auth/key session and cooldown state via /v0/management/auth-files` when provider list contains `claude`; default status 503.
6. Plugin interceptors (host optional): `InterceptRequestBeforeAuth`, `InterceptRequestAfterAuth`, `InterceptResponse`, `InterceptStreamChunk` (+ request lifecycle and websocket response observers). An interceptor may terminate with a direct response (`Terminate` -> `DirectResponse` error message with status/headers/body). A Rust port can start without plugins but must keep the hook points.

### 4.4 Streaming execution and bootstrap retries
`ExecuteStreamWithAuthManager` returns `(dataChan, headers, errChan)`:
- It first reads chunks until the first deliverable payload (the "bootstrap" read). If the stream yields an error before any payload, and `streaming.bootstrap-retries` (default 0; forced 0 in home mode) allows, it re-calls `AuthManager.ExecuteStream` (new credential) when status is 0/unknown, 401, 402, 403, 408, 429 or >=500; otherwise the error is returned through `errChan` before any bytes are written, so the HTTP handler can still answer with a normal status code.
- Empty payloads are skipped. After bootstrap, chunks forwarded one by one; mid-stream error goes to `errChan` (1 buffered) and terminates.
- For `responseProtocol == "openai-response"` an SSE JSON validator reassembles frames split across chunks (normalizes CRLF/CR to LF, frames end at blank line) and rejects invalid `data:` JSON with `502 invalid SSE data JSON (len=N): "<first 512 bytes>"`. `data: [DONE]` and empty data are allowed.
- Headers returned = filtered upstream headers (4.5).

### 4.5 Header filtering and passthrough
- Response header passthrough is OFF by default (`passthrough-headers: false`): non-stream/stream handlers then forward only headers changed by plugin interceptors (diff against base), filtered. Plugin-host internal executions always pass filtered upstream headers.
- `FilterUpstreamHeaders` removes: hop-by-hop (`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `Te`, `Trailer`, `Transfer-Encoding`, `Upgrade`), `Set-Cookie`, `Content-Length`, `Content-Encoding`, any header named in the upstream `Connection` header, CPA-reserved (`Access-Control-Allow-{Credentials,Headers,Methods,Origin}`, `Access-Control-Expose-Headers`, `Access-Control-Max-Age`, `X-Cpa-Trace-Id`), and gateway-detection prefixes (case-insensitive): `x-litellm-`, `helicone-`, `x-portkey-`, `cf-aig-`, `x-kong-`, `x-bt-`.
- `WriteUpstreamHeaders(dst, src)`: add every value only if `dst` has no value yet for that key (CPA-set `Content-Type` wins). Called right before the first body byte.
- Error path: `Retry-After` from the error (`coreauth.SafeResponseHeaders`) always copied; `Addon` (upstream error headers) copied only when passthrough enabled (replace semantics, reserved skipped). Direct-response errors always copy filtered `Headers`.

### 4.6 Error body (`BuildErrorResponseBodyWithError`, OpenAI-shaped; default for OpenAI, Responses, Gemini, Interactions)
```
{"error":{"message":<text>,"type":<type>,"code":<code, omitted if empty>,"retryable":<bool, only for terminal auth>}}
```
Algorithm, `errText` = error string (or `http.StatusText(status)` if empty; status<=0 -> 500):
1. Terminal upstream auth error (`coreauth.IsTerminalAuthError`): message = errText, or if errText is JSON with `message` or `error.message` use that; body `{"error":{"message":..,"type":"authentication_error","code":"upstream_authentication_required","retryable":false}}`.
2. If trimmed errText is valid JSON -> returned VERBATIM (upstream payload preserved, no re-wrapping).
3. Else by status: 401 `authentication_error`/`invalid_api_key`; 403 `permission_error`/`insufficient_quota`; 429 `rate_limit_error`/`rate_limit_exceeded`; 404 `invalid_request_error`/`model_not_found`; 408 `server_error`/`request_timeout`; >=500 `server_error`/`internal_server_error`; everything else `invalid_request_error` with no code.
`WriteErrorResponse` sets `Content-Type: application/json` (if nothing written yet), status = `ErrorMessage.StatusCode` (>0) else 500. Status codes used by `clienterror`: explicit `StatusCode()`; `context.Canceled` -> 499; `DeadlineExceeded` -> 504.
Handler-local error JSON (always this form): `{"error":{"message":"Invalid request: <err>","type":"invalid_request_error"}}` (400, bad body), `{"error":{"message":"Streaming not supported","type":"server_error"}}` (500, no Flusher).

### 4.7 Claude error shape (only `/v1/messages`, `count_tokens`)
`ClaudeCodeAPIHandler.WriteErrorResponse`: `{"type":"error","error":{"type":<t>,"message":<m>}}`.
- Type by status: 401 `authentication_error`, 402 `billing_error`, 403 `permission_error`, 404 `not_found_error`, 413 `request_too_large`, 429 `rate_limit_error`, 504 `timeout_error`, 529 `overloaded_error`, other >=500 `api_error`, else `invalid_request_error`.
- Message = error text trimmed (default status text). If that text is JSON: with `error` object -> take `error.type` and `error.message` (else `error.code` as message); without -> take top-level `type` (unless `"error"`) and `message`. So Anthropic upstream errors pass through with the same type/message.
- Same Retry-After/Addon/direct-response rules as 4.5. Fallback body `{"type":"error","error":{"type":"api_error","message":"Internal Server Error"}}`. The streaming terminal error uses the same struct (5.4).

### 4.8 Keepalive
- Streaming (`streaming.keepalive-seconds`, default 0 = off): `ForwardStream` ticker; each tick writes the SSE comment `: keep-alive\n\n` and flushes (the default; handlers can override). For WebSockets the same interval sends WS Ping control frames. Gemini with non-empty `alt` disables the comment keepalive (raw JSON stream). Images streaming writes the same comment even before the upstream stream is opened (`waitImagesStreamExecution`, sets SSE headers first).
- Non-streaming (`nonstream-keepalive-interval` seconds, default 0): `StartNonStreamingKeepAlive` spawns a goroutine writing a single `\n` byte and flushing every interval until stopped (stop must be called before the final write). Because bytes are flushed before the status line is decided, the response status becomes 200 with Go's implicit header at the first keepalive write; the JSON body (or error JSON) then follows after leading blank lines. Only Content-Type was set before (`application/json`). Applies to OpenAI chat/completions, Responses (non-stream and compact), Claude messages (not count_tokens), Gemini generateContent (not countTokens), Interactions non-stream, videos retrieve/content, images collect.

### 4.9 `ForwardStream` loop (`stream_forwarder.go`)
Select over: request context cancel (`cancel(ctx.Err())`, return), data chunk, error channel, keepalive tick.
- chunk: `WriteChunk`, flush, then `ChunkError()` check (terminal -> cancel, return).
- data channel closed: pending error (non-blocking read of `errs`) or `CloseError()` -> `WriteTerminalError`, flush, `cancel(err)`; else `WriteDone`, flush, `cancel(nil)`.
- error received: normalize, `WriteTerminalError`, flush, cancel, return.
Initial-chunk logic (before ForwardStream): each handler waits for first of {ctx cancel, errChan, dataChan}. Error before data: normal HTTP error response (status code from error). Data closed with no data and no error: SSE headers + dialect footer (OpenAI `data: [DONE]\n\n`, Claude/Gemini nothing) with status 200. First chunk: set SSE headers, write upstream headers, write chunk, flush, then hand over to ForwardStream.

### 4.10 SSE response headers
All SSE endpoints set: `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Connection: keep-alive`, `Access-Control-Allow-Origin: *`. They are set only when the first chunk is ready, so early errors keep JSON content type.

### 4.11 Request-scoped context and session ids
`GetContextWithCancel`: derives cancelable ctx; copies request id; endpoint `"<METHOD> <route pattern>"`; client metadata (client IP = remote addr host, resolved IP = `c.ClientIP()`, `X-Forwarded-For` joined, user agent, session id, parent session id from headers via `coresession.ExtractSessionInfo`); response status/headers holders. The returned cancel func records final status and, when `request-log` is on, appends error/response text to the `API_RESPONSE` gin key (dedupe if already present). Session id extraction also looks at body/metadata (`EnrichContextWithSessionHierarchy`); session ids are normalized to canonical UUIDs for usage records.
Client cancel -> `cancel(ctx.Err())` and handler returns without writing.

---

## 5. Per-dialect behavior

### 5.1 OpenAI: `/v1/chat/completions`, `/v1/completions`
`POST /v1/chat/completions` (handlerType `openai`):
- Body via `ReadRequestBody`. If body has no `messages` but has `input` or `instructions`, it is treated as a Responses-format request and converted with `ConvertOpenAIResponsesRequestToOpenAIChatCompletions(model, raw, stream)`; `stream` re-read from the converted body.
- Non-stream: `Content-Type: application/json`, keepalive blank lines optional, upstream body written verbatim.
- Stream SSE framing: each chunk `data: <chunk>\n\n` (chunk is the raw JSON from the translator, no `event:` line); on clean close `data: [DONE]\n\n`. Terminal error mid-stream: `data: <BuildErrorResponseBody(status,text)>\n\n` and NO `[DONE]` after it (stream just ends). Keepalive comment `: keep-alive\n\n`.
- Empty successful stream (data closed before any chunk, no error): headers + `data: [DONE]\n\n`.

`POST /v1/completions`: converts to chat in-place (`convertCompletionsRequestToChatCompletions`): `prompt` string (default `"Complete this:"`) becomes a single user message; copies `model,max_tokens,temperature,top_p,frequency_penalty,presence_penalty,stop,stream,logprobs,top_logprobs,echo`. Alt is empty. Response conversion:
- non-stream: `{"id","object":"text_completion","created","model","usage"?,"choices":[{"index","text":<message.content>,"finish_reason"?,"logprobs"?}]}`.
- stream: only chunks with non-empty `delta.content`, a non-null `finish_reason`, or `usage` are forwarded as `data: {"id","object":"text_completion","created","model","choices":[{"index","text","finish_reason"?,"logprobs"?}],"usage"?}\n\n`; then `data: [DONE]\n\n`.

### 5.2 OpenAI Responses: `POST /v1/responses`, `/v1/responses/compact`
handlerType `openai-response`. Body via `ReadRequestBody`, then:
1. `prepareCodexMultiAgentV2Tools` (when `client.codex.optimize-multi-agent-v2`, rewrites tools for official Codex multi-agent requests; sets gin key `CodexMultiAgentV2ToolsPrepared`).
2. `prepareCodexOrphanDelegation` (when `codex.orphan-delegation-compatibility` and not OAuth-only-field blocked; rewrites orphan delegation inputs).
Non-stream (`stream != true`): plain passthrough JSON.
`/responses/compact`: `stream:true` -> `400 {"error":{"message":"Streaming not supported for compact responses","type":"invalid_request_error"}}`; a present `stream:false` is deleted from the body; executes with alt `responses/compact`, non-stream with keepalive; no Multi-agent tool preparation.

Stream path (`handleStreamingResponse`, with `responsesSSEFramer`). The framer is the most intricate part; requirements:
- Client kind: Codex client iff User-Agent matches Codex (`multiagentv2.IsCodexClientUserAgent`) or header `Originator` (case-insens.) is `codex desktop`, `codex-tui`, `codex_cli_rs` or starts with `<those>/`.
- Input chunks may be partial or merged SSE frames. Framer accumulates in `pending`, splits frames on `\n\n` or `\r\n\r\n`, inserts a missing line break when a new chunk begins with `data:`, `event:`, `id:`, `retry:` or `:` and pending lacks a trailing newline; a pending `data:`-only frame is flushed when the next chunk starts a new `data:` frame; frames with `event:` + `data:` (valid JSON) may be emitted without delimiter. Output frames always end `\n\n` (preserves `\r\n` if input used it).
- Per frame (`repairFrame`): drop frames whose `event:` name or payload `type` starts with `responsesapi.`; for non-Codex clients also drop names starting `codex.`; for Codex clients drop only `codex.rate_limits`. `[DONE]` passes through (counts as a data frame). Invalid-JSON data passes through untouched.
- `response.output_item.done` payloads are recorded by `output_index` (or unindexed list). On `response.completed` whose `response.output` is missing/empty array, `response.output` is rebuilt from recorded items sorted by index (then unindexed) and the frame is rewritten with `event:` lines preserved and `data:` replaced.
- Terminal events: `response.completed`, `response.incomplete`, `response.failed`, `response.done`, `response.error`, `error`. After a terminal event further chunks are ignored.
- Error events (`type` in `response.failed|response.error|error`, or payload has non-null `error` / `response.error`, or has both `code` and `message`): rewritten via `repairErrorPayload`: status taken from first of `status,status_code,error.status,error.status_code,response.error.status,response.error.status_code` in [400,599], default 502; message sanitized (secrets redacted); emitted as `event: response.failed` (Codex clients) or `event: error` (others).
- Error chunk shapes (`sdk/api/handlers/openai_responses_stream_error.go`):
  - `error`: `{"type":"error","error":{"type":<t>,"code":<c>,"message":<m>,"param":null},"sequence_number":N}`
  - `response.failed`: `{"type":"response.failed","sequence_number":N,"response":{"status":"failed","error":{...same detail...}}}`
  - detail: if the error text is JSON with `error` (or `response.error`) object, that object is used verbatim; else `{type, code, message, param:null}` where (code,type): 401 `invalid_api_key`/`invalid_request_error`; 403 `insufficient_quota`/`invalid_request_error`; 429 `rate_limit_exceeded`/`invalid_request_error`; 404 `model_not_found`/`invalid_request_error`; 408 `request_timeout`/`server_error`; >=500 `internal_server_error`/`server_error`; other >=400 `invalid_request_error`/`invalid_request_error`; <400 `unknown_error`. A JSON payload `message`, `code`, `param`, `type` (if not `"error"`) override the defaults. `sequence_number`: payload's own if present, else data frame count (-1 for repaired mid-stream, min 0).
- Sanitization (`sanitizeResponsesStreamErrorMessage`): status clamped to 400..599 else 500; text truncated to 2048 runes (+ `…`), each field to 256; regex-redacts `api_key|access_token|token|authorization|secret` values and `Bearer <token>`; JSON keys that look secret (`authorization,secret,password,passwd,api_key,apikey,token,access_token,refresh_token,id_token,auth_token,session_token,api_token,client_secret,client_key`, or suffix `_secret|_password|_api_key|_token`, excluding names containing `tokens`/`token_count`/`token_limit`/`token_usage`) become `"[REDACTED]"`. Direct-response errors are only sanitized after first data.
Control flow:
- Buffer initial frames in `initialOutput` until the first real data frame (`dataFrames > 0`). If an error arrives before any data frame -> normal JSON HTTP error (OpenAI shape, status code real, sanitized unless DirectResponse). Stream closed before first payload and no error -> `502` `upstream stream closed before first payload`.
- After first data frame: SSE headers, flush buffered frames; if a terminal error was already framed, stop. Then `forwardResponsesStream`.
- On clean upstream close: framer flush, then if no terminal event seen -> synthetic error `502 upstream stream closed before a terminal event (last event: <name>)` emitted as `event: error` / `event: response.failed` preceded by `\n`. If a terminal event was seen, `WriteDone` writes just `\n` (NO `data: [DONE]`).
- Keepalive and mid-stream terminal error use `: keep-alive\n\n` and the same error chunk format (`\nevent: <name>\ndata: <chunk>\n\n`).

### 5.3 Responses WebSocket: `GET /v1/responses` and `GET /backend-api/codex/responses`
Auth: `AuthMiddleware` on the GET (API key via headers or `?key=`). Upgrade via gorilla with `CheckOrigin = always true`, read/write buffer 4096. Response header on upgrade: echoes `x-codex-turn-state` if the client sent it. If upgrade fails the handler just returns (gorilla already wrote the HTTP error).
Messages: client sends JSON text (or binary) frames; server sends JSON text frames (one event per frame, no SSE framing, no `data:` prefix). Non-text/binary frames are skipped. Pings: with `streaming.keepalive-seconds > 0` server sends WS Ping control frames (ticker reset on each data chunk).
Session state per connection (all in `ResponsesWebsocket`): `lastRequest` (normalized last full request), `lastResponseOutput`, `lastResponseID`, `lastResponsePendingToolCallIDs`, `pendingPrewarmID`, pinned auth id per provider, `upstreamMode` (`""|websocket|http`), `passthroughModelName`, `observedCompaction`, execution session id (random UUID used as upstream execution session). Downstream session key for tool caches: first non-empty of `X-Client-Request-Id`, `X-Codex-Turn-Metadata` JSON `session_id`, `Session-Id`, `Session_id`.
Request types (`type` field): `response.create`, `response.append`. Else `400` event `unsupported websocket request type: <t>`.
Normalization (fallback / HTTP-upstream mode, `normalizeResponsesWebsocketRequestWithIncrementalState`):
- `response.create` with no prior request: delete `type`, force `stream:true`, default `input` `[]`, require non-empty `model` (`missing model in response.create request`), `input` if present must be an array (`websocket request requires array field: input`). Stored as `lastRequest`.
- subsequent create/append: requires prior request (`websocket request received before response.create`) and array `input`. If input is a "transcript replacement" (contains `function_call`/`custom_tool_call` items, an assistant `message`, or a Codex local compaction summary message starting with the fixed prefix "Another language model started to solve this problem...") -> replace: delete `type`+`previous_response_id`, fill `model`/`instructions` from `lastRequest`, `stream:true`. Else when incremental allowed: if no `previous_response_id`, and the new input satisfies all pending tool call ids (has `function_call_output`/`custom_tool_call_output` for each) use `previous_response_id = lastResponseID`, else replace; send incremental with `previous_response_id`. Else merge `lastRequest.input + lastResponseOutput + new input` (dedupe by item id/call id), compaction items handled (`compaction`/`compaction_summary` = full transcript: with replay bypass allowed input replaces, else compaction items dropped from the appended part).
- `previous_response_id` with no history -> error event `409`: `{"error":{"message":"Previous response is not available on this websocket; resend the full conversation input without previous_response_id","type":"invalid_request_error","code":"previous_response_not_found","param":"previous_response_id"}}`.
- Tool call repair (`prepareResponsesWebsocketFallbackTurn`): caches function_call outputs and calls per downstream session (TTL 30m, max 256 per session) and rewrites orphan call/output items in the outgoing input so upstream accepts it.
Prewarm: `response.create` with `generate:false` (when not upstream-ws passthrough) is answered locally with two synthetic events and nothing is sent upstream: `{"type":"response.created","sequence_number":0,"response":{"id":"resp_prewarm_<uuid>","object":"response","created_at":<unix>,"status":"in_progress","background":false,"error":null,"output":[],"model"?}}` then `{"type":"response.completed","sequence_number":1,"response":{"id":..., "status":"completed","usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0},...,"output":[]}}`. The next request with `previous_response_id == prewarm id` is merged with the warmup request; any other previous id -> the 409 error above.
Upstream-websocket passthrough (codex/xai creds with `websockets` attribute/metadata true, or `codex` / `xai` provider selected for the model): requests are normalized minimally (`type` must be create/append, default model from earlier turn, `stream:true`) and the upstream session is reused; `previous_response_id` or `response.append` then REQUIRE the same upstream websocket: if the session fell back, connection is closed with WS close code 1012 (`CloseServiceRestart`) reason `upstream requires HTTP replay` so the client replays over a fresh socket. Credential pinned across turns (re-validated against model). If `codex.response-steering` is on, input frames are read concurrently (`readResponsesWebsocketInput`, queue 16) and fed into the upstream duplex stream.
Forwarding (`forwardResponsesWebsocket`): upstream SSE chunk bytes are split per line (`event:` lines ignored, `data:` prefix and `[DONE]` stripped, only valid JSON kept) and each JSON object is written as a text frame. Tracks `response.created` (reset output tracking), collects `response.output_item.done` items, and on completion events (`response.completed`, `response.done`) restores `response.output` when empty (and reconciles incomplete tool calls) unless native Codex output must be preserved. Also tracks pending tool call ids for the next turn.
Errors:
- Before/within a turn upstream error or `type:"error"` event: if exposable (`IsRequestFault` status or terminal auth error) the server sends one frame and closes: `{"type":"error","status":<int>,"headers":{<addon headers first value>}?,"error":<OpenAI error object>}` (error object from `BuildErrorResponseBodyWithError`, unwrapped `error` field). Non-exposable errors (5xx, 429, etc.) close the socket without a frame (client sees abnormal close and retries). `message_too_big` (413 with `error.code=message_too_big`, or ws close 1009) -> close frame 1009 with that reason (truncated to 123 bytes).
- Normal-flow parse/validation errors (the ones above, status 400/409) are written as the same error frame but the connection STAYS open (loop continues).
- Stream closed before `response.completed` -> error 408 `stream closed before response.completed` (close without frame).
- Client closes (1000, 1001, 1005) logged as disconnect; session cleanup closes the upstream execution session.
Timeline logging: when `request-log`, a websocket timeline (request/response/disconnect entries with timestamps) is attached for the request log (9.2).

### 5.4 Claude: `POST /v1/messages`, `/v1/messages/count_tokens`
handlerType `claude`. Body raw. `rewriteClaudeDDModelInBody` reverses cloaked model ids.
- Non-stream: `Content-Type: application/json`; if the response body starts with gzip magic `1f 8b` it is gunzipped before writing; then body verbatim.
- Stream: SSE passthrough of the translator output bytes (Claude event frames `event: ...\ndata: ...\n\n` are produced upstream/by translator; the handler writes chunks raw, no extra framing, NO `[DONE]`). First chunk triggers headers; data closed cleanly -> headers + flush, nothing else. Keepalive: `: keep-alive\n\n`.
- Mid-stream terminal error: `c.Status(status)` (no effect after headers sent) then `event: error\ndata: {"type":"error","error":{"type":..,"message":..}}\n\n`.
- `count_tokens`: non-stream, `ExecuteCount` path, no keepalive.
- Error body: Claude shape (4.7).

### 5.5 Gemini: `/v1beta/models/*action`, `/v1beta/interactions`
`POST /v1beta/models/<model>:<method>`: route wildcard `action` has leading `/` trimmed then split on `:`; must be exactly 2 parts else `404 {"error":{"message":"<path> not found.","type":"invalid_request_error"}}`. Methods: `generateContent` (non-stream, keepalive), `streamGenerateContent`, `countTokens`. Any other method name: handler silently does nothing (empty 200). Model name = part before `:` (may carry `models/` prefix handling downstream; `gemini` handlerType). Auth also accepts `?key=` / `x-goog-api-key`.
- `streamGenerateContent`: without `alt` (or `alt=sse`): SSE `data: <chunk>\n\n` per chunk (NO `[DONE]`); with `alt` set (e.g. `json`): raw chunk bytes written with no framing, no Content-Type SSE headers, and keepalive disabled. Error before first chunk: normal JSON error. Mid-stream error: `event: error\ndata: <OpenAI-shaped error>\n\n` (SSE mode) or the raw error JSON (alt mode).
- Errors use the OpenAI-shaped body (4.6), NOT Google's `{"error":{"code","status"}}` form.
`POST /v1beta/interactions` (`Interactions`): body must be JSON object with exactly one of `model` or `agent` (else `400 request requires exactly one of model or agent`; invalid JSON -> `400 invalid JSON body`); `model` has optional `models/` prefix stripped (body rewritten). `agent` forces provider `gemini-interactions` with auth-selection model `gemini-2.5-flash`. Entry/exit protocol `interactions`. Non-stream: JSON passthrough. Stream: SSE; each chunk forwarded as-is if it already starts with `event:`/`data:`, else prefixed `data: `, always terminated with a blank line; terminal error `event: error\ndata: <OpenAI-shaped error>\n\n`. A forced provider disagreeing with a router decision or plugin executor -> 400 `agent is only supported for native interactions execution`.

### 5.6 Images and videos
`POST /v1/images/generations`: if `disable-image-generation` is `all` -> bare 404 (abort, no body). JSON body (must be valid JSON), `model` default `gpt-image-2`, `prompt` required (400 `Invalid request: prompt is required`), `response_format` default `b64_json`, `stream` bool.
- Supported models: codex image tool models (`gpt-image-1.5`, `gpt-image-2`, `gpt-image-2.5-flare`, `gpt-image-2.5-sunburst`, `gpt-image-2.5`, optional provider prefix `x/` honored), xAI (`grok-imagine-image`, `-quality`, `-2.0`), openai-compat registry models of type image. Else `400` message `Model <m> is not supported on /v1/images/generations or /v1/images/edits. Use <list>, or a configured openai-compatibility image model.` (type `invalid_request_error`).
- Response (non-stream): `{"created":<unix>,"data":[{"b64_json"|"url":"data:<mime>;base64,<b64>","revised_prompt"?}],"background"?,"output_format"?,"quality"?,"size"?,"usage"?}`.
- Stream: SSE with named events `event: image_generation.partial_image` (`{"type":"image_generation.partial_image","partial_image_index":N,"b64_json"|"url":..}`) and `event: image_generation.completed`; edits use prefix `image_edit.`. Keepalive comment, terminal error `event: error\ndata: <openai error>\n\n`.
- Legacy path builds a Responses request with a hosted `image_generation` tool (base model `gpt-image-2-base-model` or `gpt-5.4-mini`) and converts the result; codex tool models go through routed images.
`POST /v1/images/edits`: JSON (images as urls/data) or multipart (`image` files converted to data URLs).
Videos: `POST /v1/videos` etc. map to xAI (`grok-imagine-video`, `-1.5`, `-1.5-preview`; native endpoints `/v1/videos/{generations,edits,extensions}` forward xAI JSON). `POST /openai/v1/videos` accepts OpenAI Sora-shaped requests (default model `sora-2`, seconds `4`, size `720x1280`) and returns an OpenAI-shaped video object; `GET /openai/v1/videos/:video_id` polls; `/content?variant=video` streams the mp4 (only `variant=video`, others -> 400). Video ids are pinned to the creating credential for `video-result-auth-cache-ttl` (default 3h) in an in-memory store. Retrieve/content run non-stream with keepalive.

---

## 6. Realtime / live endpoints (internal/client/codex/live)
All are Codex-OAuth only. Used by Codex desktop voice (WebRTC).
- `POST /v1/live`, `/v1/realtime`, `/v1/realtime/calls`: WebRTC call bootstrap. Body JSON (with `session`, `model`) or `application/sdp`/`text/plain` offer or multipart (`sdp` + `session`); max 16 MiB (413 `errBodyTooLarge`). Model default `gpt-live-1-codex`; any `gpt-realtime`, `gpt-realtime-*`, `*realtime-preview*` maps to `gpt-live-1-codex`. Selected Codex OAuth credential posts to `https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas` with forwarded protocol headers (`OpenAI-Alpha, X-Session-Id, Session-Id, Thread-Id, Originator, OpenAI-Safety-Identifier, OpenAI-Organization, OpenAI-Project, X-Oai-Attestation`) and `Chatgpt-Account-Id`. Response: upstream status/body; on 2xx the `Location` header (call id) is rewritten to `/v1/realtime/calls/<id>` for `/v1/realtime*` paths and the session (call id, auth id, owner principal) is stored for sideband/hangup. If `codex.live-media-relay.enabled`, a local WebRTC media relay (pion) terminates the client SDP and creates a second leg to upstream (answer `Content-Type: application/sdp`); options `max-sessions`, `disable-private-remote-ips`, `public-ip`, `udp-port-min/max`, `ice-servers`.
- `GET /v1/live/:call_id`, `/v1/realtime/calls/:call_id`, `GET /v1/realtime?call_id=...`: WebSocket sideband relay to `wss://api.openai.com/v1` bidirectionally; call must exist (404 `Codex live session not found`), not already joining (409), owner must match (403 `realtime_call_scope_mismatch`); non-upgrade -> 426 `WebSocket upgrade required`.
- `GET /v1/realtime[?model=]` (no `call_id`): standard Realtime WebSocket relayed through Codex OAuth (default model `gpt-realtime` -> `gpt-live-1-codex`); non-upgrade -> 426 with `Upgrade: websocket`.
- `POST /v1/realtime/client_secrets` (and deprecated `/v1/realtime/sessions`): issues local ephemeral key `ek_<base64url>` bound to a normalized session; body `{session?, expires_after?{anchor,seconds}}` (<=64 KiB), lifetime default 10m, min 10s, max 2h; max 1024 entries, 64 per issuer (429 `realtime_client_secret_capacity_exhausted` + `Retry-After: 1`). Response `{"value":"ek_...","expires_at":<unix>,"session":{...}}`, `Cache-Control: no-store` (legacy form adds `client_secret:{value,expires_at}` to a session object).
- `POST /v1/realtime/calls/:call_id/hangup`: forwards hangup with pinned credential.
- Capability stubs `501`: transcription sessions, translations, SIP accept/reject/refer. Body `{"error":{"message":"<cap> are not supported by the ChatGPT/Codex OAuth upstream","type":"not_supported_error","param":null,"code":"realtime_capability_not_supported"}}`.
- Error body on `/v1/realtime*`: `{"error":{"message","type","param":null,"code"}}` where `/v1/live` paths use `{"error":"<msg>"}`; `writeLiveError` types: 401 `authentication_error`, 4xx `invalid_request_error`, else `api_error`, code `realtime_request_failed`.

---

## 7. Other listeners and endpoints
### 7.1 Redis (RESP) usage queue on the main port (`internal/api/redis_queue_protocol.go`)
Active only while management is enabled (`managementRoutesEnabled`, i.e. a secret exists); disabled in home mode (`-ERR redis usage output disabled in home mode`). Commands (arrays of bulk strings only; non-`*` prefix -> `-ERR protocol error`):
- `AUTH <password>` or `AUTH <user> <password>`: password is the management key, validated by the management auth path including the failed-attempt IP ban (`ERR IP banned due to too many failed attempts ...`); `+OK`. Remote clients need `allow-remote`; localhost allowed.
- Before AUTH any command -> `-NOAUTH Authentication required.`
- `SUBSCRIBE usage|errors` -> RESP pubsub (`subscribe` ack, then `message` pushes with JSON payloads), `PING [msg]`, `UNSUBSCRIBE`, `QUIT` handled in subscribed mode; unknown channel `-ERR unsupported channel '<c>'`.
- `LPOP|RPOP usage [count]` -> pops oldest queued usage JSON (bulk string; with count: array; empty: nil bulk). Other keys unsupported.
- Unknown command `-ERR unknown command '<lower>'`.
- Usage record JSON keys: `timestamp, latency_ms, ttft_ms, source, auth_index, access_token_sha256?, client_ip, resolved_client_ip, x_forwarded_for, user_agent, tokens{input_tokens,output_tokens,reasoning_tokens,cached_tokens,cache_read_tokens,cache_creation_tokens,total_tokens}, failed, generate, stream, fail{...}, response_headers, accounting_version, token_breakdown, provider, executor_type, model, alias, endpoint, auth_type, api_key, request_id, execution_id?, trace_id?, session_id?, parent_session_id?, node_kind?, is_fork?, is_compaction?, reasoning_effort, service_tier, response_service_tier?, response_model?`. Retention governed by `redis-usage-queue-retention-seconds`; recording gated by `usage-statistics-enabled`.

### 7.2 `/v1/ws` (AI Studio wsrelay)
Browser-resident AI Studio client connects via WebSocket; server holds one session per provider id (random name `aistudio-...` unless factory supplies one; a reconnect replaces the old session). Envelope `{"id","type","payload"}` with types `http_request`, `http_response`, `stream_start`, `stream_chunk`, `stream_end`, `error`, `ping`, `pong`. Only GET; wrong path -> 404; auth only if `ws-auth`. Used by the `aistudio` executor to tunnel upstream HTTP through the browser. (Details in the executors survey.)

### 7.3 `/keep-alive` and local management password
Only when started with a local password (TUI standalone / `-password`): `cmd.StartService` registers `GET /keep-alive` with 10s idle shutdown: each call resets the timer; timeout cancels the service context. Auth: `Authorization: Bearer <pw>` or `X-Local-Password`, constant-time compare, else `401 {"error":"invalid password"}`; ok -> `200 {"status":"ok"}`.

---

## 8. Amp module
Not present in this version of the Go tree: no `amp` package, no `/api/provider/...`, `/api/auth` or `ampcode` handlers; `ampcode`, `amp-upstream-url`, `amp-upstream-api-key` keys are only deleted by config v8 migration (`internal/config/config_v8_test.go:509`). Do not port amp.

---

## 9. Logging (internal/logging, internal/api/middleware)
### 9.1 Application log (logrus) and access log
- Console/file line format (`LogFormatter`): `[YYYY-MM-DD HH:MM:SS] [<reqid8|-------->] [<level padded to 5, "warn" not "warning">] [<file>:<line>] <message>[ k=v ...]\n` (caller included when report-caller is on, which is the default: `SetReportCaller(true)`). `reqid8` = last 8 chars of the request id (`ShortRequestID`). Extra fields rendered only for a fixed ordered allowlist (`provider, model, plugin_id, plugin_name, source_id, version, active_version, retired_version, overwritten, mode, budget, level, original_mode, original_value, min, max, clamped_to, error, credential, auth_id, connection, proxy_scheme, remote_transport, media_session_id, call_id, peer, state, reason`, then plugin path fields); `credential,auth_id,connection,proxy_scheme,remote_transport,media_session_id,call_id,peer,state,reason` string values are `strconv.Quote`d.
- Destination: stdout, or when `logging-to-file` a lumberjack file `<logdir>/main.log` (rotate at 10 MB, no backups limit, no compression); background cleaner enforces `logs-max-total-size-mb` over the log dir (never deletes `main.log`).
- Log dir (`ResolveLogDirectory`): `<WRITABLE_PATH>/logs` if env/writable base set; else `logs` relative to CWD if writable; else `<auth-dir>/logs`.
- Level from `debug` flag (`util.SetLogLevel`).
- Access line (gin logger, after request): `"%3d | %13v | %15s | %-7s \"%s\""` with status, latency (ms truncated; >1m truncated to seconds), client IP, method, path + masked query; suffix ` [credits]` if antigravity credits flag; ` | <gin private errors>`; level Error for >=500, Warn >=400, else Info. Request id (UUIDv7) is generated only for paths `/v1*`, `/v1beta*`, `/openai/v1*`, `/backend-api/codex*` and stored in gin + request context; others use `--------`.
- Query masking (`MaskSensitiveQuery`): values for params named `key`, or containing `api-key|apikey|api_key|token|secret` become `HideAPIKey` (len>8: first4...last4; >4: first2...last2; >2: first1...last1) .

### 9.2 Request log files (`RequestLoggingMiddleware`, `FileRequestLogger`)
- Skipped when: `commercial-mode`; method GET unless it is a Responses websocket upgrade (`/v1/responses` or `/backend-api/codex/responses` with `Upgrade: websocket`); path under `/v0/management`, `/v8/management`, `/management*`.
- `request-log: true`: every logged request produces one file. `request-log: false`: nothing, EXCEPT a forced "error log" when the request is "actionable-error" (status >= 400 other than 499, or API response errors that are not client cancellations; ignores cancel with status < 400). Request body captured only if Content-Length <= 1 MiB (non-multipart) in error-only mode (else a deferred capture to temp file up to 32 MiB); full capture when enabled. `error-logs-max-files` (default retention) prunes the oldest `error-*.log`.
- File location: `<log dir>/`; name `<sanitized-path>-<YYYY-MM-DDTHHMMSS>-<id8>.log` where sanitized path = URL path without leading `/`, `/` and `:` and `<>:"|?*\s` -> `-`, runs collapsed, trimmed, empty -> `root`; `id8` = last 8 chars of request id (else a counter). Forced error logs prefixed `error-`. Collisions get a sequence number injected before the id (`createUniqueLogFile`, O_EXCL).
- Content layout (non-streaming), sections separated by blank lines so that each section ends with 3 newlines:
```
=== REQUEST INFO ===
Version: <build version>
URL: <path?masked-query>
Method: <M>
Downstream Transport: http|websocket     (omitted when empty)
Upstream Transport: http|websocket|websocket+http  (omitted when empty)
Timestamp: <RFC3339Nano>

=== HEADERS ===
<Key>: <value>        (authorization masked as "<scheme> <hide>", headers containing api-key/apikey/token/secret masked via HideAPIKey)

=== REQUEST BODY ===     (omitted for websocket transcripts)
<body, zstd-decoded for logging; truncated marker lines like "[DECOMPRESSED REQUEST BODY TRUNCATED]" or "[REQUEST BODY TRUNCATED: captured first N bytes]">

=== WEBSOCKET TIMELINE ===      (only for websocket requests)
=== API WEBSOCKET TIMELINE ===  (upstream websocket)
=== API REQUEST ===             (upstream request(s): per attempt block with Timestamp, Upstream URL, Method, Headers, Body)
=== API ERROR RESPONSE ===      (repeated per captured error)
HTTP Status: <n>
<error text>
=== API RESPONSE ===            (upstream response, with Timestamp of first response)
=== RESPONSE ===
Status: <n>
<response headers, one per line>

<response body, decompressed according to Content-Encoding gzip/deflate/br/zstd>
```
Streaming responses (Content-Type contains `text/event-stream`, or Content-Type unset and request body contains `"stream": true`/`"stream":true`) are written chunk by chunk to temp files and assembled at close with the same layout (response status/headers section last; status line omitted when not captured). Home mode forwards the rendered log to home instead of disk.
- The `GET /v0|v8/management/.../logs/requests/:id` endpoint finds files by the trailing `-<id8>.log`.
- Gin keys feeding the log: `API_REQUEST`/`API_RESPONSE` (+ sources for large bodies), `API_RESPONSE_ERROR`, `API_RESPONSE_TIMESTAMP`, `REQUEST_BODY_OVERRIDE`, `RESPONSE_BODY_OVERRIDE`, `WEBSOCKET_TIMELINE_OVERRIDE`.

### 9.3 Trace id
`X-CPA-TRACE-ID: <YYYYMMDDHHMMSS>-<authIndex>-<requestID>` is set on the response (at first write/flush) once an executor reports the selected credential's auth index (callback per attempt; last attempt wins). Not set for websocket upgrade responses.

### 9.4 internal/clienterror
- `HTTPStatusFromError(err)`: first `StatusCode()` on the chain >0; `context.Canceled` -> 499; `DeadlineExceeded` -> 504; else 0. `...Or(err, fallback)`.
- `IsRequestFault(status, err)` (request error, must not rotate or penalize credentials): false for 402/429; false for 401 whose body has `type: authentication_error`; false if body code is `model_not_found(_error)`; true when JSON body (paths `error.code, code, response.error.code, body.error.code`) has code in {`cyber_policy, context_length_exceeded, message_too_big, string_above_max_length, invalid_prompt, invalid_value, unsupported_value, invalid_request_error, previous_response_not_found`} or type (paths `error.type, type, response.error.type, body.error.type`) in {`invalid_request, invalid_request_error, bad_request_error, invalid_prompt`}; true for the plain-text "item with id ... not found ... items are not persisted when `store` is set to false" message; else true for status 400, 409, 413, 422.
- `IsClientCancellation(status, err)`: status 499, or `context.Canceled`, or error text contains `context canceled` / `client closed request`.

---

## 10. Management API


Sources: `internal/api/server_management.go`, `server_management_v8.go`, `server_routes.go`, `server_middleware.go`, `server_reload.go`, `server.go`, `internal/api/handlers/management/*.go`, `docs/management-api-v8.md`, `internal/managementasset/updater.go`, `internal/redisqueue/queue.go`.
Conventions: HTTP framework is gin. Errors are almost always `{"error": "<msg>"}`; some newer handlers use `{"error": "<code>", "message": "<detail>"}`. Success of simple setters is `{"status":"ok"}`. "Auth" below means the management middleware (section 1).

### 1. Management auth (handler.go, server.go, server_reload.go, config_load.go/parse.go)

#### Secret sources
- `remote-management.secret-key` in config (YAML key `secret-key`; also accepted under alias section `management.secret-key`). Stored as bcrypt (`bcrypt.DefaultCost`). On load, if non-empty and not `looksLikeBcrypt` (prefix `$2a$`, `$2b$`, `$2y$`), it is hashed and the hash is written back to the config file in place (comment-preserving, nested scalar update; uses `management` path if the file used that alias). Plaintext is never kept.
- Env `MANAGEMENT_PASSWORD` (trimmed; empty = unset). Read once at `NewHandler` and once in server ctor. Compared in plaintext with constant-time compare (not bcrypt). Setting it ALSO forces `allowRemote = true` (`allowRemoteOverride`), i.e. remote access is permitted regardless of `allow-remote`.
- Runtime local password (`optionState.localPassword`, set by TUI standalone/embedded mode via server option): accepted only when client is local (`127.0.0.1` or `::1`). Enables route registration like a secret does.
- Other config: `remote-management.allow-remote` (bool), `disable-control-panel`, `disable-auto-update-panel`, `panel-github-repository`, `base-url` (TUI client only, omitempty). Top-level `trusted-proxies` feeds `engine.SetTrustedProxies` (so `ClientIP()` honors X-Forwarded-For only from trusted proxies; on invalid value, proxies disabled).

#### Middleware behavior (`Handler.Middleware`, `AuthenticateManagementKey`)
Order for every request in the management group:
1. Always set response headers: `X-CPA-VERSION` (buildinfo.Version), `X-CPA-COMMIT`, `X-CPA-BUILD-DATE`, `X-CPA-SUPPORT-PLUGIN` (pluginhost.SupportPluginHeaderValue()).
2. `clientIP = c.ClientIP()`; `local = (ip == "127.0.0.1" || ip == "::1")`.
3. Extract key: `Authorization` header; if present, split on first space; if scheme equals `bearer` (case-insensitive) use the remainder; otherwise use the WHOLE header value as the key (raw key without "Bearer " also works). If key still empty, use `X-Management-Key`. No query-param key.
4. Decision (`AuthenticateManagementKey(ip, local, provided)`), exact order:
   a. Ban check: per-IP record; if `blockedUntil` set and now < it: `403 {"error":"IP banned due to too many failed attempts. Try again in <dur>"}` where `<dur>` = remaining time rounded to seconds in Go Duration format (e.g. `29m59s`). If ban expired: reset count and blockedUntil.
   b. `!local && !allowRemote` -> `403 {"error":"remote management disabled"}` (no failure counted).
   c. `secret-key == "" && envSecret == ""` -> `403 {"error":"remote management key not set"}` (no failure counted).
   d. `provided == ""` -> counts a failure; `401 {"error":"missing management key"}`.
   e. If local and localPassword set and equal (constant-time) -> success, reset counter.
   f. If envSecret set and equal (constant-time) -> success, reset.
   g. Else if `secret-key == ""` or `bcrypt.CompareHashAndPassword(secret-key, provided)` fails -> counts failure; `401 {"error":"invalid management key"}`.
   h. Else success, reset counter.
5. Failure accounting: `maxFailures = 5`, `banDuration = 30m`. Per client IP: `count++`, `lastActivity=now`; at count >= 5 set `blockedUntil = now+30m` and count=0. Success resets count and blockedUntil. Local clients are NOT exempt from ban. A background goroutine every 1h purges entries not banned and idle > 2h. Ban is in-memory only.
6. Aborts use `AbortWithStatusJSON(status, {"error": msg})`.
- Note: the key is bcrypt-compared on each request (cost 10). Rust: constant-time compare for env/local pw; bcrypt verify for hash.

#### Availability gate (before auth; `managementAvailabilityMiddleware`)
Returns bare `404` (empty body, `AbortWithStatus(404)`) when: server/config nil; `home.enabled` is true (home mode disables ALL local management endpoints, both v0 and v8, and `/management.html`); or `managementRoutesEnabled` is false.
- `managementRoutesEnabled` = `secret-key != "" || MANAGEMENT_PASSWORD set || localPassword != ""` at startup.
- Lazy registration: routes are registered only once (`managementRoutesRegistered` CAS) via `registerManagementRoutes()`. At startup, only if a secret exists. On config hot reload (`server_reload.go`): if env secret exists -> register (if needed) and enable; else if secret went empty->non-empty -> register + enable; non-empty->empty -> disable (flag false; routes stay registered but return 404); otherwise `enabled = secret non-empty`. `redisqueue.SetEnabled(enabled || home.enabled)` accordingly (the usage queue only collects while enabled; disabling clears queues).
- `engine.NoRoute(pluginManagementNoRoute)`: unmatched `/v0/management/...` paths pass through availability + auth, then plugin-defined routes (`ServePluginAuthURL` for `<provider>-auth-url` and `GET /v8/management/oauth/auth-url?provider=<plugin>`, then `pluginHost.ServeManagementHTTP`), else 404. `/v0/resource/plugins/*` goes to `pluginHost.ServeResourceHTTP` (no mgmt auth, 404 if home enabled).
- Home heartbeat middleware (`server_middleware.go`) exempts `/v0/management*`, `/v8/management*`, `/v0/resource/plugins/*`, `/management.html` from the 503 gate. Safe-mode middleware (example api-keys) allows `/management.html?safe-mode=configure`; for GET/HEAD of `/` or `/management.html` otherwise serves a warning page; proxy paths get `403 {"error":"unsafe_example_api_key","message":...}` with header `X-CPA-SAFE-MODE: example-api-key`.
- CORS (global): `Access-Control-Allow-Origin: *`, methods `GET, POST, PUT, PATCH, DELETE, OPTIONS`, headers `*`; OPTIONS -> 204.
- Unauthenticated routes: both OAuth callback routes (below) use only the availability gate (no key; they validate a pending `state`).

### 2. Control panel (server_management.go `serveManagementControlPanel`, server_routes.go, managementasset/updater.go, util)

- `GET /management.html` registered unconditionally in `setupRoutes` (not behind management auth, not behind secret). Behavior:
  - 404 (empty) if cfg nil, `home.enabled`, or `remote-management.disable-control-panel`.
  - `filePath = managementasset.FilePath(configFilePath)`; empty -> 404.
  - `stat`: not exist -> synchronously call `EnsureLatestManagementHTML(context.Background(), StaticDir, cfg.proxy-url, cfg.remote-management.panel-github-repository)`; if it returns false -> 404. Other stat error -> 500 (empty). Then `c.File(filePath)` (static file, gin sets content type by extension/sniff).
- Paths: `MANAGEMENT_STATIC_PATH` env overrides: if its basename equals `management.html` (case-insens.) file = that path, dir = its dirname; else dir = value, file = dir/management.html. Otherwise dir = `$WRITABLE_PATH/static` if `WRITABLE_PATH` env set; otherwise `<dir of config file>/static` (if config path is a directory, `<config path>/static`). File name is `management.html`.
- Asset source: default release API `https://api.github.com/repos/router-for-me/Cli-Proxy-API-Management-Center/releases/latest`. Release asset chosen by name equals (case-insens.) `management.html`; download from asset `browser_download_url`. Request headers: release JSON: `Accept: application/vnd.github+json`, `User-Agent: CLIProxyAPI-management-updater`, plus `Authorization: Bearer <token>` if `GITHUB_TOKEN` / `github_token` env (else `GITSTORE_GIT_TOKEN` when `GITSTORE_GIT_URL` contains github.com). Download uses only the User-Agent. HTTP client timeout 15s, uses config `proxy-url`. Download size cap 50 MiB.
- `panel-github-repository` resolution (`resolveReleaseURL`): empty/unparseable/no host -> default. Host `api.github.com`: append `/releases/latest` if path lacks that suffix. Host `github.com` with >= 2 path segments: `https://api.github.com/repos/<owner>/<repo minus .git>/releases/latest`. Any other host -> default.
- SHA-256 verification: asset JSON field `digest` (e.g. `sha256:<hex>`); `parseDigest` strips up to first `:` and lowercases. Flow of `EnsureLatestManagementHTML` (coalesced with singleflight keyed by local path; global throttle: at most one attempt per 30s, otherwise silently skipped):
  1. mkdir static dir (0755). Compute local file sha256 (hex).
  2. Fetch release info. On error: if local file missing, try fallback; else warn and keep local.
  3. If remote digest and local hash both non-empty and equal (case-insens.) -> up to date, stop.
  4. Download asset, hash it. On download error: fallback if local missing.
  5. If remote digest non-empty and != downloaded hash -> log error, ABORT update (keeps old file). If release gave no digest, the download is accepted unverified.
  6. Atomic write: temp file `management-*.html` in same dir, chmod 0644, rename over target.
  7. Returns `os.Stat(local)` success (true if the file exists after attempt, even if update failed).
- Fallback (only when local file missing and GitHub path failed): download `https://cpamc.router-for.me/` (no digest verification, warns), write atomically.
- Auto-updater (`StartAutoUpdater`, started by main for normal server and standalone TUI; once via `sync.Once`): runs immediately then every 3h; skipped per iteration (debug log) when config unavailable, `home.enabled`, `disable-control-panel`, or `disable-auto-update-panel`. Uses the latest hot-reloaded config via `SetCurrentConfig`. With `disable-auto-update-panel: true` the panel is only downloaded lazily on first `GET /management.html` when missing.
- No v8 route for panel; panel talks to `/v0/management` or `/v8/management` using the key.

### 3. Endpoint table

Base group middleware: availability gate, then auth; v8 group additionally sets context flag `management.config-v8` = true (makes config saves use v8 layout via `SaveConfigPreserveComments(..., v8=true)`). "mgmt" = auth required.

#### 3.1 Non-management routes registered alongside (no mgmt auth)
| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET | /healthz, HEAD /healthz | inline | `{"status":"ok"}` (HEAD 200 no body) |
| GET | /management.html | serveManagementControlPanel | control panel |
| GET | /anthropic/callback | inline (server_routes.go) | provider redirect; writes callback file if state pending; always 200 HTML |
| GET | /codex/callback | inline | same, provider codex |
| GET | /antigravity/callback | inline | same, provider antigravity |
| GET | /callback, /devin/callback | devinCallbackHandler | devin redirect; header `Cache-Control: no-store`; 400 `{"error":"code or error is required"}` if neither code nor error; 400 `{"error":"invalid or expired OAuth callback"}` if state not pending/invalid; else 200 HTML |
| GET | /v0/management/oauth-callback | GetOAuthCallback | availability gate only (no key) |
| POST | /v0/management/oauth-callback | PostOAuthCallback | same |
| GET | /v8/management/oauth/callback | GetOAuthCallback | same |
| POST | /v8/management/oauth/callback | PostOAuthCallback | same |

Success HTML for provider redirect routes: `<html><head><meta charset="utf-8"><title>Authentication successful</title><script>setTimeout(function(){window.close();},5000);</script></head><body><h1>Authentication successful!</h1><p>You can close this window.</p><p>This window will close automatically in 5 seconds.</p></body></html>` with `Content-Type: text/html; charset=utf-8`. These redirect routes read `code`, `state`, `error` (fallback `error_description`) from query, and call `WriteOAuthCallbackFileForPendingSession`; failures are swallowed (except devin).

#### 3.2 /v8/management (all mgmt unless noted)
| Method | Path | Handler | Purpose |
|---|---|---|---|
| GET,PUT,PATCH | /config | ConfigV8 | whole config as JSON |
| GET,PUT | /config.yaml | ConfigV8 | whole config as YAML |
| GET,PUT,PATCH,DELETE | /config/*path | ConfigV8 | subtree by YAML key path |
| GET | /server/latest-version | GetLatestVersion | latest GitHub release tag |
| POST | /requests/api-call | APICall | proxy an HTTP call with credential token |
| POST | /routing/cooldown/reset | ResetQuota | clear cooldown for auth_index |
| GET | /routing/model-definitions/:channel | GetStaticModelDefinitions | static model list |
| GET | /observability/logs | GetLogs | tail/incremental main.log |
| DELETE | /observability/logs | DeleteLogs | clear logs |
| GET | /observability/logs/errors | GetRequestErrorLogs | list error-*.log |
| GET | /observability/logs/errors/:name | DownloadRequestErrorLog | download error log |
| GET | /observability/logs/requests/:id | GetRequestLogByID | download request log by id |
| GET | /observability/usage/api-keys | GetAPIKeyUsage | per api-key success/failed |
| GET | /observability/usage/queue | GetUsageQueue | pop queued usage events |
| GET | /credentials | ListAuthFiles | list credentials |
| POST | /credentials | UploadAuthFile | upload credential JSON |
| DELETE | /credentials | DeleteAuthFile | delete credential(s) |
| GET | /credentials/models | GetAuthFileModels | models for a credential |
| GET | /credentials/download | DownloadAuthFile | download credential JSON |
| PATCH | /credentials/status | PatchAuthFileStatus | enable/disable |
| PATCH | /credentials/fields | PatchAuthFileFields | edit metadata fields |
| POST | /credentials/refresh | RefreshAuthFiles | force token refresh |
| POST | /oauth/import | ImportOAuthV8 | `?provider=vertex` -> ImportVertexCredential |
| GET | /oauth/auth-url | StartOAuthV8 | start login by `?provider=` |
| GET | /oauth/status | GetAuthStatus | poll login by `?state=` |
| DELETE | /oauth/session | CancelAuthSession | cancel by `?state=` |
| GET | /plugins | ListPlugins | list plugins |
| DELETE | /plugins/:id | DeletePlugin | delete plugin |
| GET | /plugins/store | ListPluginStore | store catalog |
| POST | /plugins/store/:id/install | InstallPluginFromStore | install/update |
| GET | /plugins/:id/quota | GetPluginQuota | fetch quota (`?auth_index=`) |
| POST | /plugins/:id/quota | FetchPluginQuota | fetch quota (JSON body) |
| DELETE | /plugins/:id/quota | ResetPluginQuota | reset quota |
(Plugin enable, plugin config, `/quota/providers`, `/quota/fetch`, `/quota/reset`, `/plugins/:id/quota/reset` exist only in v0.)

#### 3.3 /v0/management (all mgmt except oauth-callback)
| Method | Path | Handler |
|---|---|---|
| GET | /config | GetConfig (full runtime Config struct as JSON, same serialization as config yaml/json tags) |
| GET,PUT | /config.yaml | GetConfigYAML, PutConfigYAML |
| GET | /latest-version | GetLatestVersion |
| GET | /plugins | ListPlugins |
| GET | /plugin-store | ListPluginStore |
| POST | /plugin-store/:id/install | InstallPluginFromStore |
| DELETE | /plugins/:id | DeletePlugin |
| PATCH | /plugins/:id/enabled | PatchPluginEnabled |
| GET,PUT,PATCH | /plugins/:id/config | GetPluginConfig, PutPluginConfig, PatchPluginConfig |
| GET,POST,DELETE | /plugins/:id/quota | GetPluginQuota, FetchPluginQuota, ResetPluginQuota |
| POST | /plugins/:id/quota/reset | ResetPluginQuota |
| GET,PUT,PATCH | /debug | GetDebug / PutDebug |
| GET,PUT,PATCH | /logging-to-file | |
| GET,PUT,PATCH | /logs-max-total-size-mb | |
| GET,PUT,PATCH | /error-logs-max-files | |
| GET,PUT,PATCH | /usage-statistics-enabled | |
| GET,PUT,PATCH,DELETE | /proxy-url | |
| POST | /api-call | APICall |
| GET,PUT,PATCH | /quota-exceeded/switch-project | |
| GET,PUT,PATCH | /quota-exceeded/switch-preview-model | |
| POST | /reset-quota | ResetQuota |
| GET | /quota/providers | GetQuotaProviders |
| POST | /quota/fetch | FetchCredentialQuota |
| POST | /quota/reset | ResetCredentialQuota |
| GET,PUT,PATCH,DELETE | /api-keys | client keys (string list) |
| GET | /api-key-usage | GetAPIKeyUsage |
| GET | /usage-queue | GetUsageQueue |
| GET,PUT,PATCH,DELETE | /gemini-api-key, /interactions-api-key, /claude-api-key, /codex-api-key, /xai-api-key, /meta-api-key, /vertex-api-key, /openai-compatibility | provider key lists |
| GET,PUT,PATCH,DELETE | /oauth-excluded-models, /oauth-model-alias, /oauth-request-scoped-errors | per-channel maps |
| GET,DELETE | /logs | GetLogs, DeleteLogs |
| GET | /request-error-logs, /request-error-logs/:name, /request-log-by-id/:id | |
| GET,PUT,PATCH | /request-log | request-log bool |
| GET,PUT,PATCH | /ws-auth | |
| GET,PUT,PATCH | /request-retry, /max-retry-credentials, /max-retry-interval | ints |
| GET,PUT,PATCH | /force-model-prefix | bool |
| GET,PUT,PATCH | /routing/strategy | |
| GET | /auth-files, /auth-files/models, /auth-files/download | |
| GET | /model-definitions/:channel | GetStaticModelDefinitions |
| POST,DELETE | /auth-files | UploadAuthFile, DeleteAuthFile |
| PATCH | /auth-files/status, /auth-files/fields | |
| POST | /auth-files/refresh | |
| POST | /vertex/import | ImportVertexCredential |
| GET | /anthropic-auth-url, /codex-auth-url, /antigravity-auth-url, /kimi-auth-url, /kimi-ai-auth-url, /xai-auth-url, /devin-auth-url, /meta-auth-url | RequestXToken (plus plugin `<id>-auth-url` via NoRoute) |
| GET | /get-auth-status | GetAuthStatus |
| DELETE | /oauth-session | CancelAuthSession |
| GET,POST | /oauth-callback | no key |

### 4. Endpoint details

#### 4.1 v8 config tree (config_v8.go) - `ConfigV8` handles /config, /config.yaml, /config/*path
Whole handler holds `h.mu` and works on the persisted YAML file (`h.configFilePath`), not the in-memory struct. Pipeline:
1. Read file (fail: `500 {"error":"read_failed"}`). `config.NormalizeConfigLayout(data, true)` converts legacy layout to v8 layout in memory (fail: `500 {"error":"invalid_config","message":...}`). Parse as YAML node (fail `500 {"error":"invalid_config"}`).
2. `path = Trim(c.Param("path"), "/")`; `parts = split on "/"` (empty -> whole document). A trailing-slash path `/config/` is the whole doc. Paths address mapping keys only, never array indexes. `yamlRequest = FullPath ends with "/config.yaml"`.
3. GET:
   - JSON GET: first redact TURN secrets: for each item of `oauth.providers.codex.live-media-relay.ice-servers`, delete keys `username` and `credential`.
   - Node lookup by key path; missing -> `404 {"error":"not_found"}`. Sets `Cache-Control: no-store`.
   - `/config.yaml` GET returns the whole normalized YAML bytes, `Content-Type: application/yaml; charset=utf-8` (not redacted). `/config.yaml` ignores subpath (route has none).
   - JSON: node decoded to generic value and returned directly (any JSON type: object, list, scalar, null; no envelope). Decode failure `500 {"error":"decode_failed"}`. GET never writes the file or migrates it.
4. DELETE (`/config/*path` only): empty path -> `400 {"error":"cannot_delete_config"}`; key missing -> `404 {"error":"not_found"}`. Deletion removes the key and prunes only ancestors that became empty maps; other explicit empty maps remain.
5. PUT/PATCH (body read fully; no envelope: body IS the value):
   - read error `400 {"error":"invalid_body"}`; for non-YAML request, not valid JSON -> `400 {"error":"invalid_json"}`; YAML parse failure or empty -> `400 {"error":"invalid_body"}`. (`/config.yaml` PUT body parsed as YAML.)
   - root target (`/config`, `/config.yaml`) requires a mapping -> `400 {"error":"config_must_be_object"}`.
   - Navigate path creating missing intermediate mappings; if an intermediate is non-mapping or part empty -> `400 {"error":"invalid_path"}`.
   - PUT replaces target node wholesale. PATCH: if both mapping, recursive merge key by key (objects merge, lists and scalars replace; explicit `null` is retained, it is NOT a delete; DELETE is the removal op); otherwise replace.
   - For JSON writes (non-YAML, non-DELETE), omitted TURN `username`/`credential` in an `ice-servers` entry are preserved from the previous doc by matching entries with identical `urls` list (each old entry used once); explicit empty string/null clears.
6. Read-only guard (applies to all mutations): fields `credentials/concurrency/lifecycle-config-revision`, `credentials/concurrency/observation-barrier-revision`, `plugins/auth-revision` must be unchanged else `400 {"error":"read_only_field","field":"<slash path>"}`.
7. Validation: marshal to YAML (fail `400 invalid_config`); `config.ParseConfigBytes` fail -> `422 {"error":"invalid_config","message":...}`; `config.ValidateV8Config` fail -> `400 {"error":"invalid_config","message":...}` (this rejects legacy field names on v8 writes).
8. Persist: write normalized doc to temp file `.config-v8-*.yaml` in config dir; for PUT/PATCH run `SaveConfigPreserveComments(tmp, parsedConfig, v8=true)` (projects typed config back, materializes defaults); for DELETE keep the validated tree as-is. Then `WriteConfig(path, data)` overwrites in place (same inode via O_TRUNC, no rename, for Docker single-file mounts; applies `NormalizeCommentIndentation`; fsync). Errors `500 {"error":"write_failed","message":...}`.
9. Success: `h.cfg = parsed` (with `Home` runtime state carried over from old cfg), snapshot + async reload hook; response `200 {"status":"ok","config-version":8}`. Temp file removed in all cases.
10. Docs: v8 key tree mirrors `config.example.yaml` (e.g. `access/api-keys`, `api-keys/<provider>`, `client/codex/optimize-multi-agent-v2`, `observability/logs/debug`, `routing/retry/request-retry`, `plugins/configs/<id>`). `client.codex.optimize-multi-agent-v2` supersedes legacy aliases (`providers.codex...`, `oauth.providers.codex...`, flat `codex...`); client path wins incl. explicit false/null.

#### 4.2 v0 config endpoints (config_basic.go, handler.go)
- `GET /config` -> `200` serialized Config (or `{}` if nil).
- `GET /config.yaml`: raw file bytes, headers `Content-Type: application/yaml; charset=utf-8`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`; 404 `{"error":"not_found","message":"config file not found"}`; 500 `{"error":"read_failed","message":...}`.
- `PUT /config.yaml`: body = raw YAML (any Content-Type). Errors: `400 {"error":"invalid_yaml","message":...}` (not parseable as Config struct), `500 write_failed` (temp validation file), `422 {"error":"invalid_config","message":...}` (`LoadConfigOptional(tmp,false)` fails), `500 {"error":"write_failed","message":"failed to write config"}`, `500 {"error":"reload_failed",...}`. Success `200 {"ok":true,"changed":["config"]}`. Writes raw body verbatim to disk (comment indentation normalized), reloads into handler. Accepts legacy/new/mixed layout.
- `GET /latest-version` (also v8 `/server/latest-version`): GET `https://api.github.com/repos/router-for-me/CLIProxyAPI/releases/latest` with `Accept: application/vnd.github+json`, `User-Agent: CLIProxyAPI`, optional GitHub token header, 10s timeout, proxy-url. Success `200 {"latest-version":"<tag_name or name>"}`. Errors all `502` with `{"error":"request_failed|unexpected_status|decode_failed|invalid_response","message":...}`; `500 {"error":"request_create_failed","message":...}`.
- Scalar settings (all `GET` returns `{"<key>": value}`; `PUT`/`PATCH` body `{"value": <v>}`; bad/missing value `400 {"error":"invalid body"}`; success `{"status":"ok"}` after persisting config file and async reload):
  | Path | JSON key | Type | Notes |
  |---|---|---|---|
  | /debug | debug | bool | |
  | /logging-to-file | logging-to-file | bool | |
  | /logs-max-total-size-mb | logs-max-total-size-mb | int | negative -> 0 |
  | /error-logs-max-files | error-logs-max-files | int | negative -> 10 |
  | /usage-statistics-enabled | usage-statistics-enabled | bool | |
  | /proxy-url | proxy-url | string | DELETE sets "" |
  | /request-log | request-log | bool | |
  | /ws-auth | ws-auth | bool | |
  | /request-retry | request-retry | int | |
  | /max-retry-credentials | max-retry-credentials | int | |
  | /max-retry-interval | max-retry-interval | int (seconds) | |
  | /force-model-prefix | force-model-prefix | bool | |
  | /quota-exceeded/switch-project | switch-project | bool | |
  | /quota-exceeded/switch-preview-model | switch-preview-model | bool | |
  | /routing/strategy | strategy | string | normalized: ""/round-robin/roundrobin/rr -> `round-robin`; weighted-round-robin/weightedroundrobin/wrr -> `weighted-round-robin`; fill-first/fillfirst/ff -> `fill-first`; else `400 {"error":"invalid strategy"}` |
- Persist helper `persist`: save with comments preserved; failure `500 {"error":"failed to save config: <err>"}`; success `200 {"status":"ok"}`, then async reload hook on a cloned snapshot with monotonically increasing generation (stale generations skipped).

#### 4.3 v0 list endpoints (config_lists.go, config_auth_index.go)
- String list `/api-keys`: GET `{"api-keys":[...]}`. PUT body: JSON array of strings OR `{"items":[...]}` (empty items -> 400 `{"error":"invalid body"}`); bad read `400 failed to read body`. PATCH: `{"old","new"}` (replace first match or append if old absent), or `{"index":n,"value":"..."}`; else `400 {"error":"missing fields"}`. DELETE: `?index=n` or `?value=<str>`; else `400 {"error":"missing index or value"}`.
- Provider key lists (`gemini-api-key`, `interactions-api-key`, `claude-api-key`, `codex-api-key`, `xai-api-key`, `meta-api-key`, `vertex-api-key`): GET `{"<name>": [ {...config entry fields (kebab-case yaml/json tags: api-key, base-url, proxy-url, prefix, priority, weight, headers, excluded-models, models, disable-cooling, request-retry, request-scoped-errors, ...), "auth-index": "<live auth index, omitted if none>"} ]}`. PUT body: array or `{"items":[...]}` (full replace, then Sanitize*; weight validated `400 {"error":"<name>[i].weight: ..."}`; claude also `fingerprint-profile` validated). PATCH body `{"index":n | "match":"<api-key>", "value":{partial entry}}`; `match` may be narrowed by query `?base-url=`; multiple matches `400 {"error":"multiple items match; index is required"}`; none `404 {"error":"item not found"}`; invalid body `400 {"error":"invalid body"}`. A patched entry left with empty api-key and base-url is removed. DELETE: `?api-key=<k>[&base-url=<b>]` or `?index=n`; none matched `404 item not found` (gemini) / silently ok (claude, others vary); ambiguous `400 {"error":"multiple items match api-key; base-url is required"}`; neither param `400 {"error":"missing api-key or index"}`.
- `/openai-compatibility`: GET `{"openai-compatibility":[{name, priority, disabled, prefix, base-url, api-key-entries:[{..., "auth-index"}], models, headers, support-prompt-cache-key, disable-cooling, request-retry, request-scoped-errors, auth-index}]}`. PATCH `{"name"|"index", "value":{...}}`. DELETE `?name=` or `?index=`; else `400 {"error":"missing name or index"}`.
- Channel maps `/oauth-excluded-models` (key `oauth-excluded-models`, map provider->[]string), `/oauth-model-alias` (map channel->[]{name,alias,fork}), `/oauth-request-scoped-errors` (map channel->[]rules). GET `{"<key>": map}`; PUT body = map or `{"items": map}`; PATCH `{"provider"|"channel": "x", "models"|"aliases"|"rules": [...]}` (empty normalized list removes the channel, `404 {"error":"provider not found"|"channel not found"}` if absent; empty name `400 invalid provider|invalid channel`); DELETE `?provider=` (excluded) or `?channel=`/`?provider=`; missing -> `400 missing provider|missing channel`. Keys lowercased.
- Applied to disabled-cooling patches: `disable-cooling` accepts bool or null via `applyDisableCoolingPatch` (400 on invalid).

#### 4.4 Credentials / auth files (auth_files*.go) - v8 `/credentials*`, v0 `/auth-files*`
- `GET list`: query `name`, `auth_index` (filters), optional pagination `page` (>=1) and `page_size` (>=1, default `defaultAuthFilesPageSize`); invalid -> `400 {"error":"page must be a positive integer"|"page_size must be a positive integer"}`. Response `{"observed_at": RFC3339Z, "files":[entry...]}`; with pagination adds `"total","page","page_size","has_more"` and uses stable order (`compareAuthFileListOrder`), only listable auths; without pagination sorted by lowercase name. If no auth manager, lists `*.json` from auth-dir on disk (`source:"file"`, fewer fields). Entry fields: `id, auth_index, name (file name or id), type, provider, label, status, status_message, disabled, unavailable, runtime_only, source ("file"|"memory"), size, success, failed, recent_requests, quota{observed_at?,signals{}}, model_quotas{} (if any), supports_quota, quota_provider, quota_probe, email, project_id, account_type, account, created_at, modtime, updated_at, last_refresh, next_retry_after, path, id_token (codex claims), priority, note, weight, websockets, request_retry, cooldowns (snapshot or null when home mode)`. Runtime-only disabled auths and removed-from-disk lingering auths are hidden.
- `GET models`: `?name=<file name or id>` required (`400 {"error":"name is required"}`); `200 {"models":[{"id","display_name"?,"type"?,"owned_by"?}]}` from global registry for that client id.
- `GET download`: `?name=x.json`; `400 {"error":"invalid name"}` / `"name must end with .json"`; `404 {"error":"file not found"}`; success `200`, `Content-Type: application/json`, `Content-Disposition: attachment; filename="<name>"`, raw file bytes.
- `POST upload` (503 `{"error":"core auth manager unavailable"}` if no manager): either multipart (all file parts across all field names, sorted by field key; each filename basename must end `.json`) or raw JSON body with `?name=<x>.json`. Single multipart file: `200 {"status":"ok"}`; multiple: all ok `200 {"status":"ok","uploaded":N,"files":[names]}`, any failed `207 {"status":"partial","uploaded":N,"files":[...],"failed":[{"name","error"}]}`. Errors: `400 {"error":"invalid multipart form: ..."}`, `{"error":"file must be .json"}`, multipart without files `{"error":"no files uploaded"}`, raw: `{"error":"invalid name"}`, `{"error":"name must end with .json"}`, `{"error":"failed to read body"}`, write `500 {"error":"<msg>"}`. File written to `auth-dir/<basename>` mode 0600 after building the auth record from the data; registers/updates auth in manager; runs post-auth persist hook.
- `DELETE`: `?all=true|1|*` deletes every `*.json` in auth-dir -> `200 {"status":"ok","deleted":N}`. Otherwise names from repeated `?name=` or JSON body `{"name":"x"}` / `{"names":[...]}` / `["a","b"]`. One name: `200 {"status":"ok"}` or error status (400 invalid name, 404 `{"error":"auth file not found"}` presumably errAuthFileNotFound, 409 plugin-virtual auth, 500). Multiple: ok `200 {"status":"ok","deleted":N,"files":[...]}`, partial `207 {"status":"partial","deleted":N,"files":[...],"failed":[{"name","error"}]}`. None: `400 {"error":"invalid name"}`.
- `PATCH status` body `{"name","auth_index"?,"disabled":bool}`: `400 invalid request body | name is required | disabled is required`; `404 {"error":"auth file not found"}`; `409` plugin virtual child; config-API-key-backed auths are "disabled" by adding/removing excluded-model `"*"` in config -> `200 {"status":"ok","disabled":b,"via":"config:excluded-models","excluded_pattern":"*"}`; normal `200 {"status":"ok","disabled":b}`.
- `PATCH fields` body: JSON object with required `"name"` (file name or auth id) plus arbitrary metadata fields to set (dotted paths allowed, e.g. `"headers.X-Foo"`; `null` removes). Special: `weight` (integer JSON number, validated; null removes), `headers` (object merged/replaced), `request_retry` (int or null), `priority`, `note`, `websockets`, `disabled`, `prefix`, etc.; root keys canonicalized via `CanonicalCredentialMetadataKey`. Errors: `400 {"error":"invalid request body"|"name is required"|"field name is required"|"invalid field <p>"|"weight must be an integer"|"weight does not support nested fields"|"no fields to update"}`, `404 auth file not found`, `409` virtual auth, `500 failed to update auth`. Success `200 {"status":"ok"}`.
- `POST refresh`: query or JSON `{"name","auth_index","all":bool}`; `?all=true`. all -> `200 {"ok":true,"results":<ForceRefreshAll result>}`; single -> `200 {"ok":true,"auth":<refreshed auth object>}`; `400 {"error":"name or all=true is required"}`, `404 auth file not found`, `500 {"error":msg}`, `503` no manager.
- `POST oauth/import?provider=vertex` (v0 `POST /vertex/import`): multipart field `file` (service account JSON), optional `location` (form field or query, default `us-central1`). Errors: `503 config unavailable|auth directory not configured`; `400 {"error":"file required"}`, `failed to read file: ...`, `{"error":"invalid json","message":...}`, `{"error":"invalid service account","message":...}`, `{"error":"project_id missing"}`; `500 {"error":"save_failed","message":...}`. Saved as `vertex-<sanitized project_id>.json` (sanitize `/ \ :` -> `_`, space -> `-`). Success `200 {"status":"ok","auth-file":"<saved path>","project_id","email","location"}`. v8 import without/other provider: `400 {"error":"provider is required"}` / `404 {"error":"provider_not_found"}`.

#### 4.5 Model definitions, cooldown, usage, api-call
- `GET model-definitions/:channel` (v0 also accepts `?channel=`): `400 {"error":"channel is required"}`; unknown `400 {"error":"unknown channel","channel":"<c>"}`; success `200 {"channel":"<lowercase>","models":[ModelInfo...]}` from static registry.
- `POST routing/cooldown/reset` (v0 `/reset-quota`): body `{"auth_index":"..."}`; `503 core auth manager unavailable`; `400 invalid request body | auth_index is required`; `404 {"error":"auth not found"}`; `500 {"error":"failed to reset quota: ..."}`; success `200 {"status":"ok","auth_index":"...","models":<manager models result>}`.
- `GET observability/usage/api-keys` (v0 `/api-key-usage`): `200 { "<provider|compat_name lowercased or 'unknown'>": { "<base_url>|<api_key>": {"success":int,"failed":int,"recent_requests":[{...RecentRequestBucket (success, failed, time label)}]} } }`; only in-memory auths with account kind `api_key`; entries with same composite key merge (bucket-wise sums). `503 core auth manager unavailable`.
- `GET observability/usage/queue` (v0 `/usage-queue`): query `count` default 1, must be positive int else `400 {"error":"count must be a positive integer"}`. Pops up to `count` oldest records from the in-memory usage queue (destructive read; items older than retention, default 60s max 3600s, expire; queue only active while management enabled or home enabled). Response `200` JSON array; each element is the stored payload embedded as raw JSON if valid, else as JSON string. Payload fields (usage detail): timestamp, latency_ms, ttft_ms, source, auth_index, tokens{input_tokens,output_tokens,reasoning_tokens,cached_tokens,cache_read_tokens,cache_creation_tokens,total_tokens}, failed, stream, provider, executor_type, model, alias, api_key, request_id/trace_id, session ids, client ip/user agent, etc. (see redisqueue/plugin.go for the exact struct).
- `POST requests/api-call` (v0 `/api-call`): body `{"auth_index"|"authIndex"|"AuthIndex": "<opt>", "method": "GET", "url": "https://...", "proxy_url": "<opt>", "header": {k: v}, "data": "<raw string body>"}`. Validation (all `400 {"error":...}`): `invalid body`, `missing method`, `missing url`, `invalid url` (needs scheme+host), `invalid proxy_url`, `failed to build request`; token substitution failures: `auth token refresh failed`, `auth credential not found for auth_index`, `auth token not found`. `$TOKEN$` in header values or `data` replaced by credential token (antigravity/meta/xai providers refresh OAuth tokens first; else metadata `access_token`, then attributes `api_key`, `session_token`, then metadata `token/id_token/cookie`); in JSON `data` the token is JSON-escaped. Header `Host` sets `req.Host`. Proxy priority: request `proxy_url` > credential proxy > global `proxy-url` > direct (env proxies unused; `direct`/`none` bypass). Timeout 60s. Upstream failure `502 {"error":"request failed"}`, read failure `502 {"error":"failed to read response"}`. Success is always `200 {"status_code":int,"header":{name:[values]},"body":"<string>"}` regardless of upstream status.

#### 4.6 Plugins (plugins.go, plugin_store.go, plugin_quota.go)
- `GET plugins`: `200 {"plugins_enabled":bool,"plugins_dir":str,"plugins":[{id,path,configured,registered,enabled,effective_enabled,supports_oauth,oauth_provider,supports_quota,quota_provider?,logo,config_fields:[{name,type,enum_values,description}],menus:[{path,menu,description}],metadata:{name,version,author,github_repository,logo,config_fields}|null}]}`. Sorted by id; union of files on disk, config entries, registered plugins; `effective_enabled = plugins.enabled && enabled && registered`. All strings HTML-sanitized (`htmlsanitize`). `500 {"error":"plugin_directory_invalid"|"plugin_discovery_failed","message"}`.
- `DELETE plugins/:id`: success `200 {"status":"deleted","id","path","file_deleted":bool,"configured_removed":bool,"restart_required":false}`; `404 {"error":"plugin_not_found","message":"plugin not found"}`; loaded and non-unloadable -> `409 {"error":"plugin_delete_requires_restart","message":"loaded plugin cannot be deleted while the server is running","restart_required":true}`; `500 plugin_delete_failed|config_save_failed`.
- v0 only: `PATCH plugins/:id/enabled` body `{"enabled":bool}` (`400 {"error":"invalid_body","message":"enabled is required"}`) -> `200 {"status":"ok"}`. `GET plugins/:id/config` -> stored `plugins.configs.<id>` object (`{}` if none; `404 plugin_not_found`). `PUT plugins/:id/config` body JSON object replaces; `PATCH` shallow-merge (null value deletes key); bad id `400 invalid_plugin_id`, non-object `400 invalid_body "body must be a JSON object"`, invalid `400 invalid_config`; success `{"status":"ok"}`.
- `GET plugins/store` (v0 `/plugin-store`): `200 {"plugins_enabled","plugins_dir","sources":[{id,name,url}],"source_errors":[{source_id,source_name,source_url,message}]?,"plugins":[{store_id ("<source>/<id>"),source_id,source_name,source_url,id,name,description,author,version,repository,install_type,auth_required,auth_configured,platforms?:[{goos,goarch}],logo?,homepage?,license?,tags?,installed,installed_version,installed_source_id?,install_source_status?,path,configured,registered,enabled,effective_enabled,update_available}]}`. All source failures -> `502 {"error":"plugin_store_registry_failed","message"}`; `500 plugin_store_source_invalid`.
- `POST plugins/store/:id/install` (v0 `/plugin-store/:id/install`): optional `?source=<source id>`, version from `?version=` and/or JSON body `{"version":"..."}` (mismatch `400 {"error":"invalid_request","message":...}`). Success `200 {"status":"installed","source_id","source_name","source_url","id","version","install_type","path","plugins_enabled","restart_required"}` (also enables it in config). Errors: `404 plugin_not_found|plugin_store_source_not_found`, `409` (loaded plugin overwrite: `{"error":"plugin_update_requires_restart","message","restart_required":true}`, or ambiguous source), `429 {"error":"plugin_store_rate_limited","message","retry_after":sec,"retry_at":RFC3339}` with `Retry-After` header, `502 plugin_manifest_invalid|plugin_install_failed`, `500 config_update_failed|config_save_failed|plugin_manifest_failed`.
- Quota: `GET plugins/:id/quota?auth_index=` (also `authIndex`); `POST plugins/:id/quota` body `{"auth_index"|"authIndex"|"AuthIndex"}`; both -> plugin QuotaFetchResponse `200 {"subscription"?,"summary"?,"serverTimeOffsetMs"?,"groups"?}` (snake alias accepted on input); errors `400 auth_index is required|invalid request body`, `404 {"error":"auth not found"|"quota provider not found for plugin"}`, `502 {"error":"failed to fetch quota: ..."}`. `DELETE plugins/:id/quota` (v0 also `POST .../quota/reset`): auth_index from query or JSON body; success `200 {"status":"ok","auth_index","message"?}` after plugin reset plus core cooldown reset; `502` for plugin rejection/failure (`{"error": msg}`), `404` as above.
- v0 only: `GET /quota/providers` -> `{"providers":[...]}`; `POST /quota/fetch` body `{"auth_index",...,"plugin_id"?,"provider"?}` (falls back to declarative `quota_probe` in credential metadata; `501 {"error":"no quota provider available for credential"}`); `POST /quota/reset` same body (`501` when plugin host unavailable / no provider).

### 5. OAuth session and callback mechanics (oauth_sessions.go, oauth_callback.go, auth_files_provider_oauth.go, auth_files_oauth_callback.go, auth_files_devin_oauth.go, auth_files_v8.go)

#### Start login
- v0: `GET /v0/management/<provider>-auth-url` for anthropic, codex, antigravity, kimi, kimi-ai, xai, devin, meta; v8: `GET /v8/management/oauth/auth-url?provider=` with `claude|codex|antigravity|kimi|kimi-ai|xai|devin|meta|<plugin id>` (lowercased; empty `400 {"error":"provider is required"}`; unknown non-plugin `404 {"error":"provider_not_found"}`). Remaining query params are provider-specific; for plugins, all query values (except `provider` on v8) become metadata (single value -> string, multiple -> []string).
- Success: `200 {"status":"ok","url":"<authorization url>","state":"<state>"}`. Device-code flows (xai, meta, kimi, kimi-ai) add `"flow":"device"` and optionally `"user_code"` and `"expires_in"` (seconds; xai/meta default to MaxPollDuration when unset). Errors `500 {"error":"failed to generate PKCE codes"|"failed to generate state parameter"|"failed to generate authorization url"|"callback server unavailable"|"failed to start callback server"}`; plugin `502 {"error":"invalid oauth state"|"failed to generate authorization url"}`.
- State: random (`misc.GenerateRandomState`). Valid state rule (`ValidateOAuthState`): trimmed non-empty, <= 128 chars, chars only `[A-Za-z0-9_.-]`, no `/`, `\`, or `..`.
- Provider session names: `anthropic` (v8 `claude`; aliases anthropic/claude), `codex` (alias openai), `antigravity` (anti-gravity), `xai` (x-ai, x.ai, grok), `devin` (cognition), `meta` (muse), kimi uses its own provider name. Plugin providers: lowercase `[a-z0-9-]` only.
- Web UI mode: query `is_webui=1|true|yes|on` for anthropic/codex/antigravity starts a temporary TCP forwarder on `0.0.0.0:<port>` (anthropic 54545, codex 1455, antigravity `antigravity.CallbackPort`) that 302-redirects (with `Cache-Control: no-store`) to `http(s)://127.0.0.1:<server port>/<provider>/callback` preserving the query string (scheme https if `tls.enable`); stopped when the flow ends. Devin uses redirect `http://127.0.0.1:<port>/callback` directly.

#### Session store (in-process, global)
Map state -> {Provider, Status, Source("builtin"|"plugin"), Metadata, Completed, CreatedAt, ExpiresAt}. TTL 30 min (device flows need it); completed sessions kept 1 min; expired entries purged lazily on each access. `Register(state, provider)` sets Status "". `SetError(state,msg)` (ignored if completed; empty msg -> "Authentication failed") sets Status to the message and refreshes expiry. `Complete(state)` sets Completed, clears status/metadata, TTL 1 min. `Cancel(state)` deletes only a pending session (not completed, no error status). Pending = exists, not completed, Status == "" (provider match case-insensitive). `guardOAuthSessionPendingForSave` is checked right before persisting credentials so a cancel mid-exchange prevents saving. Plugin registration fails if state exists. When a credential for a provider is saved elsewhere, `CompleteOAuthSessionsByProvider` completes pending builtin sessions of that provider.

#### Callback file handoff
- Callback receivers (the `/<p>/callback` routes and the mgmt callback endpoints) write file `<auth-dir>/.oauth-<canonicalProvider>-<state>.oauth` (JSON `{"code":"","state":"","error":""}`, trimmed values), atomically (temp `.oauth-callback-*` then rename), auth dir created 0700, only if the session is pending for that provider (else `errOAuthSessionNotPending`).
- Background goroutine per login polls every 500 ms (anthropic: deadline 5 min; sets session error "Timeout waiting for OAuth callback"), reads then deletes the file, validates `state` equality (error "State code error"), `error` field (anthropic: "Bad request"; devin: "Devin authorization denied"), then exchanges code and saves credential via `saveTokenRecord`; failures set session errors such as "Failed to exchange authorization code for tokens", "Failed to save authentication tokens", "Missing authorization code". Claude codes may carry `#state` suffix (split on `#`). Devin waits max 5 min and a watcher cancels when session stops being pending (checks every 2s).
- Manual callback endpoint (v0 `/oauth-callback`, v8 `/oauth/callback`, GET or POST, no mgmt key): GET reads query `provider`, `code`, `state`, `error` (or `error_description`). POST JSON `{"provider","redirect_url","code","state","error"}`; `redirect_url` (full callback URL) fills in missing state/code/error from its query. Responses are `{"status":"error","error":msg}` except success `200 {"status":"ok"}` (meaning callback accepted, not finished):
  - 500 `handler not initialized`; 400 `invalid body` (POST parse), `invalid redirect_url`, `state is required`, `invalid state`, `code or error is required`, `unsupported provider`, `provider does not match state`.
  - 404 `unknown or expired state`; 409 `oauth flow is already completed`, 409 `<session error status text>`, 409 `oauth flow is not pending`; 500 `failed to persist oauth callback`.
  - Provider omitted -> taken from the session; explicit provider normalized (plugin sessions use plugin normalization) and must equal session provider.

#### Poll status: `GET /v8/management/oauth/status?state=` (v0 `/get-auth-status`)
All HTTP 200 except bad state. Cases in order:
- no `state` -> `{"status":"ok"}`; invalid state format -> `400 {"status":"error","error":"invalid state"}`.
- unknown/expired -> `{"status":"error","error":"unknown or expired state"}`.
- completed -> `{"status":"ok"}`.
- session error set -> `{"status":"error","error":"<message>"}`.
- plugin session with registered provider: calls `host.PollLogin(provider,state,metadata)`; error -> set session error, `{"status":"error","error":msg}`; response status pending/"" -> `{"status":"wait"}`; error status -> `{"status":"error","error":msg|"Authentication failed"}`; success -> saves returned auth records (fail: error "Failed to save authentication tokens"; none: "Authentication failed"), completes session, `{"status":"ok"}`.
- else `{"status":"wait"}`.
Cancel: `DELETE /v8/management/oauth/session?state=` (v0 `/oauth-session`): `400 {"status":"error","error":"missing state"|"invalid state"}`; `200 {"status":"ok","cancelled":bool}` (cancelled=false if not pending).
Statuses seen by clients: `wait` (pending), `ok` (done), `error` (+ `error` text).

### 6. Logs (logs.go)

Log directory (`logDirectory`): handler `logDir` set at startup from `logging.ResolveLogDirectory(cfg)`: `$WRITABLE_PATH/logs` if set; else `logs` (cwd-relative) if writable; else `<resolved auth-dir>/logs`. Relative paths are made absolute on set.
- Main file `main.log`. Rotated: `main.log.<N>` (numeric suffix, order N; larger N older) and `main-<YYYY-MM-DDTHH-MM-SS>[.<n>].log[.gz]` (local-time timestamp; ordered newest first by `MaxInt64 - unix`). `collectLogFiles` returns files oldest -> newest ending with `main.log` (rotated sorted by order, reversed so highest order/oldest first, `main.log` order 0 last).
- `GET observability/logs` (v0 `/logs`). Preconditions: `500 handler unavailable`; `503 {"error":"configuration unavailable"}`; `400 {"error":"logging to file disabled"}` when `logging-to-file` false; `500 log directory not configured`; dir missing -> `200` empty result (`lines:[]`, `latest-timestamp` = `after` or cursor's, `cursor-reset:true` only if a cursor was supplied); `500 {"error":"failed to list log files: ..."}`.
  Query: `limit` (positive int; invalid `400 {"error":"invalid limit: must be a positive integer|must be greater than zero"}`; absent = unlimited), `after` (unix seconds; non-positive/invalid = 0), `cursor` (opaque).
  Modes:
  1. `cursor` present: decode base64url (raw or padded) JSON `{"v":1,"file","offset","size","modTime","modTimeUnixNano"?,"latestTimestamp","fingerprint"}`; invalid cursor, unknown/unsafe file name (must be `main.log` or rotated name), file not found/rotated away, or fingerprint mismatch -> reset: fall back to tail mode with `cursor-reset:true`. Otherwise read complete lines (only up to last newline) starting at cursor offset through subsequent files up to `limit`; if no new lines, returns empty `lines` and the same cursor. `line-count` = number of returned lines.
  2. no cursor, `after==0`, `limit>0`: tail: last `limit` complete lines across files (newest files first, prepended), `next-cursor` at end of latest file.
  3. otherwise legacy: scan all files line by line (scanner buffer up to 8 MiB); line timestamp = first 19 chars (optional leading `[`) parsed `2006-01-02 15:04:05` in local time; lines with ts > `after` included, continuation lines (no parseable ts) follow the previous line's include decision; trailing `\r` trimmed; keep last `limit`; `line-count` = total scanned line count; `latest-timestamp = max(ts)` (or `after` if lower/zero); `next-cursor` points at end of latest complete boundary.
  Response: `200 {"lines":[str],"line-count":int,"latest-timestamp":int(unix),"next-cursor":"<str, maybe empty>"[,"cursor-reset":true]}`.
- `DELETE observability/logs`: same preconditions plus `404 {"error":"log directory not found"}`. Truncates `main.log` to 0, deletes every rotated file (non-dir entries matching rotation naming; NOT `error-*.log`, NOT request logs). `200 {"success":true,"message":"Logs cleared successfully","removed":<count of rotated deleted>}`; `500` on fs errors.
- `GET observability/logs/errors` (v0 `/request-error-logs`): if `request-log` is true -> `200 {"files":[]}`. Else list files named `error-*.log` in log dir (non-recursive): `200 {"files":[{"name","size","modified":unix_sec}]}` sorted by modified desc; missing dir -> empty list; `503 configuration unavailable`; `500 failed to list request error logs: ...`.
- `GET observability/logs/errors/:name`: name must not contain `/` or `\` (`400 {"error":"invalid log file name"}`), must start `error-` and end `.log` (else `404 {"error":"log file not found"}`); path-escape check `400 invalid log file path`; missing `404 log file not found`; dir `400 invalid log file`; success file attachment (`Content-Disposition: attachment; filename=<name>`, gin `FileAttachment`).
- `GET observability/logs/requests/:id` (v0 `/request-log-by-id/:id`, also `?id=`): `400 {"error":"missing request ID"}`; `400 {"error":"invalid request ID"}` if contains `/` or `\`; dir missing `404 log directory not found`. Matches log files whose name ends with `-<last 8 chars of id (or whole id if <= 8)>.log` (`ShortRequestID`); chooses newest by mod time (tie/odd names via `logFileIsNewer`: parsed timestamps then lexical). None -> `404 {"error":"log file not found for the given request ID"}`; success file attachment under matched file name. Not gated on `logging-to-file`.
- Request log files themselves (request-log true) and error logs are written elsewhere in the logging package into this same directory.

### 7. Misc notes for porters
- Handler holds a single mutex `h.mu` for config mutation; config reload after management save is asynchronous (cloned snapshot, generation counter, sequential via `reloadMu`; hook replaces/applies config in server).
- `X-CPA-*` headers are present on all authenticated management responses and on auth failures (set before check).
- Plugin-owned extra management routes are registered dynamically with `pluginHost.RegisterManagementRoutes` (v0 prefix only) and served via NoRoute fallback behind the same availability + auth gates.
- No pagination or envelopes in v8 config; v0 setters use `{"value": x}` envelope.

---

## 11. CLI (cmd/server/main.go, internal/cmd)

Binary prints `CLIProxyAPI Version: <v>, Commit: <c>, BuiltAt: <d>` to stdout on start (suppressed for `-discover-json` and `discover -json`). Version/commit/date are build-time vars (`dev`/`none`/`unknown` defaults). Go `flag` package: `-flag` and `--flag` both accepted, `-flag=value` or `-flag value`; booleans `-flag` / `-flag=false`. `-password` is hidden from usage output.

### 11.1 Flags
| Flag | Type, default | Meaning |
|---|---|---|
| `-config <path>` | string, `DefaultConfigPath` (build-time, normally empty -> `<cwd>/config.yaml`) | config file |
| `-codex-login` | bool | Codex OAuth login (browser, loopback callback) |
| `-codex-device-login` | bool | Codex device-code login (metadata `codex_login_mode=device`) |
| `-claude-login` | bool | Claude OAuth login |
| `-antigravity-login` | bool | Antigravity OAuth login (default callback port 51121) |
| `-kimi-login` | bool | Kimi (.com) OAuth (device flow), provider `kimi` |
| `-kimi-ai-login` | bool | Kimi.ai OAuth, provider `kimi-ai` |
| `-xai-login` | bool | xAI OAuth |
| `-devin-login` | bool | Devin OAuth |
| `-meta-login` | bool | Meta OAuth |
| `-no-browser` | bool | do not auto-open the browser (print URL) |
| `-oauth-callback-port <n>` | int, 0 | override loopback callback port (not used by kimi) |
| `-vertex-import <file>` | string | import Vertex service-account JSON into auth-dir |
| `-vertex-import-prefix <p>` | string | model namespace prefix for the import (single segment, no `/`) |
| `-password <pw>` | string | local management password (also keep-alive auth, TUI) |
| `-tui` | bool | terminal management UI |
| `-standalone` | bool | with `-tui`: start embedded server |
| `-management-base-url <url>` | string | remote management URL for TUI client mode (else config `remote-management.base-url`, else `http://127.0.0.1:<port|8317>`) |
| `-local-model` | bool | use embedded `models.json` / `codex_client_models.json`, skip remote catalog refresh |
| `-home-jwt <jwt>` | string (env `HOME_JWT`/`home_jwt`) | fetch config from a Home control plane (mTLS bootstrap); forces home mode, disables stores |
| `-home-disable-cluster-discovery` | bool | keep the configured home address |
| `-discover`, `-discover-json`, `-discover-timeout <s=3>`, `-discover-service-type <t>` (default `_ai-gateway._tcp`), `-discover-include <csv>`, `-discover-exclude <csv>` | LAN gateway discovery (mDNS/DNS-SD), then exit; also subcommand `discover` with `-timeout -json -service-type -config -include -exclude` |
Plugins may register extra flags (`pluginHost.RegisterCommandLineFlags`) from a bootstrap read of the config before `flag.Parse`; `-config` is pre-scanned from argv for that.
Login flags are mutually exclusive in effect: first matching branch in order vertex-import, antigravity, codex, codex-device, claude, kimi, kimi-ai, xai, devin, meta. Each runs one login via `sdkAuth.Manager.Login(provider, cfg, LoginOptions{NoBrowser, CallbackPort, Metadata, Prompt})`, prints `Authentication saved to <path>`, optional `Authenticated as <label>`, then `<Provider> authentication successful!` (claude/codex also have friendly error messages; port-in-use for claude/codex exits with code 13). Failures print/log and return exit code 0 (except port-in-use 13). Interactive prompts (e.g. project id) read stdin lines (`defaultProjectPrompt`).
Vertex import (`DoVertexImport`): reads JSON, normalizes service account, requires `project_id`, location fixed `us-central1`, writes `vertex-[<prefix>-]<project_id>.json` (chars `/`,`\`,`:` -> `_`, space -> `-`) via the token store with fields `service_account, project_id, email, location, type:"vertex", prefix, label`; prints `Vertex credentials imported: <path>`.

### 11.2 Startup sequence (server mode)
1. `.env` in CWD loaded (godotenv, missing ok). Storage selection by env: `PGSTORE_DSN` (postgres store; `PGSTORE_SCHEMA`, `PGSTORE_LOCAL_PATH`), else `GITSTORE_GIT_URL` (+`GITSTORE_GIT_USERNAME/TOKEN/LOCAL_PATH/GIT_BRANCH`), else `OBJECTSTORE_ENDPOINT` (+`ACCESS_KEY`, `SECRET_KEY`, `BUCKET`, `LOCAL_PATH`; http/https scheme optional); store bootstraps config from `config.example.yaml` when absent. `DEPLOY=cloud` allows a missing/empty config: `WaitForCloudDeploy` logs a standby message and blocks until SIGINT/SIGTERM without starting the API server.
2. Config load (`LoadConfigOptional`), `redisqueue` toggles, cooling flags, logging setup (`ConfigureLogOutput`), `util.SetLogLevel`, resolve `auth-dir` (`~` expansion).
3. Example-API-key safe mode computed (1.5).
4. Token store registered (file store default), `configaccess.Register` (API keys), plugin host config applied.
5. Server run: `managementasset.StartAutoUpdater`, antigravity version updater, model catalog updaters (models.json, codex client models, devin models; skipped by `-local-model`; models.json skipped in home mode), then `cmd.StartServiceWithPluginHost(cfg, configPath, password, ...)` which builds the service and runs until SIGINT/SIGTERM. With a local `-password`, keep-alive endpoint (10s idle) is enabled so a supervising TUI controls lifetime.
6. TUI: `-tui -standalone` starts the embedded server (local password `tui-<pid>-<nanos>` unless `-password`), polls management `GetConfig` up to 30 times with 100ms*1.5 backoff, redirects stdout/stderr to /dev/null and logs to an in-memory ring (2000 entries). `-tui` alone connects to a remote management URL.
