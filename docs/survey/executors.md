# Part A. CLIProxyAPI executors, auth, and upstream protocols: Rust port survey (common)

Source: `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI` (Go module `github.com/router-for-me/CLIProxyAPI/v8`). All paths below are relative to that root unless absolute. Line counts are non-test `.go` lines (`wc -l`, tests excluded).

Document layout: Part A (this part: ranking, executor contract, cross-cutting runtime), Part B Claude, Part C Codex (HTTP, upstream websocket, client `/v1/responses` websocket), Part D Google family (Gemini key, Vertex, AI Studio + wsrelay, Antigravity), Part E OpenAI-compat/xAI/Kimi/Devin/Meta. Section numbers are local to each part.

---

## 1. Ranking by importance and complexity

Importance = expected traffic and how many clients depend on it. Complexity = protocol surface plus fidelity requirements. Line counts split into executor / helps / auth (internal/auth) / sdk-auth (login flows).

| Rank | Provider (type string) | Executor LoC | helps LoC | auth LoC | sdk/auth LoC | Notes |
|---|---|---|---|---|---|---|
| 1 | Claude (`claude`) | 7997 | 3581 | 2321 | 232 | Highest fidelity burden: cloaking, cch signing, TLS (uTLS) fingerprint, device profile, tool alias remap, thinking replay |
| 2 | Codex (`codex`) HTTP+WS | 4401 http + 3754 ws | 1961 | 1421 | 502 | Responses API upstream, upstream websocket executor with session reuse, apply_patch bridge, image gen, plus client-facing `/v1/responses` websocket (3777 LoC in sdk/api/handlers/openai) |
| 3 | Antigravity (`antigravity`) | 6436 | 487 | 454 | 378 | Gemini-CLI-style envelope to Cloud Code, multi-host fallback, credits, compaction, reasoning replay (2661 LoC alone) |
| 4 | xAI (`xai`) HTTP+WS | 6308 | (shared) | 676 | 132 | Responses-style upstream with websocket executor, reasoning replay |
| 5 | Devin (`devin`) | 2559 + wire 1262 + models 325 | ~1600 | 924 | 304 | Connect-RPC/protobuf style wire format, hand-rolled in helps/devin_wire.go |
| 6 | Gemini API key (`gemini` executor) | 1093 | small | none (no OAuth login in tree) | n/a | generativelanguage.googleapis.com, key auth |
| 7 | Gemini Vertex (`vertex`) | 1225 | 86 | 292 | n/a | Service account JWT -> OAuth token, aiplatform endpoints |
| 8 | Kimi (`kimi`, `kimi-ai`, `kimi.ai`) | 1877 (+kimi_thinking_replay 484) | 188 | 822 | 166 | OpenAI-chat upstream, device-flow OAuth |
| 9 | OpenAI-compat (config-defined) | 1126 | 340 | none | none | Generic; highest practical traffic for API-key users but simplest logic |
| 10 | Meta (`meta`) | 961 | 46 | 564 | 159 | Smallest bespoke provider |
| 11 | AI Studio (`aistudio`) | 642 | none | none | none | Needs wsrelay (browser websocket relay), 874 LoC internal/wsrelay |

Practical port order: openai_compat (validates the Executor trait, SSE, usage), gemini key, claude, codex HTTP, codex WS + client WS, antigravity, vertex, xai, kimi, meta, devin, aistudio/wsrelay.

Shared infrastructure LoC: `internal/runtime/executor/helps/` 15417 total (usage_helpers 1591, devin_wire 1262, payload_helpers 1043, logging_helpers 785, claude_* 3581, codex_* 1961); executor dir total 38407; `sdk/cliproxy/executor` 499; `sdk/auth` 2644 (filestore 547); `internal/wsrelay` 874; `internal/misc` 1331; `internal/util` 4042 (gemini_schema.go 1711 is a schema sanitizer used by Gemini-family providers).

---

## 2. Executor contract and cross-cutting runtime

### 2.1 `ProviderExecutor` interface (sdk/cliproxy/auth/conductor.go:16)

```
Identifier() string                                  // provider key, e.g. "claude"
Execute(ctx, *Auth, executor.Request, executor.Options) (executor.Response, error)
ExecuteStream(ctx, *Auth, Request, Options) (*executor.StreamResult, error)
Refresh(ctx, *Auth) (*Auth, error)                    // returns updated auth (new tokens in Metadata)
CountTokens(ctx, *Auth, Request, Options) (Response, error)
HttpRequest(ctx, *Auth, *http.Request) (*http.Response, error)  // inject creds and send (management API passthrough)
```
Optional interfaces: `APIKeyConfigExecutor.ForAPIKey() ProviderExecutor` (view without OAuth-only config), `RequestAuthPreparer{ShouldPrepareRequestAuth, PrepareRequestAuth}` (fill missing metadata just before request; manager serializes and persists), `ExecutionSessionCloser.CloseExecutionSession(sessionID)` (release per-session upstream websocket state), `RefreshEvaluator.ShouldRefresh(now, auth)` on `auth.Runtime`, `RefreshLead() *time.Duration` on `auth.Runtime`.

### 2.2 Types (sdk/cliproxy/executor/types.go)

- `Request{Model string (upstream model after alias), Payload []byte (already translated to provider format), Format sdktranslator.Format, Metadata map[string]any}`.
- `Options{Stream bool, Alt string, Headers http.Header (client headers forwarded to builder), Query url.Values, OriginalRequest []byte (inbound bytes pre-translation), SourceFormat, ResponseFormat (empty => SourceFormat; helper ResponseFormatOrSource), Metadata, RequestAfterAuthInterceptor, WebSocketResponseObserver, ExecutionLifecycle, ProxyURL}`.
- `Response{Payload, Metadata, Headers}`; `StreamChunk{Payload []byte, Err error}`; `StreamResult{Headers http.Header, Chunks <-chan StreamChunk}`. Rust equivalent: `Stream<Item=Result<Bytes, ExecError>>` plus headers returned before first chunk. Each chunk is ONE logical payload unit (one SSE data line or JSON frame as the provider format defines), not arbitrary byte slices; executors translate chunks with `sdktranslator` themselves before emitting (executor yields already-client-format chunks; non-stream `Execute` returns client-format body).
- `StatusError{StatusCode() int}`: every upstream HTTP failure must surface as an error carrying the status (executors define `statusErr{code, msg}`; body text is the message). Optional extras seen on errors: `RetryAfter() *time.Duration`, `Headers()`, `ResponseBody()`, `DirectResponse()` (pass upstream body to client verbatim).
- `RequestScopedError{IsRequestScoped() bool}`: failure belongs to the request, not the credential; manager must not rotate credential or apply cooldown.
- `RequestTerminatedError{HTTPStatus, Header, Body}`: plugin interceptor terminated the request (`RequestAfterAuthInterceptor` returns `Terminate=true`).
- `UpstreamWebsocketReplayRequiredError` (websocket.go): status 426, body `{"error":{"message":"upstream transport requires full HTTP replay","type":"server_error","code":"upstream_http_replay_required","status":426}}`, request-scoped. Signals the client-facing websocket layer to replay the entire conversation over HTTP because the upstream ws cannot continue incrementally.
- Context helpers (context.go): `WithDownstreamWebsocket` (request originates from client ws), `WithRequiredUpstreamWebsocket` (incremental input valid only on current upstream ws: never fall back to HTTP), `WithUpstreamAttemptTracker/MarkUpstreamAttempt/UpstreamAttempted` (did this attempt reach the transport boundary; used to decide whether failure may be retried on another credential). request_proxy.go: `WithRequestProxyURL`, `WithoutRequestProxyURL` (refresh strips override), `RequestProxyURL`. websocket_input.go: `WebsocketInput{Payload, Err}` channel in ctx, `WebsocketAuthEnabled(ctx, authID)` live check. lifecycle.go: `ExecutionLifecycle{Bind(func() error) error; End(string)}`, `BindExecutionResource(opts, io.Closer)` closes once.
- Metadata keys (string constants; names are the JSON keys): `requested_model`, `request_path`, `disallow_free_auth`, `auth_selection_model`, `reasoning_effort`, `service_tier`, `generate`, `pinned_auth_id`, `selected_auth_id`, `selected_auth_index`, `selected_auth_callback`, `selected_auth_index_callback`, `execution_session_id`, `derived_session_id`, `lcp_affinity_session_id`, `canonical_session_id`, `parent_session_id`, `is_fork`, `is_compaction`, `node_kind`, `lcp_tail_fingerprints`, `lcp_environment_digest`, `lcp_access_generation`, `lcp_fingerprints`, `lcp_min_prefix_length`, `caller_scope`, `session_affinity_provider`, `session_affinity_model`.

### 2.3 Auth record model (sdk/cliproxy/auth/types.go)

`Auth{ID, Provider, Prefix, FileName, Storage, Label, Status, StatusMessage, Disabled, Unavailable, ProxyURL, Attributes map[string]string (immutable config), Metadata map[string]any (mutable token state, = file JSON), Quota, LastError, CreatedAt, UpdatedAt, LastRefreshedAt, NextRefreshAfter, NextRetryAfter, ModelStates, Runtime any}`.
- `Attributes` keys used by executors: `api_key`, `base_url`, `auth_kind` (`apikey`|`oauth`), `path`, `source`, `source_backend` (`config|file|git|memory|objectstore|postgres`), `runtime_only`, `weight`, `config_index`, `codex_alpha_search`, `codex_disable_cloaking`, `header:<Name>` (custom headers; see 2.6), `email`.
- `AuthKind()`: explicit attribute/metadata `auth_kind` first; else attribute `api_key` set => apikey; else metadata has any of `access_token, refresh_token, id_token, email, token_type, expires_at, expired` or non-empty `token` object => oauth.
- Expiry (`ExpirationTime()`): JWT `exp` of access token first (base64url, padding tolerant; exp number or numeric string; values normalised s/ms), then metadata keys in order `expired, expire, expires_at, expiresAt, expiry, expires` (time parse layouts: RFC3339, RFC3339Nano, `2006-01-02 15:04:05`, `2006-01-02 15:04`, unix seconds/millis string or number), then `expires_in`+`timestamp|issued_at` relative, then nested `token`/`Token` object recursion.

### 2.4 Failure handling in the conductor (sdk/cliproxy/auth/conductor_cooldown.go: `applyAuthFailureState`, `MarkResult`)

This is where executor status codes become cooldown/quota state. Rust must reproduce the numbers:
- Constants: `quotaBackoffBase=1s`, `quotaBackoffMax=30m`, `minQuotaCooldownFloor=10s`, `transientErrorCooldown=1m` (overridable via config transient seconds; negative disables), `refreshCheckInterval=5s`, `refreshMaxConcurrency=16`, `refreshPendingBackoff=1m`, `refreshFailureBackoff=5m`, `invalidGrantBackoff 1m..30m`, `refreshIneffectiveBackoff=30s`, auto-refresh timer cap 30s.
- Per status (applies to auth-level, and the same table per-model in `ModelStates`):
  - 401: `unauthorized`, NextRetryAfter = now+30m (executor may first try refresh-after-401, `tryRefreshAfterUnauthorized`).
  - 402 and 403: `payment_required`, +30m.
  - 404: `not_found`, +12h unless retry hint.
  - 429: `quota exhausted`, `Quota.Exceeded=true`; cooldown = retryAfter hint (floored to 10s) else exponential `1s * 2^level` capped 30m (level advances at most once per open window); never shortens an existing `Quota.NextRecoverAt`.
  - 408, 500, 502, 503, 504, 520-526: `transient upstream error`, cooldown = retryAfter hint else 1m.
  - other: `request failed`, 1m.
  - Cloudflare challenge body detected (status < 500): cooldown 10s min with quota backoff ladder. `invalid_grant` refresh error: 30m.
  - `disable_cooling` (auth metadata/attribute `disable_cooling`, config) zeroes all cooldowns.
  - Request-scoped errors (`RequestScopedError`) and `shouldSkipCredentialCooldown` skip all state changes.
  - A later failure never shortens a live cooldown.
- Retry-hint extraction: executors attach `RetryAfter()` parsed from upstream (Google: `helps.ParseRetryDelay` reads `error.details[].retryDelay` like `"3.5s"`, and `RetryInfo`; Codex: `resets_at`/`resets_in_seconds`; see per-provider).
- Refresh scheduling (conductor_refresh.go `shouldRefresh`): skip if 401-failed/invalid-grant-disabled or `NextRefreshAfter` in future; else `RefreshEvaluator` on `Runtime`; else if metadata/attributes `refresh_interval_seconds|refreshIntervalSeconds|refresh_interval|refreshInterval` set: refresh when expiry within interval or age >= interval; else provider lead: refresh when `expiry - now <= lead` (or `now - lastRefresh >= lead` if no expiry). Provider leads (from `RefreshLead()` in sdk/auth): codex 24h, claude 4h, antigravity 30m, xai (`xaiauth.RefreshLead()`, see xAI section), kimi 5m, devin nil, meta nil (no auto refresh). Unlisted providers (gemini, vertex, openai-compat, aistudio) never auto-refresh via lead.
- "Home" mode (`helps.RefreshAuthViaHome`, helps/home_refresh.go): when a remote control-plane ("home") is enabled, refresh is delegated to it instead of local OAuth. Port may stub as disabled initially.

### 2.5 Credential file storage (sdk/auth/filestore.go, FileTokenStore)

- Directory: config `auth-dir` (`util.ResolveAuthDir`, expands `~`). Files are `*.json`, walked recursively (`WalkDir`), id = path relative to auth-dir (lowercased on Windows). Unreadable/invalid JSON files are silently skipped. Empty files skipped.
- Read: unmarshal to `map[string]any`; `NormalizeCredentialMetadata` rewrites legacy hyphen keys to snake_case, canonical wins: `api-key->api_key`, `base-url->base_url`, `disable-cooling->disable_cooling`, `excluded-models->excluded_models`, `fingerprint-profile->fingerprint_profile`, `model-aliases->model_aliases`, `proxy-url->proxy_url`, `request-retry->request_retry`, `request-scoped-errors->request_scoped_errors`, `tool-prefix-disabled->tool_prefix_disabled`. Weight validated (`weight` field), priority applied (`priority`).
- Discriminator: top-level `"type"` string is the provider key (`claude`, `codex`, `antigravity`, `kimi`, `xai`, `devin`, `meta`, `vertex`, `gemini`, ...). `type == "gemini"` (case-insensitive) files are ignored (legacy gemini-cli creds are no longer loaded). Missing type => provider `unknown`. A plugin parser (`PluginAuthParser`) may claim a file first (can expand one file into several virtual auths with attribute `plugin_virtual=true`).
- Common optional fields in every file: `disabled` (bool, written on save), `proxy_url`, `prefix` (trimmed of `/`; must contain no inner `/`), `label`/`email` (label derivation `labelFor`), `headers` (object of custom headers -> attributes `header:<Name>`), `priority`, `weight`, `excluded_models`, `model_aliases`, `disable_cooling`, `request_retry`, `refresh_interval_seconds`, `note`.
- Write: parent dir `0700`, files `0600`. If `auth.Storage` (provider token struct) exists: set `metadata["disabled"]`, call `SetMetadata(metadata)` (extra fields merged into the struct's JSON through `misc.MergeMetadata`), then `SaveTokenToFile(path)` (provider struct JSON). Else metadata-only: marshal map, skip write if JSON-equal to existing file, otherwise truncate-in-place write (not atomic rename). A disabled auth whose file was deleted is not recreated unless the save carries `WithAuthCreationIntent`.
- `Manager.Login` (sdk/auth/manager.go): run `Authenticator.Login`, merge existing file metadata (`MergeExistingAuthMetadata`, keeps operator fields like proxy_url/priority/prefix across re-login), Claude legacy-credential migration (`claudeauth.FindMatchingLegacyCredential`: if an older-named claude file for the same account exists, merge its metadata, save canonical filename, then delete legacy), save via store, return saved path.
- `Authenticator` interface: `Provider() string; Login(ctx, cfg, *LoginOptions) (*Auth, error); RefreshLead() *time.Duration`. `LoginOptions{NoBrowser, ProjectID, CallbackPort, Metadata map[string]string, Prompt func(string)(string,error)}`. Registered authenticators: codex, claude, antigravity, kimi, kimi-ai, kimi.ai, xai, devin, meta (sdk/auth/refresh_registry.go).
- OAuth callback utilities (internal/misc/oauth.go): `GenerateRandomState` = 16 random bytes hex (32 chars). `ParseOAuthCallback(input)`: accepts full URL, `?query`, bare `k=v`, `host:port/..`; reads `code,state,error,error_description` from query then fragment; supports `code#state` form (Claude paste flow); if error empty but description present, description becomes error; errors with "callback URL missing code" if neither code nor error. Used for the manual paste fallback when the browser callback server is unreachable (`NoBrowser`).

### 2.6 Shared HTTP plumbing

- Client construction (helps/proxy_helpers.go `NewProxyAwareHTTPClient`): proxy priority = execution `Options.ProxyURL`/ctx override > `auth.ProxyURL` > `cfg.ProxyURL` > ctx RoundTripper (`ctx.Value("cliproxy.roundtripper")`). Supported schemes: `socks5, socks5h, http, https` (sdk/proxyutil; invalid scheme errors, falls through to default). Timeout 0 = none (streaming). Devin uses `NewDevinHTTPClient`: same priority but `DisableCompression=true`, `Accept-Encoding: identity` default. Transports cached per proxy URL in an LRU `TransportCache` (capacity 64, closes evicted idle conns). Claude uses a utls client (`helps/utls_client.go`, see Claude section).
- Custom headers (internal/util/header_helpers.go `ApplyCustomHeadersFromAttrs`): for each attribute `header:<Name>` apply `Set(Name, value)` AFTER built-in headers (user override wins). Value forms: literal; `$HeaderName` copies the client's inbound header (omitted if absent); `$CPA-SESSION-ID` (or embedded, case-insensitive) expands to the internal session id (omitted if none). `Host` also sets `req.Host`.
- `misc.ScrubProxyAndFingerprintHeaders(req)`: deletes `X-Forwarded-For/Host/Proto/Port`, `X-Real-IP`, `Forwarded`, `Via`, `X-Title`, `X-Stainless-{Lang,Package-Version,Os,Arch,Runtime,Runtime-Version}`, `Http-Referer`, `Referer`, `Sec-Ch-Ua{,-Mobile,-Platform}`, `Sec-Fetch-{Mode,Site,Dest}`, `Priority`, `Accept-Encoding`. Used by Antigravity (and others where noted). `misc.EnsureHeader(target, source, key, default)`: source header (trimmed non-empty) wins, then existing target, then default.
- Logging (helps/logging_helpers.go, 785): per-request upstream request/response capture for debug logging, masks `Authorization`, api keys (`util.HideAPIKey`, `MaskSensitiveHeaderValue`, `MaskSensitiveQuery`). Not behaviourally relevant beyond redaction.
- Sensitive: never log tokens; Claude/Codex tokens appear in `Authorization` / `x-api-key`.

### 2.7 Payload pipeline shared by all executors

Standard `Execute` flow (all providers follow this shape; deviations noted per provider):
1. `reporter := helps.NewExecutorUsageReporter(ctx, exec, baseModel, auth)`; `defer reporter.TrackFailure(&err)`.
2. Resolve base model: `thinking.ParseSuffix(req.Model)` splits a `model(budget|level)` suffix; `baseModel` goes to upstream, suffix drives thinking.
3. `from := opts.SourceFormat; to := <provider format>`; `originalPayload := opts.OriginalRequest or req.Payload`; `body := sdktranslator.TranslateRequest(from, to, baseModel, payload, stream)` (translators out of scope of this survey; they live in `internal/translator`).
4. `helps.ApplyRequestThinking` / `ApplyThinkingWithSourcePayload` (internal/thinking) rewrites thinking config for the target (`providerKey`: claude, codex, gemini, gemini-cli/antigravity, openai, kimi, xai, interactions). Thinking providers registered in helps/thinking_providers.go.
5. Payload config rules `helps.ApplyPayloadConfigWithRequestForExecutor` (config `payload:` section, order matters):
   1. Codex-UA tool integer normalisation when client is Codex and target is not Codex.
   2. Strip `image_generation` tool/tool_choice when `disable-image-generation` applies to the request path.
   3. `default` rules: set path only if absent in ORIGINAL request (source) and not already set by an earlier default (first wins); `default-raw` same with raw JSON.
   4. `override` rules: always set, last wins; `override-raw`.
   5. `filter` rules: delete paths (reverse index order).
   Rules match when model glob (`matchModelPattern`, `*` wildcard) matches either upstream or requested model (`payloadModelCandidates`), and optional `protocol`, `from-protocol`, header match, `match`/`not-match` JSON conditions, `exist`/`not-exist` paths hold. Paths use gjson syntax with a `root` prefix (e.g. `request` for Gemini CLI envelope), support `#(...)` queries and `*`/wildcard array expansion (`resolvePayloadRulePaths`).
6. Provider-specific mutations, headers, send.
7. Response: non-stream: read body, `reporter.Publish(parseUsage)`, `sdktranslator.TranslateNonStream(...)`. Stream: `bufio.Scanner`-like line reader (scanner buffer max 50 MiB = 52_428_800 for Claude/Codex; `streamScannerBuffer` const for Gemini family), forward lines through `TranslateStream` with a persistent `param any` state, gather usage from final events, `reporter.Publish` on completion, `EnsurePublished` in defer.
8. Response-model observation (`helps/response_model.go`): extracts the upstream-reported model (max 128 chars) from stream/non-stream payload to detect silent model substitution (warn rate-limited 10 min per key, 1024 entries); optional rewrite of response `model` to the requested alias (`sdk/cliproxy/auth/response_model_rewriter.go`).

### 2.8 Usage extraction (helps/usage_helpers.go, 1591)

`usage.Detail{InputTokens, OutputTokens, ReasoningTokens, CachedTokens, CacheReadTokens, CacheCreationTokens, TotalTokens, TokenBreakdown}`. Parsers (all gjson over raw JSON; SSE lines first stripped of `data:` prefix via `jsonPayload`):
- OpenAI-style (`ParseOpenAIUsage`, chat and responses): `input = prompt_tokens | input_tokens`; `output = completion_tokens | output_tokens`; `total_tokens`; cached = `prompt_tokens_details.cached_tokens | input_tokens_details.cached_tokens` (sets Cached and CacheRead); cache creation from `{input,prompt}_tokens_details.{cache_creation_tokens,cache_write_tokens}`; reasoning `completion_tokens_details.reasoning_tokens | output_tokens_details.reasoning_tokens`. Cached/reasoning are SUBSETS of input/output (subset breakdown). If total==0 use breakdown total. Stream: line with `usage` (chat final chunk) or `response.usage` (responses `response.completed`).
- Claude (`ParseClaudeUsage`/`ParseClaudeStreamLine`): from `usage` or `message.usage` (message_start) and `usage` (message_delta); `input_tokens`, `output_tokens` (includes thinking), `cache_read_input_tokens`, `cache_creation_input_tokens`; reasoning from `output_tokens_details.thinking_tokens | .reasoning_tokens | thinking_tokens`. CachedTokens = cache read (fallback cache creation if zero). Total = input + output + cache_read + cache_creation (cache fields independent of input).
- Gemini family (`ParseGeminiUsage`, usageMetadata): `promptTokenCount` (+ `toolUsePromptTokenCount`), `candidatesTokenCount`, `thoughtsTokenCount`, `totalTokenCount`, `cachedContentTokenCount`. Gemini CLI/antigravity envelope: `response.usageMetadata`. Stream usage taken from last chunk containing usageMetadata.
- Codex (`ParseCodexUsage`): `response.usage` in `response.completed`; image tool usage `ParseCodexImageToolUsage`.
- Interactions API (Gemini interactions): `ParseInteractionsUsage`.
- Reporter also records TTFT (first response byte/packet and first token), response model, service tier, session hierarchy, access-token fingerprint, auth index; publishes one record per request plus optional additional-model records (`PublishAdditionalModel`). Stream partial usage merging: `MergeStreamUsageDetail`. `reporter.PublishFailure` on errors with status.
- Token counting endpoint (`CountTokens`): Claude calls upstream `/v1/messages/count_tokens`; OpenAI-style providers approximate locally with tiktoken-family tokenizer (`helps.CountOpenAIChatTokens`, `TokenizerForModel`) and return `BuildOpenAIUsageJSON`; Gemini calls `:countTokens`.

### 2.9 Session identity helpers (helps/*session*, derived_session, user_id_cache)

- `helps.CachedSessionID(apiKey)` stable UUID per API key (TTL 1h refreshed on access, cleanup 15m; optionally backed by a KV store in Home mode); `CachedUserID(apiKey)` stable fake Claude `user_id` (same TTL). `DerivedSessionUUID(provider, metadata...)` deterministic UUID from `derived_session_id`/`execution_session_id` metadata (provider-scoped). `DerivedAntigravitySessionID` yields negative decimal string. `helps.GetCodexCache/SetCodexCache` store Codex `prompt_cache_key` by model+user id for 1h (cleanup 15m).
# Part B. Claude provider

Root: `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI`. Paths below are relative to it. Executor dir = `internal/runtime/executor` (abbrev `EX`), helpers = `EX/helps`, auth = `internal/auth/claude` (abbrev `AU`).

## 0. Scope and non-test line counts

- `EX/claude_executor_request.go` 2836, `claude_executor_cloaking.go` 2226, `claude_executor_stream.go` 608, `claude_signing.go` 555, `claude_executor_execute.go` 448, `claude_executor_tokens.go` 298, `claude_executor.go` 258, `claude_executor_auth.go` 182, `claude_executor_fast_error.go` 180, `claude_fingerprint_policy.go` 141, `claude_thinking_replay.go` 140, `claude_executor_diagnostics.go` 125. Executor subtotal 8007.
- `EX/helps/claude_*`: device_profile 639, diagnostics 632, client_detection 555, credential_identity 484, input_tokens 390, ratelimit 308, code_session 128, ttft_helpers 104, cli_identity_seed 102, mcp_alias 144, builtin_tools 66, upstream 17, mcp_alias_wordlist 12 (+ `claude_bip39_words.txt`, 2048 words, embedded). Subtotal 3581.
- Also in scope by dependency: `EX/helps/utls_client.go` (Claude TLS profile), `EX/helps/cloak_utils.go`, `cloak_obfuscate.go`, `user_id_cache.go`, `session_id_cache.go`, `usage_helpers.go` (ParseClaudeUsage).
- `AU/*.go`: anthropic_auth 688, oauth_server 320, identity 286, utls_transport 254, html_templates 218, errors 167, filename 116, token 104, oauth_response 72, pkce 56, anthropic 40. Subtotal 2321. `sdk/auth/claude.go` 232.
- Total non-test in listed scope: about 14,100 lines. Most of it is fingerprint mimicry of Claude Code 2.1.280 (very high churn; version constants change often).
- Port priority: (1) credential file + OAuth + refresh, (2) plain Messages passthrough with correct headers, (3) error classification and rate-limit parsing, (4) cloaking + CCH signing + beta assembly, (5) TLS fingerprint and header order, (6) MCP aliasing, diagnostics/continuity. Items 4-6 are only needed for OAuth credentials and `fingerprint-profile: claude-code-cli` keys.

## 1. Upstream URLs

- Messages: `{base_url}/v1/messages?beta=true`, base default `https://api.anthropic.com` (`EX/claude_executor_execute.go:30-34`). Method POST. `?beta=true` always appended.
- Count tokens: `{base_url}/v1/messages/count_tokens?beta=true` (`claude_executor_tokens.go`). Only used when apiKey non-empty AND base is first-party (`api.anthropic.com`, https, port empty or 443, no userinfo; `helps/claude_upstream.go`). Otherwise local estimate via tiktoken (`helps/claude_input_tokens.go`, `CountClaudeInputTokens`) returning `{"input_tokens":N}`.
- "First party" test is `helps.IsAnthropicUpstreamURL` and gates: x-api-key vs Bearer, header casing, TLS profile, beta policy, diagnostics, context_management, count_tokens.
- OAuth (`AU/anthropic_auth.go:20-35`):
  - authorize: `https://claude.ai/oauth/authorize`
  - token (exchange AND refresh): `https://platform.claude.com/v1/oauth/token`
  - profile: GET `https://api.anthropic.com/api/oauth/profile`
  - roles: GET `https://api.anthropic.com/api/oauth/claude_cli/roles`
  - client_id `9d1c250a-e61b-44d9-88ed-5944d1962f5e`
  - redirect_uri `http://localhost:54545/callback`
  - scope (single string) `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload`

## 2. Credential kinds and auth header

- `claudeCreds(auth)` (`claude_executor_request.go:1528`): `apiKey = Attributes["api_key"]`, else `Metadata["access_token"]`; `baseURL = Attributes["base_url"]`.
- OAuth token detection: `strings.Contains(apiKey, "sk-ant-oat")` (`isClaudeOAuthToken`). This is the sole OAuth discriminator for the wire policy.
- Header choice (`PrepareRequest` and `applyClaudeHeadersWithNativeProfile`): if key non-empty and upstream is first-party and credential is "API key" (not OAuth) -> `x-api-key: <key>`, delete Authorization. Otherwise -> `Authorization: Bearer <key>`, delete x-api-key. Third-party base URLs always get Bearer.
- `claudeCredentialUsesOAuth`: OAuth token -> true; `AuthKind==APIKey` -> false; `Attributes.api_key` present -> false; else true (file-backed credential with no api_key attr is Bearer).
- Custom headers: `util.ApplyCustomHeadersFromAttrs` applies `header:*` style attributes; on first-party they cannot override Anthropic-Beta or Accept/Accept-Encoding (re-set after), on streaming third-party Accept is re-set.
- API-key config entry `claude-api-key` (`internal/config/config_types.go:515`): `api-key, base-url, proxy-url, prefix, priority, weight, models[], headers{}, excluded-models, rebuild-mid-system-message, disable-cooling, request-retry, request-scoped-errors, cloak{mode,strict-mode,sensitive-words,cache-user-id}, fingerprint-profile` (`""` or `claude-code-cli`; alias `oauth-cli` normalizes to it). Global: `disable-claude-cloak-mode`, `claude.model-level-cooling`, `claude-header-defaults{user-agent,package-version,runtime-version,os,arch,timeout,timezone,stabilize-device-profile}`.

## 3. Fingerprint policy (decides everything below)

`claude_fingerprint_policy.go: resolveClaudeFingerprintPolicy`:
- `ProfileClaudeCodeCLI = isOAuthToken || profile=="claude-code-cli"` (profile from auth attr/metadata `fingerprint_profile`|`fingerprint-profile`, else matching `claude-api-key` entry matched by api-key case-insensitively and base-url).
- When ProfileClaudeCodeCLI: UseOAuthBetas, ApplyCLIIdentity, MCPAlias, InjectDiagnostics all true. SynthesizeIdentity = profile && !oauth. OAuthCancellation = oauth only.
- Plain first-party API keys with default profile are caller-owned passthrough: no cloaking, headers copied from the caller.

Cloak decision (`resolveClaudeWirePolicy`, `claude_executor_cloaking.go:1318`):
- Native Claude Code caller "confirmed" -> Cloak=false always (passthrough).
- Else Cloak = (ProfileClaudeCodeCLI || explicit cloak config) then overridden by mode: attr `cloak_mode` / config `cloak.mode`: `always` -> true, `never` -> false, `auto`/empty keeps default. `disable-claude-cloak-mode` forces `never` default.
- Cloak attrs may come from `Attributes` or credential JSON metadata keys: `cloak_mode, cloak_strict_mode, cloak_sensitive_words (comma list), cloak_cache_user_id`.

Confirmed native client detection (`helps/claude_client_detection.go:138`): requires `X-App: cli`, User-Agent matching `^claude-cli/\d+\.\d+\.\d+ \(external, <entrypoint>[, agent-sdk/x.y.z]\)$` with version plausible vs baseline (same major.minor, patch >= baseline), `anthropic-beta` containing `claude-code`, and valid `metadata.user_id` (JSON `{device_id 64hex, account_uuid uuid|"", session_id uuid}`; count_tokens exempt). Entrypoint must be one of `cli`, `sdk-cli`, `claude-vscode`. A measured Haiku "helper" shape (model `claude-haiku-4-5-20251001`, no claude-code beta) also confirms. Detection reads original (pre-translation) request headers/body.

## 4. Request pipeline (Messages, `claude_executor_execute.go` and `_stream.go`)

Execute and ExecuteStream share one pipeline; they differ only in response handling. Order:
1. `helps.EnsureSessionContext`; `/responses/compact` -> 501.
2. Model: `thinking.ParseSuffix(req.Model).ModelName` strips thinking suffix (e.g. `-thinking-8192`); optional `upstreamModelNormalizer` (only for delegated providers like Kimi); response model restored to `req.Model` afterward.
3. Target translator format `claude`. `upstreamStream = responseFormat != claude`: if the client is not speaking Claude, upstream is always streamed (SSE) even in the non-stream `Execute`; for Claude clients non-stream stays JSON.
4. Translate (`TranslateRequestPair...`) original+payload; set `model`; `ApplyRequestThinking` (generic thinking applier, `internal/thinking`).
5. Optional `rebuildMidSystemMessagesToTopLevel` (role=system turns in messages folded into top-level system; per-key config).
6. Cloaking (`applyCloakingInternal`) if policy.Cloak (section 6).
7. `context_management` injection (cloaked and first-party only): `{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}` only if absent and `thinking.type` in {enabled, adaptive}; removed again later if thinking gets disabled. Diagnostics injection (section 8).
8. Payload-config rules (`ApplyPayloadConfigWithTrackedPaths`) with protected paths `context_management, fallbacks, thinking.display, diagnostics`.
9. `ensureModelMaxTokens`: if `max_tokens` absent and model is a registered claude model -> registry `MaxCompletionTokens` else 1024 (`defaultModelMaxTokens`); unregistered model leaves it absent.
10. `disableThinkingIfToolChoiceForced`: `tool_choice.type` in {any, tool} -> delete `thinking`, `output_config.effort` (and empty `output_config`).
11. `normalizeClaudeSamplingForUpstream`: non-native: always delete `temperature`, `top_p`; delete `top_k` if thinking active (enabled/adaptive/auto). Native-owned: only fix invalid combos (thinking: temperature must be 1, top_p >= 0.95, no top_k; else drop top_p if both set).
12. cache_control (section 7).
13. `stream` field set to `upstreamStream` (helper profile omits `stream:false`).
14. `extractAndRemoveBetas`: body `betas` (array or string) removed and merged into the beta header list.
15. If MCPAlias && cloaked: tool-name aliasing (section 9). Then `sanitizeClaudeMessagesForClaudeUpstream` (signature sanitizer for thinking blocks from other providers; `internal/signature`) and `sanitizeClaudeWebSearchDomains` (delete empty `allowed_domains`/`blocked_domains` arrays on `web_search_*` tools).
16. If ApplyCLIIdentity: `metadata.user_id` rebuild (section 6).
17. Sensitive-word obfuscation (cloaked only): insert U+200B after first rune of each configured word (words of >= 2 runes) in system and messages text.
18. CCH signing if enabled (section 5). Kimi attribution stripping for Kimi hosts without CLI profile.
19. Validate mid-conversation system messages model support; build POST; headers (section 10); send via Claude TLS client (section 11).

Response model restore: any `model` / `message.model` JSON field (also in `data:` lines) rewritten to the client model name only when a normalizer is configured.

## 5. Billing header, CCH signing (`claude_signing.go`, cloaking.go:130-200)

- System block 0 text (cloaked requests), exact format:
  `x-anthropic-billing-header: cc_version=<ver>.<fp3>; cc_entrypoint=<entry>;` then optional ` cch=00000;`, ` cc_workload=<w>;`, ` cc_is_subagent=true;`, and (only when cch signing) ` cc_prev_req=<id>;`, ` cc_prompt_id=<uuid>;`, ` cc_turn_origin=human;`. Order is exactly this.
- `<ver>` = version parsed from device-profile User-Agent (default `2.1.280`). `<entry>` default `cli`. cloaked requests always claim `cli`.
- fingerprint `fp3` = first 3 hex chars of SHA256(`"59cf53e54c78"` + c4 + c7 + c20 + version) where c_i is the UTF-16 code unit at index i of the first user message text (skips the injected currentDate and context reminder blocks; missing index -> `"0"`; lone surrogate -> U+FFFD in the hashed UTF-8).
- CCH value: xxHash64 (seed `0x4D659218E32A3268`) over the final body bytes with these normalizations (done on raw bytes, NOT re-serialized JSON): the 5 digits after `cch=` set to `00000`; every string value of key `"model"` emptied; object members `max_tokens`, `fallbacks`, `fallback_credit_token` removed at every nesting level (comma handling rules in `addExcludedMemberEdits`, including the quirk that multiple trailing excluded members leave the preceding comma). Result = `format("%05x", hash & 0xFFFFF)` written in place over the placeholder digits. Rust must hash identical bytes; use a byte-level scanner, never serde re-serialization. The placeholder is located by searching `cch=` + 5 lowercase hex + `;` inside `system.0.text` raw JSON.
- If system[0] lacks the billing prefix and a fallback billing header is required, it is prepended as a new text block (string system converted to blocks).
- Signing enabled (`claudeCCHSigningEnabled`): always for OAuth token; for others only if profile claude-code-cli AND base is first-party. Native confirmed helpers without a system field are not given a billing block.

## 6. Cloaking details (`claude_executor_cloaking.go`)

When cloaked (`checkSystemInstructionsWithSigningModeAt`):
- Top-level `system` replaced with `[billingBlock, {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral"}}]` (+ a third "# Reporting outcomes" block for Fable 5.1 models, non-probe).
- Caller system blocks (non-strict): relocated, not dropped. Legacy models (explicit list `claudeLegacySystemReminderModels`, e.g. claude-sonnet-4-5, opus-4-1...) get `<system-reminder>\n<text>\n</system-reminder>` text blocks prepended to the first user message after any leading tool_result blocks (skipping duplicates). All other/unknown models get `{"role":"system","content":[text block with ephemeral cache_control]}` messages inserted after the first user turn(s) (beta `mid-conversation-system-2026-04-07`). If history contains advisor tool blocks, caller blocks stay in top-level system instead. Strict mode drops caller system entirely. Non-text caller system block types are rejected with a request-scoped 400.
- `injectClaudeCodeCurrentDate`: first user message gets a leading text block `<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is YYYY-MM-DD.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n` (after leading tool_result blocks); the first real user text block gets `cache_control:{"type":"ephemeral"}`. Date in timezone from auth attr/metadata `timezone`, then `claude-header-defaults.timezone`, else local.
- Fallbacks: opus-5-5 -> `"fallbacks":[{"model":"claude-opus-4-8"}]`; fable-5-1 -> `[{"model":"claude-opus-5"}]` (only if absent, non-probe). `thinking.display="updates"` set for opus-5-5, fable-5-1, sonnet-5, sonnet-5-5 when thinking enabled/adaptive and display unset.
- Probe/helper requests (max_tokens==1 with trivial text, or title-generation helper) skip continuity, subagent flags, 1h TTL.
- Non-CLI-profile cloak (explicit cloak config on plain key): legacy `metadata.user_id` injection: JSON string `{"device_id":"<64 hex random>","account_uuid":"","session_id":"<uuid>"}` via `GenerateFakeUserIDWithSessionID`; cached per apiKey for 1h only if `cloak_cache_user_id` true (otherwise session id still cached per apiKey 1h, device id random each call). Existing valid user_id preserved.
- CLI-profile identity (`ApplyClaudeCredentialMetadata`, `helps/claude_credential_identity.go:266`): `metadata.user_id` becomes the JSON string `{"device_id":"<cred device id>","account_uuid":"<account uuid>","session_id":"<sessionUUID>"}` (that key order) followed by any extra keys from a pre-existing user_id object (duplicates rejected). Device id = single entry of credential `claude_device_ids`. Account uuid from credential metadata `account_uuid` (required; error if empty). Session UUID = `ClaudeAgentSessionUUIDForRequest`: confirmed native keeps its own session; others derive from request session id, else `uuid5(NameSpaceOID, "cli-proxy-api\x00claude\x00agent-conversation\x00"+identity)`, else random v4. Same UUID goes in `X-Claude-Code-Session-Id`.
- Non-OAuth profile keys synthesize: account_uuid = uuid5(`6ba7b812-9dad-11d1-80b4-00c04fd430c8`, `"cpa-claude-code-cli-account|"+seed`); device id = hex(sha256(`"cpa-claude-code-cli-device|"+seed`)); seed = apiKey (Kimi: `auth-id|<id>` etc).
- Device id: 32 random bytes hex (64 lowercase hex), pool size exactly 1, stored in credential JSON `claude_device_ids` (`AU/identity.go`). Missing/invalid ones are generated and persisted on first use (`PrepareRequestAuth` for OAuth tokens; also fetches OAuth profile to fill `account_uuid`, falling back to a stable synthesized uuid on 403/empty; setup-token credentials skip the profile call).

## 7. cache_control handling

- `shouldEnsureCacheControl = !confirmedNative && (cloaked || no cache_control present)`. When CPA owns it (`ensureCacheControl`): tools get a breakpoint on last non-`defer_loading` tool only if no system is cacheable and none exists; system last block (string system converted to a text block array); last eligible user/assistant message (an assistant turn ending in thinking/redacted_thinking is ineligible; trailing system-role message special-cased), last content block, or string content converted to a text block. Each section injects independently, only if that section has no marker.
- Default marker: `{"type":"ephemeral"}` (no ttl).
- `enforceCacheControlLimit(body, 4)`: remove excess markers in phases: earliest system blocks (keep last), earliest tools (keep last), message blocks earliest-first, then the last system, then last tool.
- TTL: when CPA owns placement and profile is CLI and not probe and (not subagent or subagent requests 1h): every marker without explicit ttl becomes `{"type":"ephemeral","ttl":"1h"}` (keeps `scope` after ttl). For probes and non-1h subagents all ttl fields are stripped. Then `normalizeCacheControlTTL`: walking tools, system, messages in order, any 1h block appearing after a 5m (non-1h) block has its ttl deleted.
- 1h implies beta `extended-cache-ttl-2025-04-11`, which is emitted for OAuth non-subagent non-probe, or when body contains any 1h ttl.

## 8. Diagnostics / continuity (CLI profile on first-party, non-probe)

- Body gets `"diagnostics":{"previous_message_id":null|"<msg_id>"}` (placed right after `context_management` when present). State keyed by sha256(credentialIdentity + `\x00` + sessionUUID), TTL 1h, cleanup every 15 min (`helps/claude_diagnostics.go`). Upstream `message.id` (from SSE `message_start` when `message_stop` seen, or JSON `id`) plus response header `request-id` committed after success to feed next `cc_prev_req` / `previous_message_id`. Prompt id `cc_prompt_id` is a UUID (deterministic: sha256 of `"cpa:prompt:"+firstUserText`, with v4/variant bits forced) for new turns. Beta `cache-diagnosis-2026-04-07` added when `diagnostics` is an object. In-memory only; losing it just resets chains.

## 9. Tool name remap / MCP alias (OAuth/CLI profile, cloaked, first-party or not)

- Every declared client tool (anything without a server-tool `type` prefix: `advisor_`, `agent_toolset_`, `bash_`, `code_execution_`, `computer_`, `memory_`, `text_editor_`, `tool_search_tool_`, `web_fetch_`, `web_search_`) is renamed to `mcp__<word1>_<word2>__<word>_<semantic>` and its `type` field (e.g. `custom`) deleted. Already-valid MCP names (`mcp__x__y`, <= 64 chars, `[A-Za-z0-9_-]`) pass through.
- Derivation (`helps/claude_mcp_alias.go`): HMAC-SHA256 key = secret (downstream caller API key, default `cpa-claude-mcp-default-caller`), message = `"cpa-claude-mcp-alias-v2\x00" + purpose + "\x00" + original`. Server component: purpose `"server"`, words `bip39[BE_u16(d[0:2]) % 2048]` + `_` + `bip39[BE_u16(d[2:4]) % 2048]`. Tool word: purpose `"tool"`, `bip39[(BE_u16(d[0:2]) + attempt) % 2048]` with linear probing past reserved names. Alias = `"mcp__"+server+"__"+toolWord+"_"+suffix`, total <= 64; suffix = original with invalid-char runs collapsed to a single `_`, truncated to the remaining length, trimmed of `_-`, empty -> `tool`. BIP-39 English list must be byte-identical (file embedded).
- Rewrites `tools[].name`, `tool_choice.name`, and `tool_use` / `tool_reference` / tool_addition / tool_removal names in message history using the request-local forward map.
- Response reverse mapping (non-stream JSON and each SSE `data:` line, `reverseRemapOAuthToolNames*`): exact map first, then fuzzy recovery when the model drifts (strips repeated server prefix, unique-suffix match among aliases of the known server); unresolved aliases of a known server raise a request-scoped error.
- Legacy path `applyClaudeToolPrefix` exists but is not on the main flow.

## 10. Headers (`applyClaudeHeadersWithNativeProfile`, claude_executor_request.go:1069)

Two modes:

A) Caller-owned (`preserveCallerFingerprint` = not CLI profile and not cloaked and not confirmed): copy from incoming headers `accept, accept-encoding, user-agent, x-app, x-client-request-id, anthropic-*, x-stainless-*, x-client-app, x-anthropic-additional-protection` (and `x-claude-code-*`, `x-claude-remote-*` only for confirmed). Defaults: `Anthropic-Version: 2023-06-01`, `Accept: application/json`, `Accept-Encoding: gzip, deflate, br, zstd` (streaming on non-first-party: `text/event-stream` and `identity`), `User-Agent: CLIProxyAPI/<version>` if none. Betas: caller header plus body betas verbatim, plus `fast-mode-2026-02-01` if `speed=="fast"`, plus advisor beta when needed; OAuth credential adds `oauth-2025-04-20` after `claude-code-20250219` (or first).

B) CLI profile (OAuth, claude-code-cli, or cloaked):
- `Content-Type: application/json`
- `Anthropic-Version: 2023-06-01`
- `Anthropic-Dangerous-Direct-Browser-Access: true`
- `X-App: cli`
- `X-Stainless-Retry-Count: 0`, `X-Stainless-Runtime: node`, `X-Stainless-Lang: js`
- `X-Stainless-Timeout: 600` (`claude-header-defaults.timeout`; omitted on count_tokens)
- `X-Stainless-Package-Version: 0.112.1`, `X-Stainless-Runtime-Version: v26.3.0`, `X-Stainless-Os: MacOS`, `X-Stainless-Arch: arm64`, `User-Agent: claude-cli/2.1.280 (external, cli)` (all overridable via `claude-header-defaults`). With `stabilize-device-profile` and a confirmed client, the client's own profile is learned/cached per credential (7 day TTL, only upgraded, never downgraded below baseline); otherwise legacy path pins package/runtime versions and takes OS/arch from runtime mapping for confirmed clients.
- `X-Claude-Code-Session-Id: <session uuid>` (same value as metadata.user_id.session_id; fallback cached per-apiKey uuid, 1h).
- Optional passthrough (when present in incoming): `X-Claude-Code-Agent-Id, X-Claude-Code-Parent-Agent-Id, X-Claude-Remote-Container-Id, X-Claude-Remote-Session-Id, X-Client-App, X-Anthropic-Additional-Protection`; confirmed-only: `X-Claude-Code-Request-Class, -Agent-Type, -Prev-Tool-Durations, -Compaction, -Context-Compacted`. Confirmed clients keep their own values for the identity headers above via ensure-header.
- `x-client-request-id: <fresh uuid v4>` only on first-party base URL (helper profile: only if caller sent one).
- `Connection: keep-alive`; `Accept: application/json` and `Accept-Encoding: gzip, deflate, br, zstd` (also for streaming on first-party; for streaming on non-first-party: `text/event-stream` / `identity`).
- Wire casing (first-party only, set immediately before send; `claudeWireHeaderCasing`): `X-Stainless-OS`, `anthropic-beta`, `anthropic-version`, `x-app`, `x-client-request-id`, `anthropic-dangerous-direct-browser-access`. All other names stay Go-canonical (e.g. `X-Stainless-Retry-Count`, `X-Claude-Code-Session-Id`).
- Exact on-wire header order (custom conn writer `httpwire.NewOrderedRequestConn`), messages: `Accept, Authorization, Content-Type, User-Agent, X-Claude-Code-Session-Id, X-Stainless-Arch, X-Stainless-Lang, X-Stainless-OS, X-Stainless-Package-Version, X-Stainless-Retry-Count, X-Stainless-Runtime, X-Stainless-Runtime-Version, X-Stainless-Timeout, anthropic-beta, anthropic-dangerous-direct-browser-access, anthropic-version, x-app, x-client-request-id, Connection, Host, Accept-Encoding, Content-Length`. count_tokens: same minus `X-Stainless-Timeout`. (x-api-key position is not listed: OAuth traffic only.) Rust HTTP stack must allow explicit header order (hyper with custom serialization or raw h1 writer).

### Anthropic-Beta assembly (`claudeCodeCLIBetas`, request.go:179; spec comment lists 29 slots)

Order, comma-joined, no spaces:
1. `claude-code-20250219`
2. `oauth-2025-04-20` (OAuth or CLI-profile "UseOAuthBetas")
3. `context-1m-2025-08-07` only if caller requested it
4. `interleaved-thinking-2025-05-14`
5. `redact-thinking-2026-02-12` (dropped if `thinking.display` set)
6. `thinking-token-count-2026-05-13`
7. `context-management-2025-06-27`
8. `prompt-caching-scope-2026-01-05`
9. `mid-conversation-system-2026-04-07` (all models not in the legacy list)
10. `per-turn-control-2026-07-01` (opus-5-5 / fable-5-1, or requested)
11. `timing-2026-09-09` (per-turn timing body or requested)
12. `mid-conversation-tool-changes-2026-07-01` (non-legacy, not sonnet-5)
13. `inline-tools-2026-09-15` (inline tool_addition or requested)
14. `advisor-tool-2026-03-01`, 15. `advanced-tool-use-2025-11-20`, 16. `mid-conversation-system-clear-at-2026-08-21`, 17. `dangerous-tool-use-2026-09-03` (body has `safeguards`), 18. `effort-2025-11-24` (model supports effort and thinking active), 19. `server-side-fallback-2026-06-01` (body `fallbacks` or requested, not probe), 20. `fallback-credit-2026-06-01`, 21. `structured-outputs-2025-12-15` (requested), 22. `thinking-binding-controls-2026-08-01`, 23. `thinking-display-updates-2026-08-18`, 24. `thinking-resumption-2026-07-17` (requested), 25. `fast-mode-2026-02-01` (`speed:"fast"`), 26. `afk-mode-2026-01-31` (requested), 27. `extended-cache-ttl-2025-04-11`, 28. `prompt-caching-evict-2026-05-12`, 29. `cache-diagnosis-2026-04-07`.
- count_tokens fixed profile: `claude-code-20250219`, [`oauth-2025-04-20`], `interleaved-thinking-2025-05-14`, `context-management-2025-06-27`, `token-counting-2024-11-01` (+ advisor if needed).
- Merge policy: body `betas` and the caller header feed the "requested" set. On first-party, caller betas that are in the managed set (list at request.go:93) are dropped (only the assembled list counts); unmanaged caller betas are appended verbatim (newer-client features). On third-party gateways all caller extras are appended. Confirmed native: its own beta header is the base, with OAuth credential betas inserted (`oauth` at position 2, `extended-cache-ttl` appended unless subagent-without-1h/probe/helper). Post-fixups: strip effort if unsupported; probe: strip server-side-fallback, thinking-display-updates, extended-cache-ttl; thinking disabled: strip display-updates; Haiku without fallbacks: strip server-side-fallback; any 1h ttl in body adds extended-cache-ttl. Dedupe preserves first occurrence.

## 11. TLS / transport

- Selection (`helps.NewUtlsHTTPClient`): host is first-party Anthropic (https, api.anthropic.com, port 443) -> Claude Code TLS profile; chatgpt.com -> Chrome profile (`HelloChrome_Auto`, not Claude); everything else standard transport. Per-credential/global proxy via `proxyutil.BuildDialer` (HTTP/SOCKS); when a proxy is set the standard transport also uses it. Round trippers cached per proxy URL in a bounded LRU (64) with a TLS session cache of 32.
- Inference TLS spec (`claudeCodeTLSClientHelloSpec`, utls_client.go:193) mimics Node/OpenSSL (Claude Code 2.1.220 macOS arm64), HelloCustom: ciphers in order: TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256, ECDHE_ECDSA_AES128_GCM, ECDHE_RSA_AES128_GCM, ECDHE_ECDSA_AES256_GCM, ECDHE_RSA_AES256_GCM, ECDHE_ECDSA_CHACHA20, ECDHE_RSA_CHACHA20, ECDHE_ECDSA_AES128_CBC_SHA, ECDHE_RSA_AES128_CBC_SHA, ECDHE_ECDSA_AES256_CBC_SHA, ECDHE_RSA_AES256_CBC_SHA, RSA_AES128_GCM, RSA_AES256_GCM, RSA_AES128_CBC_SHA, RSA_AES256_CBC_SHA. Compression null. Extensions in order: SNI, extended_master_secret, renegotiation_info (once as client), supported_groups (X25519, P-256, P-384), ec_point_formats [0], session_ticket, ALPN [`http/1.1`], status_request, signature_algorithms (ecdsa_p256_sha256, pss_sha256, pkcs1_sha256, ecdsa_p384_sha384, pss_sha384, pkcs1_sha384, pss_sha512, pkcs1_sha512, pkcs1_sha1), SCT, key_share (X25519), psk_key_exchange_modes (DHE), supported_versions (1.3, 1.2), padding (BoringSSL style), pre_shared_key LAST (omitted until a session is cached). HTTP/1.1 only.
- OAuth control-plane TLS spec (`AU/utls_transport.go`): same ciphers/curves/sigalgs, TLS 1.2-1.3, but NO ALPN, NO status_request, NO SCT, NO padding; extensions: SNI, EMS, renegotiation_info, supported_groups, ec_point_formats, session_ticket, signature_algorithms, key_share, psk_modes, supported_versions, pre_shared_key. Handshake timeout 10s for refresh (context value). Session cache per proxy URL (cap 8 per cache, 64 proxies). HTTP/1.1.
- Rust note: needs a TLS stack with ClientHello control (boring/boring-ssl with custom ciphers/extension order, or rustls fork/`utls`-like). Plain rustls cannot reproduce this. This is a fidelity feature; correctness against api.anthropic.com generally works without it (risk is Cloudflare/anti-abuse flagging).

## 12. Non-stream and stream response handling

- Error path (non-2xx): body decoded by `decodeResponseBody` (Content-Encoding list processed in reverse; gzip, deflate (zlib or raw), br, zstd; if no header, sniff magic bytes gzip `1f 8b` / zstd `28 b5 2f fd`). Then classified (section 13); decode failure message `failed to decode error response body: ...` still classified by status.
- Non-stream success: read whole body; if `upstreamStream` the body is SSE, validated by `validateClaudeStreamingResponse` (502 `statusErr` if: malformed `data:` JSON, `type=error` event ("upstream returned error event: msg"), empty stream, missing `message_start` (needs non-empty `message.id` and `message.model`), or no `message_delta`). Then each line gets usage observed and tool names restored; the full SSE text is passed to `TranslateNonStream`. If not upstream-stream the JSON is translated directly. Empty translated output -> 502 only for apply_patch flow. Usage published after.
- Stream (`ExecuteStream`): `bufio.Scanner` with 50 MB max line; per line: observe id/completion (`data:` payload `message_start` -> id; `message_stop` -> completed), append log, observe model/usage, restore MCP tool names and model name. Claude-format clients: lines accumulated into one event buffer (including the terminating blank line) and flushed as one chunk per event; loop stops after the event containing `message_stop`. Other formats: each line passed to the stream translator (`TranslateStream`), emitted chunks forwarded. Channel capacity 1. After completion, continuity committed (`message_id`, response header `request-id`). Scanner error before `message_stop` -> error chunk; OAuth credentials map context-cancel to a request-scoped `claudeOAuthCancellationError` (no credential cooldown).
- SSE specifics: only `data:` lines are parsed (`strings.TrimSpace` then prefix `data:`); `[DONE]` ignored; event names not parsed; blank line = event boundary.
- Response headers from upstream are forwarded (`Headers: httpResp.Header.Clone()`).

## 13. Error classification and rate limits

`classifyClaudeUpstreamErrorWithCooling(status, headers, body, modelLevelCooling)` (request.go:698):
- Builds `statusErr{code, msg=body, retryAfter}`. `retryAfter` computed for 429 and any 4xx/5xx via `ParseClaudeRateLimitReset`.
- 429: if !modelLevelCooling AND headers show a unified 5h/7d rejection -> `claudeRateLimitError{credentialScoped:true}` (cool down this credential, rotate). Else if body (`error.message` lowercased, or whole body) contains `fast request rejected` or (`fast` and (`usage credits` or `credits are required`)) -> `claudeEntitlementError` (request-scoped, no rotation/cooldown). Else `claudeRateLimitError{credentialScoped:false}` (model-level 429).
- Other statuses: plain `statusErr` (generic pipeline handles 401/402/403/5xx cooldown and retry; that logic lives in `sdk/cliproxy/auth`, not here).
- Unified rejection (`ClaudeHeadersIndicateUnifiedRateLimitRejection`): header names `Anthropic-Ratelimit-Unified-{Status,5h-Status,7d-Status,7d_oi-Status,Overage-Status,Overage-Disabled-Reason,Representative-Claim,5h-Utilization,7d-Utilization,Reset,5h-Reset,7d-Reset,7d_oi-Reset}`. True if 5h-Status or 7d-Status == `rejected`; else if Unified-Status == `rejected` and not an "overage or Fable-only" rejection (7d_oi rejected / overage status rejected / disabled-reason non-empty / representative-claim contains `overage`, while shared 5h and 7d are `allowed`/`allowed_warning`, or one omitted with utilization in [0,1)).
- Cooldown duration (`ParseClaudeRateLimitReset`): candidates: Retry-After (seconds float or HTTP date; skipped for overage-only), 5h-Reset if 5h rejected, 7d-Reset if 7d rejected, 7d_oi-Reset (rejected, not overage-only), Unified-Reset (when unified rejected or neither shared window allowed). Reset values are unix seconds (float) or RFC3339 or HTTP date. Take the latest future deadline; result = (deadline - now) + random whole seconds in [1,30]. No candidates -> nil (generic exponential backoff).
- Fast mode (`speed:"fast"` on first-party, detected by body/beta): errors are wrapped request-scoped (`claudeFastRequestError`, no retry/rotation unless credential-scoped rate limit); non-2xx responses pass through verbatim (status, headers minus Content-Encoding/Content-Length, decoded body) via `claudeFastDirectResponseError`/`RequestTerminatedError`.
- Error interfaces consumed by the auth manager: `StatusCode()`, `RetryAfter() *Duration`, `IsRequestScoped()`, `IsCredentialScoped()`. Request-scoped errors (mid-system model unsupported, bad caller system block, token-count validation 400, MCP alias restore failure, OAuth cancellation) must not trigger credential failover.
- 4xx for token count validation (`validateClaudeTokenCountRequest`): messages must be non-empty array, roles user|assistant, content string or typed blocks.

## 14. Usage extraction (`helps/usage_helpers.go:1118`)

- Non-stream: `usage` object. Stream: any line whose `data:` JSON has `usage` or `message.usage` (merged across `message_start` and `message_delta`; latest wins per field).
- Fields: `input_tokens`, `output_tokens` (includes thinking), `cache_read_input_tokens`, `cache_creation_input_tokens`; reasoning = `output_tokens_details.thinking_tokens` | `output_tokens_details.reasoning_tokens` | `thinking_tokens`. `CachedTokens = cache_read` (falls back to cache_creation if 0). `TotalTokens = input + output + cache_read + cache_creation` (cache fields are independent of input in Messages API). Reasoning > output is clamped for the non-reasoning breakdown.
- Response model captured from `message_start`/`model`.
- count_tokens response: `input_tokens` field.

## 15. OAuth flow (login)

`sdk/auth/claude.go` (Login) + `AU/anthropic_auth.go`:
- PKCE: verifier = 96 random bytes base64url no padding (128 chars); challenge = base64url-nopad(SHA256(verifier)); method `S256` (`AU/pkce.go`). State: `misc.GenerateRandomState()`.
- Authorize URL = `https://claude.ai/oauth/authorize?` + `url.Values.Encode()` (keys sorted alphabetically) of: `code=true, client_id, response_type=code, redirect_uri, scope, code_challenge, code_challenge_method=S256, state`. Scope space-separated (encoded `+`).
- Local callback server (`AU/oauth_server.go`): port 54545 default (override `opts.CallbackPort`), routes `GET /callback` (reads `code`, `state`, `error`; missing code -> `no_code`, missing state -> `no_state`; 400; success redirects 302 to `/success`) and `/success` (HTML). Port availability checked first (error "port N is already in use"). Read/write timeouts 10s. Wait timeout 5 minutes. After 15 s, if a prompt callback exists, user can paste the callback URL manually (`misc.ParseOAuthCallback`). State mismatch -> `ErrInvalidState`.
- Code handling: `code` may be `<code>#<state>`; split on `#`; fragment state wins over the callback state.
- Token exchange: POST JSON to `https://platform.claude.com/v1/oauth/token`, body key order significant (struct order): `{"grant_type":"authorization_code","code":...,"redirect_uri":...,"client_id":...,"code_verifier":...,"state":...}`. Headers: `Accept: application/json, text/plain, */*`, `Content-Type: application/json`, `User-Agent: axios/1.15.2`, `Accept-Encoding: gzip, compress, deflate, br`, `Connection: close` (req.Close). On-wire order: `Accept, Content-Type, User-Agent, Content-Length, Accept-Encoding, Host, Connection`. Response decoded manually for gzip/deflate/br/compress. Non-200 -> error with status+body.
- Response: `{access_token, refresh_token, token_type, expires_in, organization:{uuid,name}, account:{uuid,email_address}}`. `expired = now + expires_in` formatted RFC3339 (local TZ offset, Go `time.RFC3339`).
- Post-exchange companion calls (advisory, failures only logged): GET profile then GET roles with Axios headers (`Accept, Content-Type, Authorization: Bearer, Cache-Control: no-cache, User-Agent: axios/1.15.2, Accept-Encoding, Host, Connection` order). Profile fields (`account.uuid`, `account.email`, `organization.uuid`, `organization.name`) override token response values when non-empty. Profile requires non-empty account uuid.
- Generates a fresh device id pool of 1 (32 random bytes hex). Login fails if email is empty.
- No API key is minted (`ClaudeAuthBundle.APIKey` stays empty; vestigial).

## 16. Token refresh

- Executor `Refresh` (`claude_executor_auth.go:151`): reads `refresh_token` (also `refreshToken`) from credential metadata; empty -> returns auth unchanged. Calls `RefreshTokensWithRetry(ctx, token, 3)` (attempt n waits n seconds; stops on non-retryable). Home-mode delegation `helps.RefreshAuthViaHome` first.
- Refresh request: POST `https://platform.claude.com/v1/oauth/token`, JSON (Go map, so keys serialize alphabetically): `{"client_id":...,"grant_type":"refresh_token","refresh_token":...,"scope":"<same scope string>"}`; same Axios headers; HTTP 30 s total timeout, 10 s TLS handshake.
- Single-flight per refresh token (singleflight), uses a context detached from caller cancellation. 429: parse `Retry-After` (seconds or HTTP date; `Retry-After-Ms`), clamp to [5s, 5min], record per-refresh-token block-until; refreshes during the block fail immediately with a non-retryable 429. 5xx retryable; 4xx (not 429) non-retryable; network errors retryable. Success clears the block. If response lacks `refresh_token`, keep the old one.
- After refresh, GET profile; if it fails, keep tokens and skip identity update. On success update `email, account_uuid, organization_uuid, organization_name`.
- Metadata written back (store skips empty strings for identity fields so identity is never erased): `access_token, refresh_token, expired, type:"claude", last_refresh (RFC3339 now)`.
- Timing: `ClaudeAuthenticator.RefreshLead()` = 4 hours (`sdk/auth/claude.go`), i.e. the auto-refresh loop refreshes when `expired - now <= 4h` (loop in `sdk/cliproxy/auth/auto_refresh_loop.go`, `ProviderRefreshLead`). No other skew is applied by the executor. The request path itself does not refresh inline.
- `ShouldPrepareRequestAuth` (OAuth token only): before a request, ensures device pool and account uuid exist (calls profile with 10 s timeout once; 403/scope errors and setup tokens fall back to a stable synthesized account uuid, `claude_account_profile_checked_at` recorded).

## 17. Credential JSON file schema

`AU/token.go` (`ClaudeTokenStorage`), written with `json.NewEncoder(...).Encode` (trailing newline), dir mode 0700, then arbitrary injected `Metadata` merged at top level (`misc.MergeMetadata`):

```
{
  "id_token": "",                 // always present, empty string in practice
  "access_token": "sk-ant-oat01-...",
  "refresh_token": "sk-ant-ort01-...",
  "last_refresh": "2026-10-01T12:00:00-07:00",   // RFC3339
  "email": "user@example.com",
  "account_uuid": "...",          // omitempty
  "organization_uuid": "...",     // omitempty
  "organization_name": "...",     // omitempty
  "claude_device_ids": ["<64 lowercase hex>"],   // omitempty, exactly 1 entry
  "type": "claude",               // discriminator, forced on save
  "expired": "2026-10-01T20:00:00-07:00"         // RFC3339, note key is "expired"
}
```

- Field order in the Go struct as above (id_token, access_token, refresh_token, last_refresh, email, account_uuid, organization_uuid, organization_name, claude_device_ids, type, expired); Rust can use any order but must keep the names. Extra unknown keys (e.g. `cloak_mode`, `fingerprint_profile`, `timezone`, `skip_account_profile`, `is_setup_token`, `scope`/`scopes`, `proxy_url`, custom headers) must be preserved on rewrite (metadata is a flattened map).
- Loader discrimination: `type == "claude"` selects the provider. A credential whose `access_token` contains `sk-ant-oat` is treated as OAuth; setup-token detection: metadata `skip_account_profile`, `is_setup_token`, `setup_token` booleans, `Attributes.auth_kind` in {setup_token, setup-token}, or `scope(s)` lacking `user:profile`/`user:office`.
- Filename (`AU/filename.go`): `claude-<email>.json` when no org/account identity (legacy); else `claude-<hash8>-<email>.json` where `hash8` = first 8 hex chars of SHA256(trimmed `organization_uuid`, falling back to `account_uuid`). Auth ID = filename. Migration helper `FindMatchingLegacyCredential` finds the older email-only or account-hashed file for the same identity (case-insensitive, matching org/account UUID) to overwrite/replace.
- Timestamps: Go `time.RFC3339` with local offset; Rust should parse any RFC3339 and write RFC3339.

## 18. Gotchas for Rust implementers

- JSON body edits are byte-level (gjson/sjson on raw bytes); several steps depend on exact key order and no HTML escaping (`marshalJSONStringWithoutHTMLEscape`: `<`, `>`, `&` stay literal). `cache_control` objects must serialize as `{"type":...,"ttl":...,"scope":...}`. Use `serde_json` with `preserve_order` and `RawValue`, or a small raw-edit layer; CCH requires hashing the exact final bytes after compaction, so serialization must be done once and the hash computed over that same buffer.
- CCH placeholder digits live inside a JSON string; edit them in place without reflowing.
- `metadata.user_id` is a JSON object serialized into a string (double encoded), with key order `device_id, account_uuid, session_id`.
- Token endpoint refresh body key order is alphabetical (map), exchange body key order is struct order. Authorize URL params are alphabetized by `url.Values.Encode`.
- First-party check must be exact (host, https, port 443, no userinfo); custom base URLs flip many behaviors (Bearer auth, no casing, no diagnostics, local token counting).
- `upstreamStream` is true for non-Claude downstream formats even for non-streaming client calls; usage and tool-name restoration then happen per SSE line, and the translator consumes the full SSE blob.
- `retryAfter` for 5xx/4xx also uses the rate-limit parser; fuzz is random 1-30s per call.
- Sticky/continuity state, device-profile cache (7 d TTL, 1 h cleanup), session/user-id caches (1 h) and diagnostics are process-memory (or Home KV in home mode); not persisted to credential files except `claude_device_ids`, `account_uuid`, profile fields.
- Version constants to expose as config: UA `claude-cli/2.1.280 (external, cli)`, stainless package `0.112.1`, runtime `v26.3.0`, OS `MacOS`, arch `arm64`, billing salt `59cf53e54c78`, CCH seed `0x4D659218E32A3268`, OAuth Axios UA `axios/1.15.2`. These track the real Claude Code release and will be updated upstream often.
- Thinking: generic `ApplyRequestThinking` (outside this scope, `internal/thinking`) maps suffix/level to Claude `thinking` + `output_config.effort`; Claude-specific post-processing here: forced tool_choice removes thinking, display `updates` for new models, context_management gated on thinking enabled/adaptive. Compat API-key providers (`APIKeyModelIsCompat`) also use thinking replay (`claude_thinking_replay.go`): caches assistant `content` with thinking blocks per (credential hash, model, session key) in `internal/cache` and restores them into the next request; cleared on failure. Only for API-key auth that is not OAuth, Claude-format downstream.
- Model mapping: no static table in these files; aliasing/prefix/`excluded-models` come from the config `claude-api-key.models[]` and the registry (`internal/registry`), `claudeLegacySystemReminderModels` and the `isClaude*Model` predicates are the only model-name logic here (all match on lowercased name after last `/`).
# Part C. Codex provider (HTTP, upstream websocket, client /v1/responses websocket)

Repo root `R` = `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI`. Paths below are relative to `R`.

## 0. Scope and non-test line counts

| Group | Lines |
|---|---|
| `internal/runtime/executor/codex_*.go` (17 files) | 7929 |
| `internal/runtime/executor/openai_responses_signature.go` | 226 |
| `internal/runtime/executor/helps/codex_*.go` + `apply_patch*.go` | 1961 |
| `internal/auth/codex/**` | 1421 |
| `sdk/auth/codex.go`, `codex_device.go` | 502 |
| `sdk/api/handlers/openai/openai_responses_websocket*.go` (8 files) | 3777 |
| Total | 15816 |

Largest files: `codex_openai_images.go` 1115, `codex_websockets_session.go` 990, `codex_executor_reasoning.go` 826, `codex_websockets_stream.go` 822, `openai_responses_websocket.go` 909, `openai_responses_websocket_requests.go` 745, `..._toolcall_repair.go` 680, `..._forward.go` 663, `codex_executor_terminal.go` 635.

Related code outside scope that the port still needs (not documented here): translators `internal/translator/codex/*`, `internal/client/codex/optimize-multi-agent-v2` (1046+122 lines), `internal/client/codex/tool-schema` (281), `internal/client/codex/live/*` (Codex Live/realtime, ~4000 lines), `internal/signature/gpt_validation.go`, `helps/usage_helpers.go`, `helps/utls_client.go`, conductor (`sdk/cliproxy/auth`), `internal/cache` codex reasoning replay store.

## 1. Executors and transport selection

- Provider id `"codex"`. Three types in `internal/runtime/executor/`:
  - `CodexExecutor` (`codex_executor.go`): stateless HTTP+SSE.
  - `CodexWebsocketsExecutor` (`codex_websockets_executor.go`): embeds `CodexExecutor`, adds upstream WebSocket with a process-global session store.
  - `CodexAutoExecutor`: the one registered. `Execute`/`ExecuteStream` use the WS executor iff `DownstreamWebsocket(ctx)` (client came in over `/v1/responses` WS) AND `codexWebsocketsEnabled(auth)`; else HTTP. If `RequiredUpstreamWebsocket(ctx)` and WS not usable: return `UpstreamWebsocketReplayRequiredError` (client must replay full turn over HTTP). `Refresh`, `CountTokens`, `PrepareRequest`, `HttpRequest` always delegate to HTTP executor.
- `codexWebsocketsEnabled(auth)`: `auth.Attributes["websockets"]` bool-parse wins; else `auth.Metadata["websockets"]` (bool or string). Default false.
- `SupportsApplyPatch()` is true for both executors (Auto requires both).
- `modelLevelCooling()` = `cfg.Codex.ModelLevelCooling`; scopes quota cooldown to the model instead of the credential.
- HTTP client: `helps.NewUtlsHTTPClient` (uTLS `HelloChrome_Auto` fingerprint for host `chatgpt.com`, fallback transport otherwise; proxy from ctx/auth/cfg). Images path uses `helps.NewProxyAwareHTTPClient` (no uTLS). Rust port needs a Chrome-fingerprint TLS option for chatgpt.com (e.g. rustls with custom hello or boring/wreq-style impersonation).

## 2. Upstream URLs

Base URL default `https://chatgpt.com/backend-api/codex`; override via `auth.Attributes["base_url"]` (API-key auth entries, `codex-api-key` config). Trailing `/` trimmed.

| Use | Method | URL |
|---|---|---|
| Responses (stream and non-stream, both force `stream:true` upstream) | POST | `{base}/responses` |
| Compact | POST | `{base}/responses/compact` (non-stream; `Accept: application/json`; `stream` field deleted) |
| Responses over WS | GET upgrade | `wss://chatgpt.com/backend-api/codex/responses` (http->ws, https->wss scheme swap of `{base}/responses`) |
| Hosted image gen/edit (legacy path) | POST | `{base}/responses` with `image_generation` tool |
| Direct image models | POST | `{base}/images/generations`, `{base}/images/edits` |
| Token refresh | POST | `https://auth.openai.com/oauth/token` |

Local routes serving clients (`internal/api/server_routes.go`): `GET/POST /v1/responses`, `POST /v1/responses/compact`, plus alias group `/backend-api/codex/{responses,responses/compact,alpha/search}` (GET `/responses` is the WS handler). `/v1/alpha/search`, `/v1/live*`, `/v1/realtime*` exist but are other Codex features outside this section.

`CountTokens` is local: tiktoken (`gpt-5*` -> GPT5 codec, `gpt-4.1`, `gpt-4o`, `gpt-4`, `gpt-3*` -> matching; else cl100k) over instructions + message/function_call/function_call_output text + tool name/description/parameters + `text.format` name/schema, joined by `\n`. Returns usage `{input_tokens:N, output_tokens:0, total_tokens:N}` translated to client format.

## 3. Credential and auth model

### 3.1 Credential file (`internal/auth/codex/token.go`, `CodexTokenStorage`)
JSON object, file name via `CredentialFileName`:
```
{ "id_token": str, "access_token": str, "refresh_token": str, "account_id": str,
  "last_refresh": RFC3339, "email": str, "type": "codex", "expired": RFC3339,
  "plan_type": str (omitempty, default "free"), ...extra metadata keys flattened at top level }
```
- `type` is always `"codex"` (set on save; discriminator for loader). `expired` (not `expire`) is the access-token expiry; `last_refresh` RFC3339.
- Files saved with dir mode 0700; extra hook metadata merged into top level via `misc.MergeMetadata`.
- Filename: `codex-{hash8}-{email}-{plan}.json` where `hash8` = first 8 hex of sha256(account_id), `plan` normalized (letters/digits split on non-alnum, lowercased, joined with `-`). Fallbacks drop hash then plan: `codex-{email}-{plan}.json`, `codex-{email}.json` (`filename.go`; provider prefix optional).
- In the runtime Auth: `auth.Metadata` holds the JSON keys (`access_token`, `refresh_token`, `id_token`, `account_id`, `email`, `expired`, `last_refresh`, `plan_type`, `websockets`); `auth.Attributes` holds `plan_type`, `api_key`, `base_url`, `websockets`, `header:*` custom headers, `codex_disable_cloaking`, config index.
- API-key mode ("codex-api-key" config entries): `Attributes["api_key"]`, `Attributes["base_url"]`; `codexAuthUsesAPIKey` = `AuthKind==APIKey` or non-empty `Attributes["api_key"]`. Token chosen by `codexCreds`: `Attributes["api_key"]`, else `Metadata["access_token"]`.
- `resolveCodexKeyConfig` matches the auth to a `cfg.CodexKey[]` entry by config-index attr (validated against key/base) then by case-insensitive api_key+base_url match; gives per-key `Models[]` (`name`, `alias`, `is_compat`) and `DisableCodexCloaking`. `is_compat` model flag changes translation (keeps empty thinking blocks, keeps reasoning_text in `content`, converts agent_message to plain messages).

### 3.2 JWT parsing (`jwt_parser.go`)
No signature verification. Split on `.` (need 3 parts), base64url-decode part[1] with padding fix, unmarshal `JWTClaims`. Used fields: `email`; namespace claim `"https://api.openai.com/auth"` -> `chatgpt_account_id` (account id), `chatgpt_plan_type` (plan, default `"free"` if empty); other fields (organizations, user_id, subscription dates) parsed but unused. Applied to `id_token`.

### 3.3 OAuth (PKCE) constants (`openai_auth.go`)
- Authorize: `https://auth.openai.com/oauth/authorize`; token: `https://auth.openai.com/oauth/token`.
- Client id `app_EMoamEEZ73f0CkXaXp7hrann` (public client, no secret).
- Redirect URI `http://localhost:1455/auth/callback` (callback port 1455; `LoginOptions.CallbackPort` can override port but redirect_uri constant stays 1455, so override only changes the local listener).
- Authorize query: `client_id`, `response_type=code`, `redirect_uri`, `scope=openid email profile offline_access`, `state`, `code_challenge`, `code_challenge_method=S256`, `prompt=login`, `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`.
- PKCE (`pkce.go`): verifier = 96 random bytes base64url no-padding (128 chars); challenge = base64url-nopad(sha256(verifier)).
- Code exchange: POST form-urlencoded `grant_type=authorization_code, client_id, code, redirect_uri, code_verifier`; headers `Content-Type: application/x-www-form-urlencoded`, `Accept: application/json`. Non-200 -> error with body. Response `{access_token, refresh_token, id_token, token_type, expires_in}`. Expire = now + expires_in as RFC3339. Account id/email/plan from id_token JWT (plan default `free`).
- Refresh: POST form `client_id, grant_type=refresh_token, refresh_token, scope="openid profile email"`. Same response handling. Single-flight keyed by refresh token (`singleflight`), 30 s timeout on a context detached from caller cancel.
- `RefreshTokensWithRetry(ctx, rt, 3)`: attempts 3, sleep `attempt` seconds between (0, 1 s, 2 s); aborts immediately if error text contains `refresh_token_reused`.
- `CodexExecutor.Refresh`: no-op if no refresh_token; on success updates Metadata (`id_token`, `access_token`, `refresh_token` if non-empty, `account_id` if non-empty, `email`, `expired`, `type`, `last_refresh`, `plan_type`) and `Attributes.plan_type`; clones `Storage`. Home mode may handle refresh via `helps.RefreshAuthViaHome`.
- Refresh lead: `CodexAuthenticator.RefreshLead() = 24h` (conductor refreshes 24 h before expiry).

### 3.4 Browser login (`sdk/auth/codex.go`, `oauth_server.go`)
Local HTTP server on `:1455` (fails if port busy): `GET /auth/callback` (query `code`, `state`, `error`; errors `no_code`, `no_state`; success redirects 302 to `/success`) and `GET /success` (HTML page, `html_templates.go`, optional `setup_required`, `platform_url` default `https://platform.openai.com`). Read/Write timeouts 10 s. Flow: gen PKCE + random state -> start server -> open browser (or print URL + SSH-tunnel hint when `NoBrowser`/no browser) -> wait up to 5 min; after 15 s, if a prompt callback exists, also accept a pasted callback URL (`misc.ParseOAuthCallback`) -> verify `state` -> exchange code -> `buildAuthRecord`. Typed errors in `errors.go` (port in use, server start failed, callback timeout, invalid state, code exchange failed).
`buildAuthRecord`: requires email; plan from id_token claim; file name per 3.1; Auth{ID=FileName, Provider "codex", Storage=token storage, Metadata{email, plan_type}, Attributes{plan_type}}.

### 3.5 Device-code login (`sdk/auth/codex_device.go`)
Selected when `LoginOptions.Metadata["codex_login_mode"]=="device"`.
1. POST JSON `{"client_id": ClientID}` to `https://auth.openai.com/api/accounts/deviceauth/usercode` (404 -> "endpoint unavailable"). Response `{device_auth_id, user_code | usercode, interval (string or int, default 5 s)}`.
2. Show `https://auth.openai.com/codex/device` and the code; open browser unless `NoBrowser`.
3. Poll POST JSON `{device_auth_id, user_code}` to `https://auth.openai.com/api/accounts/deviceauth/token` every `interval`; 403/404 = pending; 2xx = done; other = fatal; overall timeout 15 min. Response `{authorization_code, code_verifier, code_challenge}` (all required).
4. Exchange via normal token endpoint with `redirect_uri=https://auth.openai.com/deviceauth/callback` and the returned verifier. Then `buildAuthRecord`.

## 4. HTTP request construction (CodexExecutor)

Pipeline for `Execute` / `ExecuteStream` (`codex_executor_execute.go`, `codex_executor_stream.go`), in order:
1. `helps.EnsureSessionContext`. Dispatch: `opts.Alt=="responses/compact"` -> compact (stream variant returns 400 "streaming not supported for /responses/compact"); source format `openai-image` on `/v1/images/{generations,edits}` -> images path (sec. 9).
2. `baseModel = ParseSuffix(req.Model).ModelName` (strips `(...)` thinking suffix).
3. Translate client format -> `codex` format via `sdktranslator.TranslateRequestEnvelope` (baseline "original" payload translated separately when `opts.OriginalRequest` differs, used later by payload-config rules and response translators). Compat Claude->Codex uses `ConvertClaudeRequestToCodexWithCompat`. Source formats seen: openai, openai-response, claude, gemini etc.
4. `ApplyRequestThinking` (reasoning effort from model suffix/config), `ApplyPayloadConfigWithRequestForExecutor` (config `payload` rules keyed by executor id `codex`, or `codex-websockets` on the WS path).
5. Field mutations (HTTP): `model=baseModel`; `stream=true`; delete `previous_response_id`, `generate`, `prompt_cache_retention`, `safety_identifier`; delete `stream_options` except `stream_options.reasoning_summary_delivery` is preserved on the stream path only. Compact: delete `stream`, keep rest. (WS path: keeps `previous_response_id`/`generate`, still deletes `prompt_cache_retention`, `safety_identifier`.)
6. `normalizeCodexInstructions`: if not a native Codex client request and `instructions` missing/null, set `instructions:""`. Native = source and response format both in {codex, openai-response} AND Responses-Lite marker (header `X-OpenAI-Internal-Codex-Responses-Lite: true` or body `client_metadata.ws_request_header_x_openai_internal_codex_responses_lite` true); native requests pass through untouched and responses are not output-patched.
7. `ensureImageGenerationTool` (unless `cfg.DisableImageGeneration` != Off): append `{"type":"image_generation","output_format":"png"}` to `tools` (create array if absent) unless: Responses-Lite request, model ends with `spark`, auth is `plan_type=="free"`, or tools already contain `image_generation` or the function/namespace form `image_gen.imagegen`.
8. `sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat` (`openai_responses_signature.go`): for each input item of type `reasoning`: (non-compat) if `content` array non-empty, promote `reasoning_text` parts into `summary` (as `summary_text`) when summary empty, then force `content=[]`; if `encrypted_content` absent and `store` not true, drop item `id` (avoids "Item not found, store=false"); if `encrypted_content` present, validate via `InspectGPTReasoningSignature` (trimmed, <=32 MiB, prefix `gAAAA`, base64url charset, decoded >= 73 bytes, first byte 0x80, ciphertext len = decoded-57 positive multiple of 16); invalid -> delete `encrypted_content` (and `id` if non-compat and store off).
9. `normalizeCodexParallelToolCalls`: Lite request -> `parallel_tool_calls=false`; otherwise delete `parallel_tool_calls` if tools array empty/absent (WS path only does the Lite -> false part).
10. `helps.NormalizeCodexToolSchemas`: for `function`/`custom` tools (recursing into `namespace.tools`): strip `pattern`/`patternProperties` keys that use unsupported `\p{..}`/`\P{..}` or `\0`; schema-aware; collapse pure-constant `oneOf`/`anyOf` unions with >= 8 branches into `enum` lists. Does NOT change number->integer for Codex targets.
11. `OptimizeCodexMultiAgentV2RequestForAuth` (`helps/codex_multi_agent_v2.go` -> `internal/client/codex/optimize-multi-agent-v2`): orphan-delegation rewrite (if `codex.orphan-delegation-compatibility` and header `X-Openai-Subagent: collab_spawn`); if `client.codex.optimize-multi-agent-v2` and UA is a Codex multi-agent client: rewrite `spawn_agent` description (marker "Spawns an agent", inject available-model list heading "Available model overrides (optional; inherited parent model is preferred):"), rename namespace `collaboration` -> `collaboration-optimize` (prefix `collaboration-optimize__` / `.`); strip `message.encrypted` property of `spawn_agent`/`send_message`/`followup_task`; for compat models convert `agent_message` input items to portable `message/user` and drop `author`, `recipient`, `internal_chat_message_metadata_passthrough`. Responses are reverse-mapped by `RestoreCodexMultiAgentV2Response` when `optimized` is true (skipped if request itself defined `collaboration-optimize` namespace: "conflict"). Handler also calls `PrepareCodexMultiAgentV2Tools` at the WS boundary.
12. Reasoning replay cache (Claude source only): `applyCodexReasoningReplayCacheRequired` looks up cached `reasoning` / `function_call` / `custom_tool_call` items from the prior turn, keyed `(model, sessionKey)`, and re-inserts them in the right turn positions (anchor by prefix fingerprint sha256 of input items, assistant message fingerprint, call ids; call ids shortened/aligned). Session key precedence: Claude Code execution scope; `execution:{id}` metadata; payload `prompt_cache_key` -> `prompt-cache:`; `client_metadata.x-codex-window-id` -> `window:`; turn-metadata JSON; headers `X-Codex-Turn-Metadata`, `X-Codex-Window-Id`, `Session_id`/`Session-Id` -> `session-id:`, `Conversation_id`; OpenAI source: `prompt-cache:{uuidv5(OID,"cli-proxy-api:codex:prompt-cache:"+apiKey)}`. On completion (`response.completed`/`response.done`, not incomplete) `cacheCodexReasoningReplayFromCompleted` appends a marker item (`CodexReasoningReplayTurnType`, id = sha256 of fingerprints + items) + the reasoning/tool-call output items. On an error classified `thinking_signature_invalid` the session's cache entry is deleted. Store failures return errors ("Required" variants).
13. `prompt_cache_key` + `Session-Id` (`cacheHelper`): Claude source -> `ClaudeCodePromptCache` = uuidv5(OID, `"cli-proxy-api:codex:claude-code\0{model}\0{executionScope}"`); openai-response -> body `prompt_cache_key`; openai chat -> body `prompt_cache_key` else `ProviderSessionUUID("codex", req.Metadata)` else uuidv5 of API key (above). Fallback for all: `ProviderSessionUUID("codex", metadata)` = uuidv5(OID, `"cli-proxy-api\0codex\0execution-session\0{id}"`) if execution-session metadata exists, else derived-session variant. If non-empty: set body `prompt_cache_key` and header `Session-Id`.
14. `SanitizeCodexInputItemIDs` (`helps/codex_input_ids.go`): limit 64 runes. Per type prefix: `message`->`msg`, `reasoning`->`rs`, `function_call`->`fc`, `custom_tool_call`->`ctc`, `custom_tool_call_output`->`ctco` (id rewritten to `{prefix}_{id}` unless it already starts with prefix). Encrypted reasoning items with id > 64 runes are dropped. Other ids > 64 shortened to `prefix[..64-17] + "_" + hex(sha256(id)[:8])`, collision-resolved by hashing `id\0attempt`. Preserved ids win collisions.
15. `service_tier`: not rewritten by the executor; whatever the translator/payload rules left in body is sent, and echoed into `X-Codex-Routing-Hint` (sec. 5). Response `service_tier` captured into usage.
16. Reasoning/include/store defaults are produced by the translator (`internal/translator/codex/*`), e.g. `store:false`, `include:["reasoning.encrypted_content"]`, `reasoning:{effort,summary:"auto"}`, `parallel_tool_calls:true` (the images path shows the canonical template: `{"instructions":"","stream":true,"reasoning":{"effort":"medium","summary":"auto"},"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"model":"","store":false,"tool_choice":..., "tools":[]}`). Port must read the translator spec for exact defaults.

### apply_patch bridge
- `custom` tool named `apply_patch` (freeform grammar) is native to Codex: codex executor passes it through unchanged (`SupportsApplyPatch`). For non-native executors (OpenAI-compat, etc.) `internal/client/codex/apply-patch/tool.go` + `helps/apply_patch*.go` convert it to a function tool with schema `{"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}` and an instruction preamble describing the Codex patch grammar (`*** Begin Patch` ... `*** End Patch`), wrap/unwrap arguments `{"input": patchText}` (strict: single `input` string key, no trailing JSON), and validate streamed argument deltas. Failure maps to 502 with message `Invalid apply_patch tool arguments received from upstream.` (`ApplyPatchTranslationError`). Codex executor still checks `ApplyPatchTranslationError(param)` after translation and returns 502 on error. `models.go` advertises `apply_patch_tool_type:"freeform"` per model when every routing candidate supports it. Port priority: low for Codex-only deployments; needed for chat->Responses cross-provider routing.

## 5. Headers

### HTTP/SSE (`applyCodexHeadersFromSources`, `codex_executor_request.go`)
Constants: `codexUserAgent = "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)"`, `codexOriginator = "codex-tui"`. Source for passthrough = client request headers (`opts.Headers`).
- Always: `Content-Type: application/json`; `Authorization: Bearer {token}` (deleted if token empty); `Connection: Keep-Alive`; `Accept: text/event-stream` (stream/Execute) or `application/json` (compact, images direct).
- Copy-if-present from client: `X-Codex-Beta-Features`, `Version`, `X-Codex-Turn-Metadata`, `X-Codex-Turn-State`, `X-Client-Request-Id`, `X-Codex-Window-Id`, `Thread-Id`, `Session-Id`, `X-Openai-Internal-Codex-Responses-Lite`.
- `User-Agent`: (already set) > `cfg.codex-header-defaults.user-agent` (OAuth auths only) > client UA > `codexUserAgent`.
- `Originator`: client `Originator` if present; else (OAuth only) `codex-tui`. API-key auths get none unless client sent it.
- `Chatgpt-Account-Id`: `auth.Metadata["account_id"]` for OAuth auths (not API-key).
- `Session-Id`: set by `cacheHelper` from prompt cache id (client value kept by `EnsureHeader` if set earlier; note cacheHelper sets first).
- Custom headers: `util.ApplyCustomHeadersFromAttrs(req, attrs, clientHeaders)` applies auth `header:*` attributes (static or `$Header` references to client headers).
- Cloaking (`applyCodexCloakingHeaders`): unless disabled (auth attr `codex_disable_cloaking` bool > matching `codex-api-key` entry `disable-codex-cloaking` > `codex.disable-codex-cloaking`), force `User-Agent=codexUserAgent` and `Originator=codex-tui` (overrides everything above). Note: only active when `cfg != nil`.
- `X-Codex-Routing-Hint` (`applyCodexRoutingHint`, OAuth auths only): `model={baseModel}` plus `;tier={service_tier}` if final body has non-empty string `service_tier`. Any client-sent hint is deleted and replaced; an operator `header:X-Codex-Routing-Hint` rule that resolves non-empty wins.
- `applyModelHeaderOverrides`: `registry.ModelOverrideHeaders(model)` from models.json `override_header` forced via `Set`; if UA contains "Mac OS" and no session header, adds `Session_id: {uuid}`.
- Direct image calls (`applyCodexDirectImageHeaders`): same but client `User-Agent` dropped (avoid Cloudflare 1010), non-stream accept.

### WebSocket handshake (`applyCodexWebsocketHeaders`, `codex_websockets_request.go`)
Built from `applyCodexPromptCacheHeadersWithContext` (sets `session_id` (lower-case, case preserved) and `Conversation_id` = cache id; also sets body `prompt_cache_key`; Claude/openai-response sources as above, else `ProviderSessionUUID`; note openai-chat source does NOT derive from API key here). Then:
- `Authorization: Bearer {token}`.
- Copy-if-absent from client: `x-codex-turn-state`, `x-codex-turn-metadata`, `x-client-request-id`, `x-responsesapi-include-timing-metrics`, `Version`; `x-codex-beta-features` priority: existing > client > `cfg.codex-header-defaults.beta-features` (OAuth only); `X-OpenAI-Internal-Codex-Responses-Lite` only for native requests.
- `User-Agent`: API-key auth -> client UA only; OAuth -> config > client > `codexUserAgent`.
- `OpenAI-Beta`: client value if it contains `responses_websockets=`, else `responses_websockets=2026-02-06`.
- `session_id` (lowercase key, preserved case): existing/client `Session-Id`/`Session_id`/`session_id`, else fresh UUID when UA contains "Mac OS"; `Session-Id` header deleted so only `session_id` is sent.
- If native request and cloaking disabled: client `session-id/session_id/conversation_id/thread-id/x-codex-routing-hint/x-codex-window-id` are forwarded verbatim instead.
- `Originator`: client else `codex-tui` (OAuth only). `ChatGPT-Account-ID` (this exact casing) from `Metadata.account_id` (trimmed; OAuth only).
- Custom headers then cloaking (as HTTP), then routing hint, then model overrides.
- gorilla adds `Upgrade`, `Connection`, `Sec-WebSocket-*`, `Sec-WebSocket-Extensions: permessage-deflate` (compression negotiated; outbound compression disabled).

## 6. Streaming SSE handling (HTTP)

Reading: `bufio.Scanner`, 50 MiB max line. Only lines starting `data:` are parsed (trim); other lines (comments, `event:`, blanks) forwarded to the translator as-is (cloned) so the downstream sees valid SSE. Per `data:` event, in order:
1. `RestoreCodexMultiAgentV2Response` (if optimized); observe TTFT/model (`ObserveResponsesTokenEvent`, `ObserveCodexResponseModel`).
2. `codexTerminalFailureErrWithCooling(event)`: matches `type=="error"` (body from `error`, or top-level `message/code/error_type/param`) and `type=="response.failed"` (`response.error`, fallback `error`). Builds `{"error":{...}}` body (message fallbacks: `response.error.message`, code, type; else "upstream stream failed without error details"; copies `sequence_number`). Status: `error.status_code`/`error.status` in 400..599, else mapped from type/code (`cyber_policy`->400; `not_found_error|not_found|model_not_found`->404; `authentication_error|invalid_api_key|unauthorized`->401; `permission_error|forbidden|permission_denied`->403; `rate_limit_error|rate_limit_exceeded`->429; `invalid_request_error|bad_request_error`->400; else 502). Context-length, usage-limit, model-capacity and `thinking_signature_invalid` bodies are normalized to 400 via `codexTerminalStreamErr` first, then `newCodexStatusErrWithCooling` upgrades usage-limit/capacity to 429.
3. `HasMeaningfulCodexOutputDelta` tracks `response.output_text.delta`, `response.reasoning_text.delta`, `response.reasoning_summary_text.delta`, `response.function_call_arguments.delta` with non-blank `delta`.
4. Empty-incomplete check: `response.incomplete` with no deltas seen, zero collected output items, empty `response.output`, and `response.usage.output_tokens` literally numeric `0` -> error 502 `stream error: upstream terminated with incomplete empty response (0 tokens)` (request-scoped: no credential penalty/failover).
5. `response.output_item.done`: collected by `output_index` (map) or in fallback list.
6. Terminal success set = {`response.completed`, `response.incomplete`, `response.done`}: `response.done` is rewritten to `response.completed` (`normalizeCodexWebsocketCompletion`); usage published (`ParseCodexUsage`, or `EnsurePublished` if absent); image tool usage published as extra model; for non-native requests `patchCodexCompletedOutput`: if `response.output` is empty/missing and items were collected, rebuild `response.output` (sorted by index, then fallback), else hydrate missing `id`s of existing items from the `output_item.done` copies; replay cache updated for completed/done. The frame is translated to the client format; the stream goroutine ends after a terminal success.
7. End-of-stream without a terminal event: if nothing was ever emitted -> 502 `upstream stream closed before first payload` (no chunk error on unbuffered path, just recorded); else error 408 `stream error: stream disconnected before completion: stream closed before response.completed` (request-scoped, flagged so conductor does not cool the credential). Scanner errors with a cancelled context are ignored.
- Non-stream `Execute`: reads whole body, loops `data:` lines the same way, and on first `response.completed|response.incomplete` translates with `TranslateNonStream`; no terminal -> `newCodexIncompleteStreamError` (408 as above). `response.incomplete` non-empty is returned as a success.
- Response headers: upstream headers cloned into `StreamResult.Headers`/`Response.Headers`.

### Bootstrap buffering (`cfg.codex.stream-bootstrap-buffering`, default off; `stream-bootstrap-timeout` default 0 = unbounded)
Hold downstream response headers until first real token so a `server_is_overloaded`-style rejection that upstream tucks inside an HTTP 200 stream can fail over to another credential. Bufferable events (closed list): empty `data:` heartbeat, `response.created`, `response.in_progress`, `codex.rate_limits`, `codex.response.metadata`, `keepalive`, `response.output_item.added` for empty message / reasoning (no summary, content or encrypted_content) / function_call with empty `arguments` / custom_tool_call with empty `input`, `response.content_part.added` and `response.reasoning_summary_part.added` with empty text/refusal part. Anything else releases the stream. Budgets: 48 frames (SSE: per line read; WS: per message read), 1 MiB (frame + translated bytes), optional time. If a terminal failure arrives while buffering and `isCodexOverloadBootstrapFailure(body)` (model capacity, `service_unavailable_error`, `server_is_overloaded`, `rate_limit_error|rate_limit_exceeded`, or `server_error` whose message contains "you can retry your request") and budget not exhausted -> return error with status 503 (`newCodexBootstrapOverloadErr`) from `ExecuteStream` so the conductor retries elsewhere. Other terminal errors are queued and delivered in-stream after flushing buffered frames. Stream ending while buffering -> 502 "upstream stream closed before first payload" (if nothing seen; returns an empty closed stream with headers) else 408 incomplete error.

## 7. Error classification, cooldown, retries

`newCodexStatusErrWithCooling(status, body, modelLevelCooling)` (`codex_executor_terminal.go`) builds `statusErr{code, msg, retryAfter, credentialScoped}`:
- If body is usage-limit (`error.type` or top-level `type` equals `usage_limit_reached`, case-insens.) or model-capacity (message/body contains "model is at capacity", `model_at_capacity`, `model_is_at_capacity`, or "model" + "at capacity"): status forced to 429.
- `credentialScoped = usage_limit && !modelLevelCooling` (cooldown applies to whole credential vs only the model).
- `retryAfter` (only status 429 + usage_limit_reached): from `error.resets_at` (or top-level) unix seconds if in the future -> `resets_at - now`; else `resets_in_seconds` > 0 -> that many seconds; else nil (conductor default backoff). Other 429s (`rate_limit_error`) carry no retryAfter (transient, retried not cooled).
- Body rewrite `classifyCodexStatusError` to `{"error":{"message","type","code"}}` for: `context_too_large` (413, `context_length_exceeded|context_too_large`, or invalid-request message with "context length"/"maximum context"/"too many tokens") type `invalid_request_error`; `thinking_signature_invalid` (body contains "invalid signature in thinking block" or `invalid_encrypted_content`); `previous_response_not_found` (code or text with `previous_response_id` + "not found"); `auth_unavailable` type `authentication_error` (401, `authentication_error`, `invalid_api_key`, "invalid or expired token", `refresh_token_reused`). Otherwise body is passed through verbatim.
- HTTP non-2xx: read full body, optional replay-cache clear, `RecordAPIResponse*`, return the error. There is no retry loop inside the executor; retries/credential rotation/cooldown scheduling are in the conductor (`sdk/cliproxy/auth`), which consumes `StatusCode()`, `RetryAfter()`, `IsCredentialScoped()`, `IsRequestScoped()`, `Headers()`.
- Errors flagged request-scoped (`IsRequestScoped()==true`): incomplete stream 408, empty-incomplete 502, WS message-too-big 413, duplex connection errors.
- WS error frame mapping: see 8.6.
- Quota headers: `helps/codex_quota.go` `ParseCodexQuotaEventHeaders` converts a WS `codex.rate_limits` event (or error frame `headers`) into `X-Codex-*` pseudo response headers: `X-Codex-{Primary,Secondary}-{Used-Percent,Window-Minutes,Reset-After-Seconds,Reset-At}`, `X-Codex-Code-Review-*`, `X-Codex-Credits-{Has-Credits,Unlimited,Balance}`, `X-Codex-Active-Limit`, `X-Codex-Plan-Type`, additional limits as `X-Codex-Additional-{NAME}-*` (max 8, with `-Limit-Name`). Accepts snake and camel keys; values validated (no control chars). The HTTP path forwards real `x-codex-*` response headers via the stored upstream headers. `/wham/usage` probe exists elsewhere (quota refresh).

## 8. WebSocket executor full protocol spec (`codex_websockets_*.go`)

### 8.1 Connection and handshake
- URL: `wss://chatgpt.com/backend-api/codex/responses` (or base_url with http->ws/https->wss; other schemes error). Single URL for all models.
- Dialer (gorilla): `HandshakeTimeout 30 s`, `EnableCompression=true` (permessage-deflate negotiated, but `EnableWriteCompression(false)` after connect to dodge flate-tail issues), TCP dial timeout 30 s keepalive 30 s. No read limit set. Proxy resolution: ctx request proxy > `auth.ProxyURL` > `cfg.ProxyURL`; `direct` disables; `http/https` via `ProxyURL`; `socks5/socks5h` via `golang.org/x/net/proxy`; fallback `ProxyFromEnvironment`.
- Handshake headers: sec. 5 WS list. Handshake response headers are surfaced as `StreamResult.Headers` (stream) and logged.
- Handshake failures: HTTP 426 (Upgrade Required) -> if no `ExecutionLifecycle` and not downstream WS: transparently fall back to the HTTP executor (`CodexExecutor.Execute/ExecuteStream`); otherwise return status error 426. Any other handshake status -> `newCodexStatusErrWithCooling(status, body)` (so 401/429/usage_limit classification applies). Transport dial error with no response -> raw error. `MarkUpstreamAttempt(ctx)` called when the dial really attempted so the conductor knows the upstream was touched.

### 8.2 Session keying and pooling
- Global store `globalCodexWebsocketSessionStore: map[sessionID]*codexWebsocketSession` (mutex-protected). `sessionID` = the execution session id from `opts.Metadata[ExecutionSessionMetadataKey]` (the client handler passes its per-socket UUID `passthroughSessionID`). Missing id -> ephemeral session (one connection per request, closed on completion/error with reason `completed`/`error`/`send_error`/etc.).
- A persistent session holds exactly ONE upstream connection plus metadata `{wsURL, authID, proxyURL}`. Reuse rule: connection reused iff `(authID, wsURL, proxyURL)` all equal the stored triple; if the target changes the old conn is closed (reason `target_changed`) and its lifecycle ended, then redial. So the pool key is the client-side execution session, not the credential; there is no cross-session pooling.
- Per-session `reqMu` serializes requests (lock held from request start until stream completion or error; released early on terminal failure so the next request can redial).
- `ensureUpstreamConn` race: if another request installed a conn while dialing, close the new one and reuse existing.
- Per-connection reader goroutine `readUpstreamLoop` (started once per conn): sets read deadline `now + 5 min` before every `ReadMessage` (idle timeout); on read error pushes terminal `codexWebsocketRead{err}` to the active request channel, invalidates the conn (reason `upstream_disconnected`) and exits. Binary frames -> error `unexpected binary message` + invalidate. Text payloads are trimmed, `type` recorded as `lastEventType`, and pushed to the active request's channel (buffered 4096; `activate(conn)` creates it). With no active request, frames are dropped.
- `invalidateUpstreamConn(sess, conn, reason, err)`: clears conn/closer/multiAgent flag, closes conn once, ends the bound `ExecutionLifecycle(reason)`, and (when `notify`) fires `upstreamDisconnectCh` once (buffered 1, then closed) which the client WS handler listens to (see 11.1).
- `CloseExecutionSession(id)`: delete + close (reason `session_closed`); special id `CloseAllExecutionSessionsID` closes all (`executor_shutdown`). `CloseCodexWebsocketSessionsForAuthID(authID, reason)` closes every session whose conn belongs to that auth (called on auth removal).
- Lifecycle binding (`bindExecutionLifecycle`): binds a close callback into the caller's `ExecutionLifecycle` (`Bind`, `Retain`), replacing/ending previous lifecycle (`target_replaced`). Ephemeral sessions bind the raw closer via `BindExecutionResource`.

### 8.3 Pings, liveness, deadlines
- Upstream -> us pings: custom ping handler replies with a pong via `WriteControl` (10 s deadline, bypasses `writeMu` so long writes do not starve pongs). Our code never sends pings upstream.
- Idle timeout: 5 min read deadline reset on each read in `readUpstreamLoop` (so a quiet turn longer than 5 min with no frames kills the connection). Pings answered by the handler do not extend the deadline beyond the next read attempt (deadline set before each `ReadMessage`; gorilla processes control frames inside `ReadMessage`, so a ping does not reset it; port should reset deadline on any received frame or accept the same behavior).
- Close handler records a `CloseError{code,text}` as the connection's disconnect error (used to map write failures to 1009 message-too-big).
- Write: `writeMu`-serialized; payloads > 32 KiB written in 32 KiB chunks through `NextWriter`.

### 8.4 Request frame
- Body is the same translated body as HTTP (sec. 4) with additions: `type:"response.create"` (always; `response.append` is never sent upstream), `SanitizeCodexInputItemIDs` applied again, `prompt_cache_key` set. Frame = JSON text message. Fields kept for WS: `previous_response_id` (continuation), `generate` (prewarm), `stream:true`, `store`, `include`, `tools`, `instructions`, `input`, `reasoning`, `service_tier`, `client_metadata`, etc.
- Instructions: `normalizeCodexInstructions` still defaults to `""` for non-native.
- Request log method string `"WEBSOCKET"`.

### 8.5 Continuation
- The executor does no stateful merging. Continuation relies on the client handler (sec. 11): when downstream WS client and upstream WS auth are paired ("native passthrough"), the handler forwards `previous_response_id` + incremental `input` unchanged as a `response.create`, and the executor reuses the same upstream conn so upstream can resolve the previous response from its per-connection cache. If the conn dropped, the handler gets a replay-required error (close code 1012 with reason `upstream requires HTTP replay`) so the client resends the full transcript.
- `RequiredUpstreamWebsocket(ctx)` set by handler for continuation requests: executor must use the EXISTING conn via `existingWebsocketSessionConn` (matching authID/url/proxy, no disconnect error); if absent -> `UpstreamWebsocketReplayRequiredError`; send errors on such requests invalidate silently (no disconnect notify) and also yield replay-required unless request-scoped.
- Without that flag, a request on a persistent session that fails its first send is retried ONCE on a freshly dialed conn (rebuilds frame, re-binds lifecycle, logs retry). Request-scoped errors (message too big) are not retried.

### 8.6 Event handling (Execute non-stream and ExecuteStream)
Per text frame: trim; skip empty; `observeCodexTokenEvent`; append to API log; emit websocket response event to hooks (`EmitWebSocketResponseEvent`); restore multi-agent namespace; then:
1. `parseCodexWebsocketErrorWithCooling`: frame `type=="error"` with integer `status` (or `status_code`) > 0 -> `statusErrWithHeaders`. Body rebuilt as `{"status":N,"body":<body?>,"error":<body.error | error | {type:"server_error",message:StatusText}>}`; optional `headers` object (string/number/bool values) copied to error headers (carries `x-codex-*` quota info and `retry-after` style data). usage_limit -> credentialScoped unless model cooling; `retryAfter` from `resets_at/resets_in_seconds`; error code/type `websocket_connection_limit_reached` -> `retryAfter=0` (immediate retry on another connection). Action: invalidate conn (`upstream_error`), clear replay cache if signature-invalid, return error. Error frames without a status are not errors here and fall to step 2.
2. `codexTerminalFailureErrWithCooling` (same as SSE; `response.failed`, status-less `error`): invalidate conn (`terminal_failure`), release session lock, return error.
3. Delta/item tracking, empty-incomplete check (invalidates conn `terminal_empty_incomplete`).
4. Terminal events: `response.completed`, `response.done` (normalized to completed), `response.incomplete` complete the turn (usage, replay cache for non-incomplete, `patchCodexCompletedOutput` unless native). `response.failed` / `error` count as terminal for loop exit. Handled/pass-through events: `response.created`, `response.in_progress`, `response.output_item.added/done`, `response.content_part.*`, `response.output_text.delta/done`, `response.reasoning_*`, `response.function_call_arguments.*`, `codex.rate_limits`, `codex.response.metadata`, `response.steer.*` (duplex), and any unknown type (forwarded untouched).
- Non-stream: returns on first terminal as above (no explicit "stream ended early" branch: read error -> returned, mapped via `mapCodexWebsocketReadError`: close code 1009 -> 413 `{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}`, request-scoped).
- Stream (non-duplex): after terminal, `clearActive`, unlock session (persistent session conn stays open for the next turn); ephemeral closes. Downstream-WS mode: payload forwarded as raw JSON (after `EnsureResponsesUsageDetails`, completion output patched unless native) one chunk per frame; non-WS downstream (HTTP client but WS upstream): frame re-encoded as `data: {json}` SSE line and sent through the translator.
- Context cancel: persistent session unlocks and clears active; ephemeral closes (`context_done`). Read error: conn invalidated (`read_error`), error returned or sent as `StreamChunk{Err}`.
- Bootstrap buffering identical in spirit to SSE (sec. 6), frames counted per websocket message read; WS terminal failures during buffering invalidate conn without disconnect notify when failing over (`terminal_failure` silent) so the client WS is not torn down.

### 8.7 Fallback to HTTP
Only: (a) `opts.Alt=="responses/compact"` always uses HTTP; (b) handshake 426 when not a lifecycle-bound/downstream-WS request; (c) `CodexAutoExecutor` picks HTTP for non-WS downstream or non-websocket-enabled auth. There is no mid-stream WS->HTTP fallback; downstream WS clients get replay-required close (1012) and re-issue over their own transport.

### 8.8 Duplex / response steering (`codex_websockets_duplex.go`)
Enabled when `cfg.codex.response-steering` (or `CodexResponseSteering` SDK mirror; may be limited to OAuth auths via `OAuthOnlyFields["codex.response-steering"]`) AND the downstream WS provided an input channel (`WebsocketInputFromContext`). After the first `response.create` was written by `ExecuteStream`, control hands to `streamCodexDuplex`, which keeps the socket until the downstream disconnects (a response terminal event does NOT end the stream).
- Writer goroutine consumes downstream frames (`message.Payload`, bounded queue 16): invalid JSON -> local error event `{"type":"error","status":400,"error":{"type":"invalid_request_error","message":...}}`; types: `response.steer` (forwarded byte-for-byte upstream, registers an unacknowledged steer against `previous_response_id`; requires parent settings or waits until `pending` creates drain), `response.create`/`response.append` (queued, max 16 outstanding; sent only when no unacknowledged steers, no automatic successor active, and accepted steers only wait on `waitingParent`), anything else -> local 400 "unsupported websocket request type". `response.append` gets `previous_response_id` = last response id and inherits `instructions`. A create whose `model` differs from the session model, or resolves to a different wsURL, aborts the stream with replay-required. Each create is re-run through `prepareCodexWebsocketStream` (full pipeline). Before every write the credential must still be enabled (`WebsocketAuthEnabled`) else the stream fails.
- Reader goroutine tracks: `response.created` (establishes first response; pops next pending prepared request or inherits parent settings for automatic successors; snapshots `reasoning` and `instructions` per response id, last 16 retained), `response.steer.accepted|failed|pending` (opaque pass-through; updates `unacknowledgedSteers`, `acceptedSteers`, `waitingParent`), `error`/`response.failed` (after first response: 401/403/429 -> emit frame then `StreamChunk.Err` and end; failure attribution to current/pending request by response id, ambiguous -> connection error), completion events (patch output, usage per response with a new `UsageReporter` per response after the first).
- Connection failures in duplex mode are wrapped `codexDuplexConnectionError` (request-scoped, never cools the credential, no redial/replay). On exit the conn is invalidated (`duplex_closed`) and the writer joined.

## 9. Usage extraction
`helps.ParseCodexUsage(event)`: reads `response.usage`; requires `total_tokens` or any input/output bucket field; maps `input_tokens|prompt_tokens`, `output_tokens|completion_tokens`, `total_tokens`, `input_tokens_details.cached_tokens` (also cache read), cache creation (`cache_creation_tokens|cache_write_tokens`), `output_tokens_details.reasoning_tokens`; also `response.service_tier` -> `ResponseServiceTier`. Image tool usage: `response.tool_usage.image_gen` published as an additional model usage (model from image tool `model` else `gpt-image-2`). Compact response uses `ParseOpenAIUsage` on top-level `usage`. `reporter.ObserveCodexResponseModel` records the served model. Failures call `reporter.PublishFailure`. Responses format output gets `EnsureResponsesUsageDetails` (adds missing `*_tokens_details`).

## 10. Images (`codex_openai_images.go`)
Detected when source format is `openai-image` and request path ends `/v1/images/generations` or `/v1/images/edits`.
- Direct models (`gpt-image-1.5`, `gpt-image-2`, `gpt-image-2.5`, `gpt-image-2.5-flare`, `gpt-image-2.5-sunburst`; base name after `/` and suffix strip, lowercase): POST `{base}/images/generations|edits` with the client JSON (multipart edits rewritten to JSON with data URLs, form fields mapped to JSON paths); direct image headers; response body passed through, usage via `ParseOpenAIUsage`; errors via `newCodexStatusErrWithCooling`. (Stream variant forwards upstream SSE.)
- Hosted path (other image model ids): builds a Responses request on main model `cfg.gpt-image-2-base-model` (must start with `gpt-`, default `gpt-5.4-mini`) with `tool_choice:{"type":"image_generation"}` and tool `{"type":"image_generation","action":"generate|edit","model":<requested or gpt-image-2>, size, quality, background, output_format, moderation, input_fidelity (edit), output_compression, partial_images, input_image_mask.image_url}`; input = one user message with `input_text` prompt + `input_image` data URLs (multipart edits parsed with 32 MiB form limit). Sent to `{base}/responses` as SSE (standard Codex headers). Collects `response.output_item.done` `image_generation_call` items (`result` base64, `revised_prompt`, `output_format`, `size`, `background`, `quality`); non-stream returns `{created, data:[{b64_json|url(data URI), revised_prompt}], background, output_format, quality, size, usage}` (`response_format=url` yields `data:` URIs); none -> 502 "upstream did not return image output"; no completion -> 504 "stream error: stream disconnected before completion". Stream mode emits SSE `image_generation.partial_image` / `image_edit.partial_image` (`partial_image_b64`, index) and `.completed` frames.

## 11. Client-facing `/v1/responses` WebSocket handler (`sdk/api/handlers/openai/openai_responses_websocket*.go`)

Routes: `GET /v1/responses` and `GET /backend-api/codex/responses` (behind access-manager auth). Method `OpenAIResponsesAPIHandler.ResponsesWebsocket`.

### 11.1 Connection
- gorilla `Upgrader` (4096 read/write buffers, `CheckOrigin` always true). Upgrade response headers: echoes `x-codex-turn-state` if the client sent it (sticky across reconnects).
- Per socket: `passthroughSessionID = uuid` (this is the executor execution-session id), `downstreamSessionKey` (tool-cache key: `X-Client-Request-Id`, else `X-Codex-Turn-Metadata.session_id`, else `Session-Id`, else `Session_id`), request timeline log (only if `request-log` enabled), a serialized `responsesWebsocketWriter` (write mutex, `closing` flag).
- Reads: `conn.ReadMessage()` (text or binary accepted; others ignored); close errors 1000/1001/1005 logged as normal. In duplex mode (`cfg.CodexResponseSteering`) a dedicated reader goroutine (`readResponsesWebsocketInput`, chan cap 16) is the only reader and cancels the socket context on read error.
- Keepalive: during forwarding, `StreamingKeepAliveInterval(cfg)` = `cfg.streaming.keepalive-seconds` (default 0 = disabled); when > 0, WS ping control frames are sent to the client, ticker reset on every data chunk.
- Upstream disconnect subscription: for providers `codex` and `xai` executors implementing `UpstreamDisconnectChan(sessionID)`, a goroutine waits and then `closeForUpstreamDisconnect(err)`: mirrors close codes 1009 (message too big) and 1012 (replay required) as close frames; otherwise, exposes an error event only for request-shape faults (`shouldExposeResponsesUpstreamError`) and otherwise just closes silently (client reconnects; reconnect implies full-context resend). Skipped when duplex stream owns closure.
- Teardown: release tool caches, flush timeline, `AuthManager.CloseExecutionSession(passthroughSessionID)` (closes upstream WS), close conn.

### 11.2 Client message protocol
Client -> proxy JSON text frames: `{"type":"response.create", model, input:[...], previous_response_id?, generate?, instructions?, tools?, ...}` and `{"type":"response.append", input:[...]}` (also `response.steer` in duplex). Proxy -> client: raw Responses events as JSON text frames (`response.created`, deltas, `response.output_item.*`, `response.completed`/`response.done`, `response.incomplete`, ...), plus `{"type":"error","status":N,"error":{...},"headers"?:{...}}` error frames. `[DONE]` markers and SSE `event:` lines from translators are stripped (`websocketJSONPayloadsFromChunk` extracts `data:` JSON per line).

### 11.3 Per-socket state
`lastRequest` (last normalized request body), `lastResponseOutput` (last completed `response.output` array), `lastResponseID`, `lastResponsePendingToolCallIDs`, `pendingPrewarmID`, `pinnedAuthID` + `pinnedAuthByProvider[provider]={authID, modelKey}`, `passthroughModelName`, `upstreamMode` ("" | "websocket" | "http"), `upstreamWebsocketAuthID`, `observedCompaction` (model/provider/auth/plugin that proved compaction replay works).

### 11.4 Ws vs http decision
- `upstreamModeForAuth(auth)`: "websocket" iff auth has `websockets` true (attributes/metadata) and provider in {codex, xai}; else "http".
- `useUpstreamWebsocketPassthrough` = all candidate auths for the model are the same provider (codex or xai) with websockets enabled and an executor is registered; overridden by the pinned auth if present.
- `nativeWebsocketPassthrough` = no route override AND `upstreamMode=="websocket"` AND passthrough eligible AND `pinnedAuthID == upstreamWebsocketAuthID` (the same credential carried the previous turn). In this mode the handler does NOT keep transcript state: it validates JSON, requires `type` in {create, append}, fills missing `model` from `passthroughModelName`, sets `stream:true`, keeps `type` and `previous_response_id`, and the executor sends it over the pinned upstream socket.
- Otherwise ("fallback" mode, HTTP upstream or any non-WS provider): the handler emulates websocket continuation by merging transcripts (11.5) and executes through the normal streaming path (`ExecuteStreamWithAuthManager`, handler type OpenAI-Response). The context carries `WithDownstreamWebsocket`, `WithExecutionSessionID(passthroughSessionID)`, selected-auth callback (records `lastAttemptedAuthID`, upstream mode of the selected auth, sets `preserveNativeOutput` for native Codex clients on codex auths, `codexDuplexStream`), optional `WithPinnedAuthID` (route stickiness to pinned auth unless a model route override applies), `WithRequiredUpstreamWebsocket` (native passthrough + continuation request), duplex input channel.
- If `upstreamMode==websocket` but passthrough is no longer allowed: a continuation request (has `previous_response_id` or `type==response.append`) closes the socket with 1012 "upstream requires HTTP replay"; a full `response.create` is accepted and starts a new transport.
- Failures on a pinned auth (401/429) during a continuation with required-current-upstream are suppressed and turned into the 1012 replay close. Pinned auth is released (`forgetPinnedAuth`) on 401, 402, 403, 429, 408, 502, 503, 504 or messages containing `stream closed before response.completed`, `previous_response_not_found`, `ws_failed`, `upstream stream closed before first payload`, `empty_stream`.
- After a successful turn: if attempted mode is websocket -> remember pin (`rememberPinnedAuth`), clear local transcript state (`lastRequest=nil`, `lastResponseOutput=[]`...), `passthroughModelName=model`; if http -> store `lastRequest=normalized request`, `lastResponseOutput=completed output`, `lastResponseID`, pending tool call ids, and update `observedCompaction` when completed output contains `compaction`/`compaction_summary` items.

### 11.4a Prewarm
`generate:false` on a `response.create` (when not in upstream-WS passthrough) is handled locally (`shouldHandleResponsesWebsocketPrewarmLocally`): nothing is sent upstream; proxy synthesizes `response.created` (seq 0, status in_progress, id `resp_prewarm_{uuid}`, `created_at`, `model`) and `response.completed` (seq 1, empty output, zero usage) and stores `pendingPrewarmID`, `lastRequest=normalized warmup request` (without `generate`), `lastResponseOutput=[]`. The next request: if `previous_response_id == pendingPrewarmID` -> `normalizeResponsesWebsocketPrewarmFollowup` merges warmup input + new input and drops the id; if it differs -> 409 `previous_response_not_found`; a `response.create` without parent id replaces transcript. `pendingPrewarmID` clears after a generating request commits.

### 11.5 Request normalization in fallback mode (`openai_responses_websocket_requests.go`)
- `response.create` with no prior request: delete `type`, force `stream:true`, default `input:[]`, require `model` (400 "missing model in response.create request"), `input` must be an array (400 "websocket request requires array field: input").
- Subsequent `create`/`append` (append before any create -> 400 "websocket request received before response.create"):
  - Transcript replacement (input is a self-contained history; no `previous_response_id`): detected when input contains `function_call`/`custom_tool_call` items or assistant `message`s, or (create without parent) a Codex local compaction summary (user message starting with the "Another language model started to solve this problem..." prefix + newline, only user/developer messages, optional leading `additional_tools` developer item). Result: drop `type`/`previous_response_id`, inherit `model`/`instructions` from `lastRequest`, `stream:true`, input as sent.
  - Incremental-with-previous_response_id (only when `allowIncrementalInputWithPreviousResponseID`; currently the handler passes `false`, i.e. it always expands): would keep `previous_response_id` and delta input.
  - Default merge: `input = lastRequest.input ++ lastResponseOutput ++ newInput`, then `dedupeFunctionCalls` (drop repeated tool-call `call_id`) and `dedupeByID` (same `id` -> keep last, but prefer item whose `call_id` has a matching output); compaction items: when `allowCompactionReplayBypass` and input `inputContainsFullTranscript` (has `compaction`/`compaction_summary`) the new input is used verbatim (skips merge), else compaction items are stripped from the appended input. `previous_response_id` removed; `model`/`instructions` inherited; `stream:true`. Without lastRequest but with `previous_response_id` -> 409 `previous_response_not_found` ("Previous response is not available on this websocket; resend the full conversation input without previous_response_id").
  - `allowCompactionReplayBypass` true when provider is codex (all candidate auths, or pinned auth) or observed compaction proved it for the same model/route/auth.
- Then `PrepareCodexMultiAgentV2Tools` and optional orphan-delegation rewrite (config gated) and, in fallback mode, tool-call repair.

### 11.6 Tool-call repair (`..._toolcall_repair.go`)
Purpose: upstream (Responses) rejects transcripts with unmatched `function_call`/`function_call_output`. Per downstream session key, two LRU-ish caches (max 256 entries/session, TTL disabled by default): output cache (`call_id` -> `function_call_output`/`custom_tool_call_output` item) and call cache (`call_id` -> call item). Refcounted per socket key (`retain/release` on connect/disconnect; cleared on last release). For each fallback request (`prepareResponsesWebsocketFallbackTurn`, read-locked, recording deferred to a turn object committed only after a successful response):
- Passes: record outputs/calls seen in input. Then filter: output with empty `call_id` kept only if `function_call_output` with a non-empty `name` (Codex heartbeat/delegation results); output whose call is in the payload kept; output with missing call: if `previous_response_id` present keep (orphans allowed), else reinsert cached call before it, else drop. Call items with empty `call_id` dropped; call with no output in payload: keep if orphans allowed, else append cached output right after it, else drop. Finally `dedupeResponsesWebsocketInputItems`.
- Responses are scanned (`response.completed`, `response.output_item.added/done`) to record complete tool calls (type `function_call` with string `arguments`, `custom_tool_call` with string `input`, non-empty `call_id` and `name`). Completed output is reconciled so tool call items match the fully collected `output_item.done` versions (fixes truncated/empty `response.output`), and `response.output` is rebuilt from collected items when empty (unless `preserveNativeOutput`).
- Handler tracks `pendingToolCallIDs` (calls without outputs) after each completed turn; used by incremental normalization to decide whether input satisfies pending calls.

### 11.7 Forwarding and termination (`openai_responses_websocket_forward.go`)
- `forwardResponsesWebsocket` selects over: request ctx done (cancel upstream, return), keepalive ticker (WS ping), `errs` channel, `data` channel.
- Per data chunk: split into JSON payloads; `response.created` resets per-turn collectors; collects `output_item.done` items; for completion events (`response.completed`/`response.done`) restores/reconciles output (skipped when `preserveCompletionOutput()`); records tool calls; tracks pending call ids; `error` payloads (except preserved ones in duplex after `response.created`) become `ErrorMessage` (status from `status`/`status_code`, default 500) and terminate; completion sets `completedOutput`/`completedResponseID`. Each payload is written as a text frame and appended to the timeline.
- Stream closes without completion (non-duplex) -> 408 `stream closed before response.completed`, socket closed silently (no error frame).
- Terminal error policy (`shouldExposeResponsesUpstreamError`): error frame is shown to the client only for terminal-auth errors or request faults (`clienterror.IsRequestFault`: client-fixable 4xx); credential/quota/transport errors close the socket without a frame (client reconnects, reconnect implies full resend). Frame shape: `{"type":"error","status":N,"headers"?:{...first value per header},"error":{...}}` built via `handlers.BuildErrorResponseBodyWithError`. Special close frames: replay-required -> 1012 reason `upstream requires HTTP replay`; message-too-big (413 with `error.code==message_too_big`, or close 1009) -> close 1009 with reason (truncated to 123 bytes UTF-8 safe).
- In-handler non-stream errors (normalize failures) are written as error frames and the socket stays open for the next message.

### 11.8 Timeline logging (`..._timeline.go`)
Request-log feature: when `cfg.request-log` is on, each request and response frame is appended as `Timestamp: {RFC3339Nano}\nEvent: websocket.{request|response|disconnect}\n{payload}\n` either to a per-request file part source (`WebsocketTimelineSourceContextKey`) or an in-memory builder stored on the gin context key `WEBSOCKET_TIMELINE_OVERRIDE`. Disconnect reason appended on termination. Also sets `API_RESPONSE_TIMESTAMP` on first response write. Purely observability; port can defer.

## 12. Port notes and risks
- Heaviest fidelity items: Chrome TLS fingerprint to chatgpt.com; exact header casing on WS handshake (`session_id`, `ChatGPT-Account-ID`, `OpenAI-Beta: responses_websockets=2026-02-06`); the pinned UA string and `codex-tui` originator cloaking; 5 min WS idle read timeout; one-conn-per-client-session pooling with target-triple check; replay-required (1012) protocol with clients.
- JSON field-level mutations use gjson/sjson on raw bytes preserving key order and unknown fields; Rust should use `serde_json` with `preserve_order` + `RawValue` or a raw-path patcher to avoid reordering and number-format changes.
- Status classification is string-matching heavy (lowercased body contains ...); port 1:1 using the lists in sec. 7.
- State to persist across requests in-process: WS session store, reasoning replay cache (backed by `internal/cache`, may be Redis/Home-backed with "Required" error semantics), websocket tool caches, refresh single-flight map.
- Config keys touched: `codex.{disable-codex-cloaking, stream-bootstrap-buffering, stream-bootstrap-timeout, orphan-delegation-compatibility, model-level-cooling, response-steering, live-media-relay}`, `codex-header-defaults.{user-agent,beta-features}`, `codex-api-key[]`, `client.codex.optimize-multi-agent-v2`, `disable-image-generation`, `gpt-image-2-base-model`, `streaming.keepalive-seconds`, `request-log`, `proxy-url`.
# Part D. Google family (gemini, vertex, aistudio + wsrelay, antigravity)

Source root: `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI` (Go, module `.../CLIProxyAPI/v8`). All paths below are relative to it.
Non-test line counts are in section 0. Shared helpers referenced here (translator registry, thinking applier, payload config, usage reporter, conductor retry/cooldown) belong to other spec sections; only their Google-specific hooks are described.

## 0. Non-test line counts

- gemini (API key + interactions): `internal/runtime/executor/gemini_executor.go` 1093; `helps/gemini_content_turns.go` 91; `internal/signature/gemini_sanitize.go` 345, `gemini_validation.go` 610; `internal/thinking/provider/gemini/apply.go` 182. Total about 2.3k.
- vertex: `gemini_vertex_executor.go` 1225; `internal/auth/vertex/vertex_credentials.go` 84 + `keyutil.go` 208; `internal/cmd/vertex_import.go` ~130; `helps/vertex_payload_helpers.go` 86. Total about 1.7k.
- aistudio: `aistudio_executor.go` 642; `internal/wsrelay/` message.go 27, manager.go 205, session.go 394, http.go 248 (874); `sdk/cliproxy/service_auth.go` ws hooks (~60). Total about 1.6k.
- antigravity executor: `antigravity_executor.go` 976, `_auth.go` 306, `_credits.go` 789, `_execute.go` 667, `_request.go` 542, `_stream.go` 342, `_tokens.go` 153, `antigravity_reasoning_replay.go` 2661 (sum 6436); `helps/antigravity_compaction.go` 383, `antigravity_grounding_urls.go` 104; `internal/auth/antigravity/` auth.go 406 + constants.go 32 + filename.go 16; `sdk/auth/antigravity.go` 378; `internal/misc/antigravity_version.go` 270; `sdk/cliproxy/antigravity_models.go` 489; `internal/thinking/provider/antigravity/apply.go` 220; `internal/util/gemini_schema.go` 1711 (shared with gemini). Total about 11k, excluding translators (`internal/translator/antigravity/*` ~4.7k, `internal/translator/gemini/*` ~3.7k, documented elsewhere).
- gemini-cli OAuth: NOT PRESENT in this version. There is no gemini-cli login, no cloudcode-pa executor for provider `gemini`, no `internal/auth/gemini`. `sdk/auth/filestore.go:248` explicitly skips any auth file whose `type` is `gemini` (returns nil, nil) so legacy gemini-cli files are ignored. Do not port gemini-cli. Provider `gemini` means API-key only.

## 1. Provider ids, executors, routing

- Executor registration: `sdk/cliproxy/service_executors.go` lines ~190-300. Identifiers: `gemini` (GeminiExecutor), `gemini-interactions` (same struct, identifier swapped, constant `constant.GeminiInteractions`), `vertex` (GeminiVertexExecutor), `aistudio` (AIStudioExecutor, one executor registered with wsGateway), `antigravity` (AntigravityExecutor).
- Default upstream request format per provider: `sdk/cliproxy/auth/conductor_execution.go:423-430`: gemini/vertex/aistudio -> `FormatGemini`; antigravity -> `FormatAntigravity`; gemini-interactions executor uses `FormatInteractions` when the source format is one of interactions/openai/openai-response/claude/gemini (`RequestToFormat`, gemini_executor.go:76).
- All executors implement: Identifier, PrepareRequest, HttpRequest (raw passthrough with creds injected), Execute, ExecuteStream, CountTokens, Refresh, SupportsApplyPatch. Refresh for gemini/vertex/aistudio is a no-op unless `helps.RefreshAuthViaHome` (Home server mode) handles it.
- `/responses/compact`: gemini/vertex/aistudio return 501 `/responses/compact not supported`. Antigravity implements compaction itself (section 6.9).
- Thinking suffix: every executor does `thinking.ParseSuffix(req.Model).ModelName` to get `baseModel` (strip `(budget)` style suffix); the suffix drives `helps.ApplyRequestThinking` (applier registered per provider: `gemini`, `antigravity`, `interactions`).

## 2. Gemini API key executor (`gemini_executor.go`)

### 2.1 Endpoints
- Base: `https://generativelanguage.googleapis.com`, version `v1beta`. Override per credential: `auth.Attributes["base_url"]` (trailing `/` trimmed).
- Generate: `POST {base}/v1beta/models/{baseModel}:generateContent` (+ `?$alt={opts.Alt}` when Alt non-empty).
- Stream: `POST .../models/{m}:streamGenerateContent?alt=sse` (or `?$alt={Alt}` if Alt set).
- Count tokens: `POST .../models/{m}:countTokens`; response `totalTokens` read via gjson. Triggered by `req.Metadata["action"]=="countTokens"` (inside Execute) or the CountTokens method.
- Interactions (provider `gemini-interactions` only): `POST {base}/v1beta/interactions`, header `Api-Revision: 2026-05-20` default (client `Api-Revision` header passed through if present).
- Streams use `bufio.Scanner` with buffer cap 52,428,800 bytes (`streamScannerBuffer`).

### 2.2 Headers
- `Content-Type: application/json`, `x-goog-api-key: <attributes.api_key>`; `Authorization` deleted. No User-Agent or x-goog-api-client is set by the executor (Go default UA). Custom headers: `util.ApplyCustomHeadersFromAttrs(req, attrs, clientHeaders)` applies `header:`-prefixed attributes (config `headers:` map) and optionally client passthrough headers.
- Credentials are config-only: `gemini-api-key:` list of `GeminiKey{api-key, priority, weight, prefix, base-url, proxy-url, models[], headers, excluded-models, disable-cooling, request-retry, request-scoped-errors}` (`internal/config/config_types.go:748`); also `interactions-api-key:` list of same type. Synthesized into Auth with `Attributes{api_key, base_url, ...}`. No auth file, no OAuth.

### 2.3 Request pipeline (non-stream; stream is identical except noted)
1. `helps.EnsureSessionContext`.
2. Translate `opts.SourceFormat` -> `gemini` for both `opts.OriginalRequest` (as "original", used by payload rules) and `req.Payload`: `helps.TranslateRequestPairWithAPIKeyModelCompatibility(..., isCompat)` where `isCompat = helps.APIKeyModelIsCompat(req)` (per-key `models[]` entries flagged compat).
3. `helps.ApplyRequestThinking(body, req, opts, from, "gemini", identifier)` -> sets `generationConfig.thinkingConfig.{thinkingBudget|thinkingLevel,includeThoughts}` (see 2.6).
4. `fixGeminiImageAspectRatio` (only model `gemini-2.5-flash-image-preview`): if `generationConfig.imageConfig.aspectRatio` exists and no `inlineData` part in contents, prepend a white PNG (`util.CreateWhiteImageBase64(aspect)`) plus an instruction text part to `contents.0.parts`, set `generationConfig.responseModalities=["IMAGE","TEXT"]`, delete `imageConfig`.
5. `helps.ApplyPayloadConfigWithRequest(cfg, baseModel, "gemini", fromFormat, root="", body, originalTranslated, requestedModel, requestPath, headers)`: config `payload:` default/override rules.
6. `SetStringIfDifferent(body,"model",baseModel)`; `capGeminiMaxOutputTokens`: if `generationConfig.maxOutputTokens` numeric and registry `LookupModelInfo(model,"gemini")` has `OutputTokenLimit` (or `MaxCompletionTokens`) lower, clamp to it.
7. `internalsignature.SanitizeGeminiRequestThoughtSignatures(body,"contents")`: keep native signatures; missing/incompatible first functionCall gets sentinel `GeminiSkipThoughtSignatureValidator`; functionResponse never carries signatures; toolCall/toolResponse untouched.
8. Turn shape: non-stream: `EnsureGeminiLeadingUserContent` (prepend `{"role":"user","parts":[{"text":""}]}` if first role is `model`) and, unless countTokens, `EnsureGeminiTrailingUserContent` (append the same empty user turn if last role is model/assistant and last content has no functionResponse). Stream uses `EnsureGeminiBoundaryUserContent` (both). countTokens additionally deletes `tools`, `generationConfig`, `safetySettings`.
9. Delete `session_id` from body. Send.

### 2.4 Response handling
- Non-2xx: read body, return `statusErr{code, msg: body}` (no retry inside executor; see 8).
- Non-stream 2xx: `TranslateNonStream(gemini -> responseFormat)`; usage = `helps.ParseGeminiUsage(data)` (reads `usageMetadata` or `usage_metadata`; fields promptTokenCount, candidatesTokenCount, thoughtsTokenCount, cachedContentTokenCount, totalTokenCount via `parseGeminiFamilyUsageDetail`). If translated output empty -> 502 (apply_patch error message).
- Stream: SSE lines. Per line: `FilterSSEUsageMetadata` (renames `usageMetadata` to `cpaUsageMetadata` on non-terminal chunks, i.e. chunks whose `candidates.0.finishReason` is absent, so downstream clients see usage only at the end), `JSONPayload` (strips `data:`, drops `event:` and `[DONE]`, requires leading `{`), then `ParseGeminiStreamUsage` buffered in `StreamUsageBuffer`; then `TranslateStreamWithClaudeInputTokens`. After the scanner ends, a synthetic `[DONE]` is fed to the translator. No keepalive handling in this executor (upstream SSE only). A scanner error is emitted as `StreamChunk{Err}`.
- Interactions stream: SSE frames split by blank lines; payload is JSON after `data:`; `event: done` or `data: [DONE]` is the terminator; if response format is interactions the raw frame is forwarded verbatim (+`\n\n`), else translated. Usage via `ParseInteractionsStreamUsage`. Before sending: `sanitizeGeminiInteractionsUnsupportedInputIDs` (function_call: move `call_id` to `id`; delete `id` on other step types and on content parts) and stream sets `stream=true`.

### 2.5 CountTokens specifics
Translate source -> gemini, apply thinking, strip tools/generationConfig/safetySettings, set model, sanitize signatures, ensure leading user content, POST `:countTokens`, translate via `TranslateTokenCount(ctx, gemini, responseFormat, totalTokens, raw)`.

### 2.6 Thinking applier (`internal/thinking/provider/gemini/apply.go`)
Path `generationConfig.thinkingConfig.*`. Modes: Level -> `thinkingLevel` (deletes `thinkingBudget`, snake_case variants, `includeThoughts` then re-adds `includeThoughts` only if the original had true/false); Budget/Auto -> `thinkingBudget` (Auto = -1); None with model levels -> level format, none with `budget==0 && level==""` -> delete `thinkingConfig`. User-defined (compat) models use a looser path. No-op if `modelInfo.Thinking == nil`.

## 3. Vertex executor (`gemini_vertex_executor.go`)

### 3.1 Auth modes
- API-key mode (config `vertex-api-key:` list, `VertexCompatKey{api-key, base-url, proxy-url, prefix, models, headers, ...}`): `vertexAPICreds(auth)` returns `attributes.api_key` (fallback `metadata.access_token`) and `attributes.base_url`. Header `x-goog-api-key`.
- Service-account mode (auth file `type: vertex`): `vertexCreds(auth)` reads metadata `project_id` (fallback `project`; required), `location` (default `us-central1`), `service_account` (object, required). The map is passed through `vertexauth.NormalizeServiceAccountMap` then JSON-marshalled and given to `golang.org/x/oauth2/google.CredentialsFromJSON(ctx, saJSON, "https://www.googleapis.com/auth/cloud-platform")`; the token is fetched with `creds.TokenSource.Token()` on EVERY request (oauth2 library caches internally per CredentialsFromJSON call, but a new creds object is built per request, so effectively a JWT-bearer exchange per request). Rust port needs: RS256 JWT with claims `iss=client_email, scope=cloud-platform, aud=token_uri (https://oauth2.googleapis.com/token), iat, exp (+3600)`, POST `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`, cache token until ~expiry minus skew. Token fetch uses credential/global proxy only, not the per-request execution proxy (`WithoutRequestProxyURL`).
- Selection: `apiKey == ""` -> service account path, else API-key path. Token errors are logged and returned as `statusErr{500,"internal server error"}`.

### 3.2 Endpoints
- Service account: `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:{action}`; `location=="global"` uses host `https://aiplatform.googleapis.com`. `vertexAPIVersion="v1"`.
- API key: `{base_url or https://aiplatform.googleapis.com}/v1/publishers/google/models/{model}:{action}` (no project/location).
- Actions: `generateContent`, `streamGenerateContent`, `countTokens`; Imagen models (name contains `imagen`, case-insens) use `predict` and no streaming query. Query: stream adds `?alt=sse` (or `?$alt=`), non-stream adds `?$alt={Alt}` if set (not for countTokens).
- Headers: `Content-Type: application/json`, `Authorization: Bearer <token>` or `x-goog-api-key`, plus custom headers. No UA/x-goog-api-client.

### 3.3 Request pipeline
Same as Gemini API key steps 2-9, except: translation uses `helps.TranslateRequestWithCodexMultiAgentV2`; extra `helps.StripVertexOpenAIResponsesToolCallIDs(body, from)` (when source is `openai-response`, removes `functionCall.id`/`functionResponse.id` that Vertex rejects); no maxOutputTokens cap; no `SetTranslatedReasoning` difference. Imagen path: `convertToImagenRequest` builds `{"instances":[{"prompt":..,"negativePrompt"?}],"parameters":{"sampleCount":1,"aspectRatio"?}}` where prompt comes from `contents.0.parts.0.text`, else first `messages.#.content`, else `prompt`; response `predictions[].{bytesBase64Encoded,mimeType}` is rewritten by `convertImagenToGeminiResponse` into a Gemini candidate with `inlineData` parts, `finishReason STOP`, `responseId imagen-<unixnano>`, zero usage.
- Streaming parsing: line-based scanner (same 50MB cap), each raw line handed to `ParseGeminiStreamUsage` and the translator (no `FilterSSEUsageMetadata` here), trailing `[DONE]` synthesized.
- Errors: non-2xx -> `statusErr{code, body}`. Usage: `ParseGeminiUsage`.

### 3.4 Credential file schema (`internal/auth/vertex/vertex_credentials.go`)
```json
{
  "service_account": { ...verbatim Google SA JSON: type, project_id, private_key_id, private_key, client_email, client_id, auth_uri, token_uri, ... },
  "project_id": "my-proj",
  "email": "sa@my-proj.iam.gserviceaccount.com",
  "location": "us-central1",
  "type": "vertex",
  "prefix": "teamA",
  "label": "my-proj (sa@...)"
}
```
- `location` and `prefix` are omitempty in the struct but import writes both. Filename: `vertex-{sanitize(projectID)}.json` or `vertex-{sanitize(prefix)}-{sanitize(projectID)}.json`; sanitize maps `/`->`_`, `\`->`_`, `:`->`_`, space->`-`. File mode: dir 0700; written pretty-printed (2 spaces). Extra hook metadata is merged via `misc.MergeMetadata`.
- Import CLI: flags `-vertex-import <key.json>` and `-vertex-import-prefix <seg>` (`cmd/server/main.go:154`, `internal/cmd/vertex_import.go`); prefix must be a single segment (no `/`); requires `project_id`; warns if `client_email` empty; location defaults to `us-central1` (edit file to change). Management API upload exists at `internal/api/handlers/management/vertex_import.go`.
- Private key normalization (`keyutil.go`): strip ANSI escapes, normalize CRLF, rebuild PEM from base64 body if malformed (`rebuildPEM`, `filterBase64`), accept PKCS#1 or PKCS#8, always re-emit as `RSA PRIVATE KEY` (PKCS#1) PEM. Missing `private_key` is an error. Port: accept both PEM types in the JWT signer instead of re-encoding.

## 4. AI Studio executor and websocket relay

### 4.1 Concept
Provider `aistudio` has no upstream credentials in the proxy. A browser/userscript-driven AI Studio page connects to the proxy over WebSocket and acts as an HTTP client: the proxy sends it `http_request` envelopes for `https://generativelanguage.googleapis.com/v1beta/models/...` and the page performs the fetch with its own logged-in Google session, relaying the result back. Each connected socket is a runtime-only auth.

### 4.2 Relay: `internal/wsrelay`
- `Manager` (manager.go): mounted by `sdk/cliproxy/service_lifecycle.go:141` via `server.AttachWebsocketRoute(path="/v1/ws", handler)` (`internal/api/server_routes.go:543`). Route is wrapped by the normal `AuthMiddleware` only when `ws-auth` is true (config `ws-auth`, default true, hot-togglable; toggling to enabled terminates all existing sessions via `Manager.Stop`). Upgrade: gorilla/websocket, read/write buffers 1024, `CheckOrigin` always true. Rejects non-GET with 405, wrong path with 404.
- Session identity: `ProviderFactory` option is nil in the service, so each connection gets `randomProviderName()` = `"aistudio-" + 16 random chars [a-z0-9]`; stored lowercased in `sessions map[string]*session`. Reconnecting does not reuse names. If a name collides, the old session is cleaned up with cause `replaced by new connection`.
- Lifecycle callbacks (`sdk/cliproxy/service_auth.go:279-340`): `OnConnected(channelID)`: only for ids prefixed `aistudio-`; if an active enabled auth with that ID already exists, nothing; else emit `AuthUpdate{Add}` with `Auth{ID: channelID, Provider:"aistudio", Label: channelID, Status: Active, Attributes{"runtime_only":"true"}, Metadata{"email": channelID}}`. `OnDisconnected(channelID, cause)`: if cause contains `replaced by new connection` do nothing; else emit `AuthUpdate{Delete, ID}`. So the credential pool contains one auth per live socket; models come from the registry `aistudio` list (`internal/registry/model_definitions.go:494`).
- `Manager.Send(ctx, provider, msg)`: lookup session by lowercase provider (== auth.ID); error `wsrelay: provider X not connected`.
- Message schema (message.go), JSON text frames:
```json
{"id": "<uuid string>", "type": "<type>", "payload": { ... }}
```
  Types: `http_request`, `http_response`, `stream_start`, `stream_chunk`, `stream_end`, `error`, `ping`, `pong`.
- Server -> client `http_request` payload (http.go `encodeRequest`): `{"method":"POST","url":"https://generativelanguage.googleapis.com/v1beta/models/{m}:{action}[?alt=sse|?$alt=..]","headers":{"Content-Type":["application/json"], ...} (map of string -> []string),"body":"<request body as a string>","sent_at":"<RFC3339Nano UTC>"}`. `id` = fresh `uuid.NewString()` per request.
- Client -> server responses, correlated by `id`:
  - `http_response` payload `{"status": <int>, "headers": {k: [..] | k: "v"}, "body": "<string>"}` (terminal). Missing payload -> status 502; missing status defaults 200.
  - `stream_start` payload `{"status":int,"headers":{...}}` (non-terminal; defaults status 200).
  - `stream_chunk` payload `{"data":"<string>"}` (non-terminal). Data is raw SSE text, possibly multiple lines per chunk.
  - `stream_end` (terminal, payload ignored).
  - `error` payload `{"error":"<msg>","status":<int>}` (terminal). Decoded as error string `"<msg> (status=N)"`; default message `wsrelay: upstream error`.
  - `ping` from client: server replies `{"id":same,"type":"pong"}` immediately (no pending lookup).
- Session mechanics (session.go): constants `readTimeout=60s`, `writeTimeout=10s`, `maxInboundMessageLen=64MiB`, `heartbeatInterval=30s`, `pendingChannelBuffer=64`. WebSocket-level ping (control frame, payload "ping") every 30s; pong handler extends the 60s read deadline; the read deadline is set at connect and refreshed by pongs (note: application-level `ping` JSON messages do NOT refresh it; only WS pongs do). Single writer guarded by `writeMutex`; each write sets a 10s deadline and calls `MarkUpstreamAttempt(ctx)` (usage/retry bookkeeping hook).
- `pending sync.Map[id -> pendingRequest]`; `request()` registers the id (duplicate id -> error), registers `context.AfterFunc(ctx, cancel)`, writes the message, returns a channel (cap 64). Dispatch: non-terminal messages are delivered with back-pressure (blocks the read loop when the 64 buffer is full, aborting on ctx done/session closed; an `onBlocked` hook exists but is unused); terminal messages (`http_response`, `error`, `stream_end`) are delivered then the pending entry is removed and the channel closed. If a terminal cannot be delivered (ctx cancelled / session closed), the channel gets a synthesized `error` message with the cause. Unknown-id terminals are debug logged. On session cleanup every pending request is failed with the cleanup cause (`websocket session closed` default, `wsrelay: manager stopped` on Stop, or the read error).
- Client helpers (http.go):
  - `NonStream(ctx, provider, *HTTPRequest) -> *HTTPResponse`: waits for `http_response` -> returns it; or aggregates `stream_start/stream_chunk/stream_end` into one body (status/headers from `stream_start`, default 200); `error` -> error; channel close without terminal -> `wsrelay: connection closed during response`.
  - `Stream(ctx, provider, req) -> chan StreamEvent{Type, Payload, Status, Headers, Err}`: forwards `stream_start` (status+headers), `stream_chunk` (data bytes), `stream_end` (then closes), `error` (Err), `http_response` (status/headers/body as Payload, then closes); channel close unexpectedly -> `Err: wsrelay: stream closed`.

### 4.3 Executor behavior (`aistudio_executor.go`)
- Request build (`translateRequest`): translate source -> gemini via `TranslateRequestWithCodexMultiAgentV2`; `ApplyThinkingWithSourcePayload`; `fixGeminiImageAspectRatio`; payload config; then DELETE `generationConfig.maxOutputTokens`, `generationConfig.responseMimeType`, `generationConfig.responseJsonSchema`; delete `session_id`; leading/trailing user-content fixes (trailing skipped for countTokens); `normalizeAIStudioThinkingLevel` uppercases `generationConfig.thinkingConfig.thinkingLevel` if it is one of minimal/low/medium/high (AI Studio rejects lowercase with 400).
- Endpoint (`buildEndpoint`): `https://generativelanguage.googleapis.com/v1beta/models/{m}:{generateContent|streamGenerateContent|countTokens}`; stream adds `?alt=sse` or `?$alt={url.QueryEscape(alt)}`; non-stream adds `?$alt=` if Alt set (not countTokens). Headers sent in the envelope: only `Content-Type: application/json` plus custom attribute headers and client passthrough headers. Auth ID (== channel id) selects the socket.
- Non-stream: `relay.NonStream`; status check; `TranslateNonStream`; usage `ParseGeminiUsage`; output passed through `ensureColonSpacedJSON` (re-marshals JSON indented then collapses whitespace so every `":"` becomes `": "` with otherwise compact output; a fingerprinting nicety, not required for correctness).
- Stream: `relay.Stream`; first event is read synchronously. If `firstEvent.Status != 200` (and >0), the rest of the stream is drained into an error body and returned as `statusErr{status, body}`; otherwise a goroutine handles events: `stream_start` records headers; `stream_chunk` -> `FilterSSEUsageMetadata` -> `ParseGeminiStreamUsage` -> translate -> emit each line through `ensureColonSpacedJSON`; `http_response` is translated as a single final payload; `stream_end` finalizes; error events surface as `wsrelay: <err>`. Stream context is cancelled on exit (cancels pending request).
- CountTokens: same translate with metadata action `countTokens`, strips generationConfig/tools/safetySettings; requires `totalTokens > 0` else error `wsrelay: totalTokens missing in response`.
- `HttpRequest`: forwards arbitrary requests (e.g. management api-call tool) through `relay.NonStream`, rebuilding an `http.Response`.
- No token refresh, no cooldown logic beyond generic conductor status handling.

## 5. Antigravity OAuth (auth + login)

### 5.1 Constants (`internal/auth/antigravity/constants.go`; duplicated in executor)
- Client ID `1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com`, secret `<client secret: see upstream internal/auth/antigravity/constants.go>`, callback port `51121`.
- Scopes (space-joined): `https://www.googleapis.com/auth/cloud-platform`, `.../auth/userinfo.email`, `.../auth/userinfo.profile`, `.../auth/cclog`, `.../auth/experimentsandconfigs`.
- Endpoints: auth `https://accounts.google.com/o/oauth2/v2/auth`; token `https://oauth2.googleapis.com/token`; userinfo `https://www.googleapis.com/oauth2/v2/userinfo?alt=json`; API `https://cloudcode-pa.googleapis.com`, `https://daily-cloudcode-pa.googleapis.com`, version `v1internal`.

### 5.2 Login flow (`sdk/auth/antigravity.go`, `internal/auth/antigravity/auth.go`)
1. CLI flag `-antigravity-login` (`internal/cmd/antigravity_login.go` -> `sdkAuth` manager `Login("antigravity")`); optional `-no-browser`, `--oauth-callback-port` override.
2. Generate random `state` (`misc.GenerateRandomState`). NO PKCE (no code_challenge). Start loopback HTTP server on `:{port}` (default 51121; port 0/ephemeral picks actual port) path `/oauth-callback`; redirect URI `http://localhost:{port}/oauth-callback`.
3. Auth URL params: `access_type=offline`, `client_id`, `prompt=consent`, `redirect_uri`, `response_type=code`, `scope`, `state`.
4. Open browser (or print URL + SSH-tunnel hint). After 15s, if a prompt callback is available, ask the user to paste the callback URL (`misc.ParseOAuthCallback`). Overall timeout 5 minutes. Validates `state`, `error`, `code`.
5. Token exchange: `POST token` form `code, client_id, client_secret, redirect_uri, grant_type=authorization_code`; response `{access_token, refresh_token, expires_in, token_type}`.
6. Userinfo: `GET userinfo` `Authorization: Bearer`, `User-Agent: short UA`; require `email`.
7. Project discovery `FetchProjectID`: `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist` body `{"metadata":{"ideType":"ANTIGRAVITY"}}`, headers `Authorization: Bearer`, `Accept: */*`, `Content-Type: application/json`, `User-Agent: antigravity/hub/{ver} darwin/arm64`. Project id is first non-empty of `cloudaicompanionProject`, `projectId`, `project` (string, or object with `id`).
8. If absent: `OnboardUser`: tier = first `allowedTiers[]` entry with `isDefault:true` (`id`), else `currentTier.id`, else `free-tier`. `POST https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser` body `{"tier_id": tier, "metadata": {"ide_type":"ANTIGRAVITY","ide_version":"<version>","ide_name":"antigravity"}}`, headers: Bearer, `Accept: */*`, `Content-Type`, `User-Agent: antigravity/hub/{ver} darwin/arm64 google-api-nodejs-client/10.3.0`, `X-Goog-Api-Client: gl-node/22.21.1`. Poll up to 5 attempts, each with 30s timeout, sleeping 2s while `done != true`; on `done:true` read `response` with the same project-key extraction; non-200 -> `HTTPStatusError`. Failure to get a project id fails login.
9. `RefreshLead` = 30 minutes (conductor refreshes tokens 30 min before expiry).

### 5.3 Credential file (`BuildAntigravityAuth`)
Filename `antigravity-{email}.json` (or `antigravity.json` when no email). JSON metadata:
```json
{"type":"antigravity","access_token":"...","refresh_token":"...","expires_in":3599,
 "timestamp":1700000000000,"expired":"2025-01-01T00:00:00Z","email":"a@b.com","project_id":"proj-123"}
```
`timestamp` is ms epoch of issue; `expired` is RFC3339 of issue+expires_in. Optional extras read by executor: `user_agent`, `base_url`, `proxy_url` (auth-level), plus `attributes.base_url`, `attributes.base_urls` (comma list, model-hint probe only), `attributes.user_agent`. `Auth.ID == FileName`, `Label = email`. Expiry resolution (`Auth.ExpirationTime`): JWT `exp` of access token first, then keys `expired|expire|expires_at|expiresAt|expiry|expires`, then `expires_in + timestamp`.

### 5.4 Version / UA (`internal/misc/antigravity_version.go`)
- Fallback version `2.9.1` (Cloud Code rejects newer models for clients under 2.9.0). Background updater (`StartAntigravityVersionUpdater`, refresh every 3h, cache TTL 6h) fetches YAML manifest `https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml` with `User-Agent: electron-builder`, `Cache-Control: no-cache`, 10s timeout, 4KB cap; reads `version`; must match `N.N.N` digits.
- UA forms: runtime/short = `antigravity/hub/{version} darwin/arm64`; control-plane/long (onboardUser) = short + ` google-api-nodejs-client/10.3.0`. A configured `user_agent` (attribute or metadata) starting with `antigravity/` is honored (long suffix trimmed for short form). `X-Goog-Api-Client: gl-node/22.21.1` only on onboardUser.

## 6. Antigravity executor

### 6.1 Endpoints and host selection
- Base URL (`resolveAntigravityRequestBaseURL`): `auth.Attributes["base_url"]`, else `metadata.base_url`, else `https://daily-cloudcode-pa.googleapis.com`. There is NO fallback host list and NO cross-host retry in this version. `antigravitySandboxBaseURLDaily = https://daily-cloudcode-pa.sandbox.googleapis.com` is defined but unused. loadCodeAssist for credits uses custom base or `https://cloudcode-pa.googleapis.com` (prod). Model capability probe (`sdk/cliproxy/antigravity_models.go`) uses `attributes.base_urls` (comma list, probed in parallel, first success with web-search ids wins) else base_url else daily.
- Paths: `POST {base}/v1internal:generateContent`, `/v1internal:streamGenerateContent?alt=sse` (alt set -> `?$alt=`), `/v1internal:countTokens`. `httpReq.Host` set to base host. Model list/capability: `POST {base}/v1internal:fetchAvailableModels` body `{}`, 5s timeout; response `webSearchModelIds[]` marks models that can run native `googleSearch`; model catalog itself is static in the registry (`antigravity` list). Cache TTL 5 min success, 1 min failure/auth-failure; 401/403 -> auth error status.

### 6.2 Headers
`Content-Type: application/json`, `Authorization: Bearer <access_token>`, `User-Agent: antigravity/hub/{ver} darwin/arm64` (or configured). Nothing else (no x-goog-api-client on generate; no Connection header). Custom attribute headers applied. HTTP/1.1 only, no ALPN (fingerprint match): per-credential transport pool (`antigravityTransports`, cap 8192) keyed by credential id/path/refresh-token hash + proxy; default is short-lived mode (`MaxIdleConnsPerHost=-1`); pooling only if config `antigravity.connection-pool.enabled: true` (idle timeout default 30s, max 210s; max idle per host default 2, max 100). On any 429 the credential's idle connections are closed. Proxy precedence: request proxy URL, `auth.ProxyURL`, `cfg.ProxyURL`.

### 6.3 Request pipeline (Execute, ExecuteStream; non-stream for Claude/gemini-3-pro/image models is done by streaming and aggregating)
1. Compaction expansion/trigger (6.9).
2. Short-cooldown precheck (6.6). If in cooldown and not bypassed (credits fallback), return 429 with `retryAfter=remaining` so the conductor switches auth.
3. Routing: if model contains `claude`, `gemini-3-pro`, or `gemini-3.1-flash-image`, non-stream `Execute` calls `executeClaudeNonStream`, which issues a streaming request and merges SSE via `convertStreamToNonStream`. Other models do a real `generateContent`.
4. `validateAntigravityRequestSignatures` (only when source is claude): for non-Claude targets (gemini models) strip thinking blocks lacking valid Gemini signatures; for claude targets strip empty-signature thinking blocks and, when signature cache disabled and bypass strict mode on, strip invalid bypass signatures.
5. `ensureAccessToken` (6.4).
6. Translate source -> `antigravity` (`helps.TranslateRequestEnvelopePairWithCodexMultiAgentV2`, includes `ModelInfo`, `Stream` flag). The translated payload is the Gemini request wrapped in the envelope (see 6.5).
7. `ApplyRequestThinking` (applier `antigravity`: paths `request.generationConfig.thinkingConfig.*`; for Claude models budget must be < max tokens and thinkingConfig is dropped if budget < model Thinking.Min; Level mode sets `thinkingLevel`; Auto = budget -1; `includeThoughts` preserved from client).
8. `ApplyPayloadConfigWithRequest(..., "antigravity", from, root="request", ...)`.
9. `obfuscateSensitiveWords`: config `antigravity.sensitive-words` inserts zero-width characters into words inside `request.systemInstruction` (helps `BuildSensitiveWordMatcher`).
10. `sanitizeAntigravityGeminiRequestSignatures` (non-Claude models using replay cache): `SanitizeGeminiRequestThoughtSignatures(..., "request.contents")` plus `normalizeAntigravityGeminiFunctionResponseRoles`: repairs functionResponse `name` from matching functionCall `id` when name empty/`unknown`, orders response parts to match pending call order, and forces `role:"model"` on pure functionResponse contents.
11. Stream only: delete `request.stream`.
12. Credits: if ctx carries the credits flag and `quota-exceeded.antigravity-credits` is on, add top-level `"enabledCreditTypes":["GOOGLE_ONE_AI"]` and mark credits used (6.7).
13. Reasoning replay (6.8) for gemini-family models (`antigravityUsesReasoningReplayCache`: lowercase name contains gemini/flash/agent and not claude).
14. Turn shape: unless model contains `claude`, `EnsureGeminiBoundaryUserContent` on `request.contents`.
15. `buildRequest`: sets `project`, wraps, then:
    - maxOutputTokens capped to registry `MaxCompletionTokens` (`antigravity` models).
    - If request has `request.tools.0` or any response schema key: schema sanitization (6.5b); for models containing `claude`, set `request.toolConfig.functionCallingConfig.mode="VALIDATED"`; for non-Claude, DELETE `request.generationConfig.maxOutputTokens`. If no tools/schemas: same VALIDATED / delete-maxOutputTokens rule without sanitization.
    - `applyAntigravityNativeSignatureReplayIfNeeded(model, payload)` (final signature pass).
    - Request log capture if `request-log` on.

### 6.4 Token handling (`antigravity_executor_auth.go`)
- Metadata keys used: `access_token`, `refresh_token`, `expires_in`, `timestamp`, `expired`, `project_id`, `type`.
- `ensureAccessToken`: if token present and expiry (from `Auth.ExpirationTime`) is after now + 5 min (`antigravityRequestTokenSafetyWindow`), use it (and opportunistically refresh credits hint). Else refresh (Home mode: `RefreshAuthViaHome`).
- Refresh: `POST https://oauth2.googleapis.com/token`, form `client_id, client_secret, grant_type=refresh_token, refresh_token`; headers `Host: oauth2.googleapis.com`, `Content-Type: application/x-www-form-urlencoded`, `User-Agent: Go-http-client/2.0`. Single-flight keyed by refresh token (`singleflight.Group`), 30s timeout detached from request cancel. Non-2xx -> `statusErr{code, body}`; 429 parses retry delay (6.6). Success updates metadata `access_token`, `refresh_token` (if returned), `expires_in`, `timestamp=now ms`, `expired=RFC3339(now+expires_in)`, `type="antigravity"`; then ensures `project_id` (loadCodeAssist flow, errors only warned) and queues a credits balance refresh. Returned updated Auth is persisted by the conductor.
- `PrepareRequestAuth`/`ShouldPrepareRequestAuth`: when metadata `project_id` is empty, the conductor calls this before execution to refresh token and run project discovery; failure -> `statusErr` (400 default, or upstream status/retryAfter) with message `antigravity auth missing project_id: ...`.
- Missing refresh token -> 401 `missing refresh token`.

### 6.5 Request envelope (`geminiToAntigravity`)
Top-level JSON given to upstream:
```json
{
  "project": "<metadata.project_id>",
  "model": "<baseModel>",
  "userAgent": "antigravity",
  "requestType": "agent" | "image_gen" | (translator-provided e.g. "web_search"),
  "requestId": "agent-<uuid>" | "image_gen/<unix_ms>/<uuid>/12",
  "request": { "contents": [...], "systemInstruction": {...}, "generationConfig": {...}, "tools": [...], "toolConfig": {...}, "sessionId": "<id>" },
  "enabledCreditTypes": ["GOOGLE_ONE_AI"]   // only on credits retries
}
```
- `requestType` defaults to `image_gen` if model name contains `image`, else `agent`, unless the translator already set it. `requestId` not set for `requestType=="web_search"`. `request.sessionId`: existing > derived session id (`helps.DerivedAntigravitySessionID(opts.Metadata, req.Metadata)`) > stable hash (`"-" + (first 8 bytes of SHA-256 of first user part text, big-endian, masked to 63 bits)`) > random negative decimal (`"-" + rand < 9e18`). `project` removed if empty (but execution aborts earlier with missing project). `request.safetySettings` deleted. Top-level `toolConfig` is moved under `request.toolConfig`.
- Translators involved (documented elsewhere): `internal/translator/antigravity/{claude,gemini,openai,interactions}` produce this envelope from each source format.

### 6.5b Schema sanitization
Applied only to schema locations, never whole payload (history args must not be rewritten): function declarations in `request.tools[].functionDeclarations|function_declarations[]` keys `parameters`, `parametersJsonSchema`/`parameters_json_schema` (renamed to `parameters`), `response`, `responseJsonSchema`, `response_json_schema`; and `request.generationConfig|generation_config` keys `responseSchema`, `responseJsonSchema`, `response_schema`, `response_json_schema`. Tool schemas use `util.CleanJSONSchemaForAntigravityTool(schema, useAntigravitySchema)` where `useAntigravitySchema = model contains claude | gemini-3-pro | gemini-3.1-pro` (adds required placeholder property `reason` "Brief explanation of why you are calling this tool" for empty schemas under VALIDATED; schemas are nested one level during cleaning to keep placeholder behavior). Cleaner (`internal/util/gemini_schema.go`, 1711 lines): inline local `$ref`, convert unresolved refs/const/enum/constraints/`not`/additionalProperties to description hints, merge allOf, flatten anyOf/oneOf to strongest branch (null -> `nullable`), enums forced to strings and dropped for tools, remove titles/metadata/x-*. Response schemas use `CleanJSONSchemaForAntigravityResponse` (keeps enum, `additionalProperties:false`, nullable). This file is a port-critical chunk of its own.

### 6.6 429 and error classification
- Executors return `statusErr{code, msg=raw body, retryAfter}` (type in `openai_compat_executor.go:1066` implementing `StatusCode()`, `RetryAfter()`, `IsCredentialScoped()`); no in-executor retries. Conductor (other section) handles retry/rotation/cooldown from status and `retryAfter`.
- `helps.ParseRetryDelay(body)` (`helps/json_retry_helpers.go`), in order: (1) `error.details[]` with `@type == type.googleapis.com/google.rpc.RetryInfo` -> `retryDelay` parsed as Go duration (e.g. `"0.847655010s"`, `"3s"`); parse failure is an error; (2) `ErrorInfo.metadata.quotaResetDelay` duration; (3) regex on `error.message` `after\s+(\d+)s\.?` seconds; (4) regex `after\s+((?:\d+h)?(?:\d+m)?(?:\d+s)?)` on lowercased message; else error `no RetryInfo found`.
- `decideAntigravity429(body)` kinds:
  - requires `error.status == RESOURCE_EXHAUSTED` (case-insens) else SoftRetry.
  - `ErrorInfo.reason == QUOTA_EXHAUSTED` -> FullQuotaExhausted.
  - `reason == RATE_LIMIT_EXCEEDED`: no retry delay -> SoftRetry; delay < 3s (`antigravityInstantRetryThreshold`) -> InstantRetrySameAuth; < 5 min (`antigravityShortQuotaCooldownThreshold`) -> ShortCooldownSwitchAuth; else FullQuotaExhausted.
  - Else if body lowercased contains `quota_exhausted` or `quota exhausted` -> FullQuotaExhausted; else SoftRetry.
  - Executor actions: ShortCooldownSwitchAuth: close credential idle conns, record short cooldown for (auth, model) with `retryAfter` (unless cooling disabled). FullQuotaExhausted: close idle conns; if credits mode was used and an `ErrorInfo.reason == INSUFFICIENT_G1_CREDITS_BALANCE` exists, mark credits permanently disabled. InstantRetry/SoftRetry have no executor-side action (the conductor sees the 429 and `retryAfter`).
- Short cooldown store: in-memory `sync.Map["{authID}|{model}|sc" -> time]`, or Home KV key `cpa:antigravity:short-cooldown:{authID}:{hash(model)}` (value = until unix nanos, EX duration+5s). Cooldown disabled when `QuotaCooldownDisabledForAuthWithConfig` (config `disable-cooling` per auth/global) is true. Home KV failures surface as 503 `home kv store unavailable`.
- Other statuses are passed through; 400 whose body contains `signature` triggers `clearAntigravityReasoningReplayOnInvalidSignature` (delete cached replay items if unchanged). Context cancel/deadline errors returned as-is. Request-level 4xx/5xx classification into auth-unavailable vs request-scoped is done in `sdk/cliproxy/auth` (`MarkResult`, `isCredentialScopedError`).
- Note: a missing `statusErr` wrapper matters: untyped executor errors are treated as credential faults, so keep typed status errors in the port.

### 6.7 Credits fallback (`antigravity_executor_credits.go`, `sdk/cliproxy/auth/conductor_home.go:1280-1490`)
- Enabled by config `quota-exceeded.antigravity-credits` (default false). Not supported in Home mode (`home_fallback_unsupported`).
- Conductor trigger (`shouldAttemptAntigravityCreditsFallback`): after normal attempts fail with status 429 or 503 (or `auth_not_found|auth_unavailable|model_cooldown` code) and providers include antigravity. `tryAntigravityCreditsExecute[Stream]` selects candidate antigravity auths: only for route models containing `claude`; sorts auths with a known available credits hint first (by ID) then unknown (by ID); skips known-unavailable; re-runs the request with `WithAntigravityCredits(ctx)` per candidate, which makes executors add `enabledCreditTypes:["GOOGLE_ONE_AI"]` and bypass the short-cooldown precheck.
- Balance probe (`updateAntigravityCreditsBalance`): `POST {prodOrCustomBase}/v1internal:loadCodeAssist` body `{"metadata":{"ideType":"ANTIGRAVITY"}}`; reads `paidTier.id` and `paidTier.availableCredits[]` entry with `creditType == "GOOGLE_ONE_AI"`: `creditAmount` and `minimumCreditAmountForUsage` (strings parsed to float). Available iff `creditAmount >= minimumCreditAmountForUsage`; no array -> Known, unavailable. Stored in memory map or Home KV `cpa:antigravity:credits-balance:{authID}` (EX 30m). Refresh throttled every 10 min per auth (`antigravityCreditsHintRefreshInterval`), 5s timeout, triggered from `ensureAccessToken` and after token refresh. Hint struct `AntigravityCreditsHint{Known, Available, CreditAmount, MinCreditAmount, PaidTierID, UpdatedAt}` in `sdk/cliproxy/auth/antigravity_credits.go`.
- On success in credits mode the failure state is cleared.

### 6.8 Reasoning replay (`antigravity_reasoning_replay.go`, 2661 lines; plus `internal/cache` antigravity replay ledger)
Purpose: Gemini thought signatures are opaque and must be replayed on later turns, but translated clients (OpenAI/Claude) drop them. The executor records, per (model, session), the signatures/thought parts/function calls the upstream emitted and re-inserts them into the next request's `request.contents`.
- Scope key: model name + session key, chosen in priority: Claude Code execution scope (+ `:context:<sha256-16 of normalized system>` lane), `Session-Id`/`Session_id` header (`responses:`), body `session_id`/`metadata.session_id` (`responses:`), execution session metadata (`execution:`), `prompt_cache_key` (`prompt-cache:`), derived session id (`derived:`), then payload `sessionId`/`request.sessionId` or stable hash (`session:`).
- Capture: `newAntigravityReasoningReplayAccumulator` observes SSE lines (and non-stream body via `cacheAntigravityReasoningReplayFromResponse`) and builds items (thoughtSignature on text/thought parts, functionCall parts with ids and args, each carrying a context hash = SHA-256 chain of prior content); limited by `AntigravityReasoningReplayCacheMaxItemsPerEntry` / `MaxBytesPerEntry` (overflow disables storing); committed after clean end of stream.
- Apply: `applyAntigravityReasoningReplayCache` reads ledger items and merges into payload: restore thoughtSignature on matching parts (matched by part content fingerprint and occurrence, gated by context hash for positional matches), restore native functionCall identity (id/name/args normalized against tool schemas) when Claude-facing reserved tool ids (`util.IsGeminiClaudeToolUseID`) are present, and restore functionResponse ids. Unresolvable reserved ids are degraded to deterministic synthetic ids with bypass signature (`degradeAntigravityClaudeToolProvenanceIDs`); first function call of a model turn left unsigned gets sentinel (`antigravityRepairUnsignedFirstFunctionCalls`). `ValidateGeminiFunctionCallPairing` guards the result; if replay broke pairing the original payload is used and the ledger entry invalidated; if even the original is invalid -> 400 `antigravity executor: invalid Gemini function call history`.
- Storage: in-process cache or Home KV (CAS-based; `ErrCompareAndSwapUnsupported` degrades silently). Replay errors never fail a request.
- Port advice: ship v1 without replay (just sanitize signatures with sentinel) and add replay later; the 2.6k lines are the highest-risk part of the port.

### 6.9 Compaction (`helps/antigravity_compaction.go`, `antigravity_executor_execute.go:executeCompaction`)
- Trigger: `/responses/compact` or an OpenAI Responses `input` item `type:"compaction_trigger"`. Executor builds a summary request (strip trigger items, append a user input_text asking for a concise summary), runs a normal non-stream `Execute` with source/response format `openai-response`, extracts summary text, and returns a Responses-style object containing a `compaction` item whose `encrypted_content` is a capsule: prefix `cpa-ag-compact-v1:` + base64(AES-256-GCM(JSON `{summary, model, created_at}`)), key derived from the fixed secret string `CLIProxyAPI` (SHA-256), nonce prepended. Response/stream ids `resp_ag_compact_<unixnano>`, `cmp_ag_compact_<unixnano>`. Streaming variant emits canned SSE frames.
- Expansion: any later request containing `type:"compaction"` items has capsules decrypted and replaced by summary text (invalid capsule -> 400).
- Usage tokens taken from the summary response.

### 6.10 Streaming parsing (antigravity)
- Upstream stream is SSE with `data: {"response": {...Gemini chunk...}, "traceId": "..."}` lines (50MB scanner cap). Steps per line: record raw, feed replay accumulator, `FilterSSEUsageMetadata` (hold `usageMetadata` to the terminal chunk; also tracks `traceId`: a stop chunk with no usage is remembered for 10 min so a following usage-only chunk with the same trace id is the one that carries usage, and the earlier stop chunk is dropped), `JSONPayload`, parse usage (`ParseAntigravityStreamUsage`: `response.usageMetadata` > `usageMetadata` > `usage_metadata`), resolve grounding URLs, translate; at end synth `[DONE]`, replay commit, publish usage. No explicit keepalive logic in the executor (the downstream keepalive, if any, is in handlers).
- Non-stream aggregation (`convertStreamToNonStream`): merges all lines into one response: consecutive text parts concatenated, thought parts concatenated (keep last `thoughtSignature`), functionCall/inlineData parts appended as-is (snake_case `thought_signature`/`inline_data` normalized to camelCase), fields `candidates.0.content.role`, `finishReason`, `modelVersion`, `responseId`, `usageMetadata` (default zeros) and `traceId` retained; output shape `{"response":{...},"traceId":"..."}`.
- Grounding URLs (`helps/antigravity_grounding_urls.go`): when request has `googleSearch` tool and the source request used a typed web search tool (Claude `web_search_20250305`/`web_search_20260209`, or Responses `web_search*`), each `groundingChunks[].web.uri` pointing to `https://vertexaisearch.cloud.google.com/grounding-api-redirect/...` is HEAD-requested (no redirect follow); the 3xx `Location` (https) replaces it.
- Usage: `ParseAntigravityUsage` for non-stream (`response.usageMetadata` first).

### 6.11 CountTokens
Same translation/thinking/signature steps, then deletes `project`, `model`, `request.safetySettings`, `request.toolConfig`, `request.labels`, `request.sessionId`, `POST {base}/v1internal:countTokens`, reads `totalTokens`, translates with `TranslateTokenCount`. 429 handling: close idle conns, attach `retryAfter`.

### 6.12 HttpRequest passthrough
`HttpRequest` strips all incoming headers except `Content-Type`, sets `User-Agent`, injects Bearer via `ensureAccessToken`; used by management api-call.

## 7. Cross-provider usage extraction summary
- gemini/vertex/aistudio: `usageMetadata` (fallback `usage_metadata`) at root.
- antigravity: `response.usageMetadata` (fallback root).
- interactions: `ParseInteractionsUsage`.
- Field mapping in `parseGeminiFamilyUsageDetail` (helps/usage_helpers.go): prompt, candidates, thoughts (reasoning), cached, total; includes token-accounting breakdown validation (negative values -> inconsistent quality).

## 8. Port notes and gotchas
- Delete-before-send rules that are easy to miss: aistudio drops `maxOutputTokens/responseMimeType/responseJsonSchema`; antigravity drops `maxOutputTokens` for non-Claude and `safetySettings` everywhere; all drop `session_id`.
- Model-name checks are substring based (`claude`, `gemini-3-pro`, `gemini-3.1-pro`, `gemini-3.1-flash-image`, `image`, `imagen`); keep them identical.
- `$alt` vs `alt`: stream default is `?alt=sse`; explicit alt uses `?$alt=<alt>`.
- Antigravity transport must be HTTP/1.1, no ALPN, per-credential connection pools; use reqwest with `http1_only()` and rustls with empty ALPN, one client per credential key.
- Client secrets above are public embedded values of the Antigravity desktop client; port as constants.
- Vertex SA JWT: Rust crates `jsonwebtoken` or `yup-oauth2`; cache tokens per credential, do not mint per request.
- Interactions (`gemini-interactions`) is a separate native API ("interactions") with `Api-Revision` header; translators live in `internal/translator/gemini/interactions` (2.1k lines) and `internal/translator/antigravity/interactions` (1.6k lines).
- gemini-cli/`cloudcode-pa` for provider `gemini` and Vertex-with-OAuth are absent; only the antigravity provider talks to cloudcode-pa.

---

# Part E. OpenAI-compat, xAI, Kimi, Devin, Meta (section numbers 6.x are local to this part)

Shared note: these executors all use the package-level `statusErr{code, msg, retryAfter *time.Duration, credentialScoped bool}` defined in `internal/runtime/executor/openai_compat_executor.go:1066` (`StatusCode()`, `RetryAfter()`, `IsCredentialScoped()`; `Error()` returns `msg` or `"status N"`). `credentialScoped=true` means the failure is about this credential (rotate/cool down); the conductor may otherwise treat the error as request-level. Every `Execute/ExecuteStream` also follows the shape in Part A section 2.7. `ForAPIKey()` (internal/runtime/executor/oauth_scope_executor.go) exists for Codex, Claude, Gemini, Vertex, OpenAI-compat, Meta, xAI, Kimi and the ws/auto wrappers: it returns a copy of the executor with `cfg = cfg.ForAPIKey()` (config view without OAuth-only settings), sharing session stores.

### 6.1 OpenAI-compatible (config-defined providers)

Files: `internal/runtime/executor/openai_compat_executor.go` (1126 LoC), `helps/openai_compat_max_tokens.go` (84), `helps/openai_compat_tool_results.go` (256), `util/provider.go` (provider key), `internal/config/config_types.go:836`.

**Identity and credentials**
- One executor instance per configured provider: `NewOpenAICompatExecutor(provider, cfg)`; `Identifier()` returns that provider key. Provider key for a config entry is `util.OpenAICompatibleProviderKey(name)` = `"openai-compatible-" + lowercased name` (const prefix in util/provider.go:15). Auth `Provider` may be that key or the bare `name`.
- Config (`openai-compatibility:` YAML list, struct `OpenAICompatibility`): `name`, `priority`, `disabled`, `prefix` (model namespace prefix), `base-url`, `api-key-entries[]{api-key, weight, proxy-url}`, `models[]`, `headers` (extra HTTP headers), `support-prompt-cache-key`, `disable-cooling`, `request-retry`, `request-scoped-errors[]`. Model entry: `name` (upstream id), `alias` (client-facing), `display-name`, `max-context-length`, `force-mapping` (rewrite response `model` back to alias), `image` (callable via `/v1/images/*`), `input-modalities` (e.g. `["text","image"]`), `use-max-completion-tokens`, thinking config.
- Each api-key entry is materialised as an in-memory `Auth` with attributes `api_key`, `base_url`, `compat_name`, `provider_key`, `config_index`, `auth_kind=apikey`, `source=config:...`, `header:<k>` for each `headers` entry. No auth file format of its own; but a JSON file with `type` equal to the provider key and `api_key`/`base_url` fields would load through the generic path (attributes come from config, not metadata, in practice).
- `resolveCredentials`: `baseURL = Attributes["base_url"]`, `apiKey = Attributes["api_key"]`. Missing baseURL: returns `statusErr{401, "missing provider baseURL"}`.
- `resolveCompatConfig(auth, req)`: Home mode reads options from `Metadata["credential_options"]` / attribute `support_prompt_cache_key`; else config entry by `config_index` (if `AuthSourceKind==config` and not disabled); else match by name among `compat_name`, `provider_key`, `auth.Provider` (case-insensitive, skipping disabled).

**Request**
- Target translator format: `openai` (chat completions) -> `POST {base_url trimmed of trailing "/"}/chat/completions`. If `opts.Alt == "responses/compact"`: format `openai-response`, endpoint `/responses/compact` (stream field deleted, encrypted reasoning content sanitised). Images: when `SourceFormat == "openai-image"`, endpoint is `/images/generations` or `/images/edits` chosen from the original request path suffix (default generations); body passes through (multipart for edits: `model` rewritten inside the multipart payload, boundary preserved); streaming images supported (`executeImagesStream`, `Accept: text/event-stream`).
- Headers: `Content-Type: application/json` (or the multipart content type), `Authorization: Bearer <api_key>` (omitted if empty), `User-Agent: cli-proxy-openai-compat`, stream adds `Accept: text/event-stream` and `Cache-Control: no-cache`; then custom `header:*` attributes applied last (can override anything, including User-Agent/Authorization).
- Mutations in order: translate (`TranslateRequestPairWithAPIKeyModelCompatibilityAndUpdateIntent`, with `isCompat = helps.APIKeyModelIsCompat(req)`); `ApplyRequestThinking(provider=Identifier())` (thinking provider key `openai`); payload config rules (Part A section 2.7); `NormalizeOpenAIToolResultsTextOnly` if the resolved model has `input-modalities` that exclude `image` (tool message content collapsed to strings, image parts replaced by a short marker); `NormalizeOpenAIMaxTokens(useMCT)`: if model config `use-max-completion-tokens=true` ensure `max_completion_tokens` set and `max_tokens` removed, else the reverse; `applyPromptCacheKey` when `support-prompt-cache-key`: priority `prompt_cache_key` from request/original/translated payload; for Claude-source requests the Claude Code prompt cache id (`helps.ClaudeCodePromptCache`); else UUIDv5-SHA1 (namespace OID) over `"cli-proxy-api:openai-compat:prompt-cache\0<provider>\0<model>\0<from-format>\0<session uuid>"` where session = `helps.ProviderSessionUUID`; model rewritten to the upstream model name (`model` field set to base model; alias resolved earlier by the auth manager).
- `apply_patch` tool bridging (`helps.ApplyPatch*`): custom/freeform apply_patch tools are converted to function tools and reconstituted on the way back; failures surface as `statusErr{502, helps.ApplyPatchUpstreamErrorMessage}`. `SupportsApplyPatch()` returns true.

**Response**
- Non-stream: status not 2xx -> `newOpenAICompatStatusError(status, headers, body)` (msg is the raw upstream body). 2xx: read body, `reporter.ObserveResponseModel`, translate back via `TranslateNonStream(to, responseFormat, ...)`, usage from `helps.ParseOpenAIUsage(body)`; when the client wants OpenAI-responses, `EnsureResponsesUsageDetails`.
- Stream: SSE frames parsed by a hand-written frame assembler (blank-line delimited, `data:` lines accumulated, `event:` name tracked, comment `:`/`id:`/`retry:` ignored). Error detection: event name in {`error`,`response.error`,`response.failed`} or payload with `error` / `response.error` / `type` of those / (`code` and `message` top-level) -> `statusErr` with status read from `status|status_code|error.status|error.status_code|response.error.status[_code]` (must be 400..599, else 502). A bare `{`/`[` JSON line instead of SSE -> 502 with that body. Stream ending without `[DONE]` or with incomplete frame -> 502 (messages: "upstream stream closed before [DONE]", "upstream stream ended with incomplete data before [DONE]", etc.). Stream usage via `StreamUsageBuffer.ObserveOpenAIStream`.
- Retry-After (`openAICompatRetryAfter`, only for 429): `Retry-After` header as integer seconds or HTTP date; if absent and body `error.code` contains `tpmratelimitexceeded` or message contains "tokens per minute" + "limit" + "exceeded" -> 60s fallback; else nil.
- `CountTokens`: local tokenizer estimate (no upstream call). `Refresh`: Home refresh if enabled; if metadata has `refresh_token`/`refreshToken` -> error "cannot refresh oauth credentials"; else returns auth unchanged.

### 6.2 xAI (Grok)

Files: `internal/runtime/executor/xai_*.go` (6308 LoC total: request 1808, ws executor 1880, response 1144, execute 445, stream 235, reasoning replay 306, media 147, tokens 149, auth 76, executor 118), `internal/auth/xai/{xai.go 495, types.go 75, token.go 106}`, `sdk/auth/xai.go` (132).

**Endpoints**
- `DefaultAPIBaseURL = https://api.x.ai/v1` (official API; used for websocket, compact, and HTTP when `using_api` true or key-based), `CLIChatProxyBaseURL = https://cli-chat-proxy.grok.com/v1` (Grok CLI proxy; OAuth default for HTTP chat and media when `using_api` false).
- Base URL selection (`xaiChatBaseURL`): `using_api` (attribute, then metadata; bool or string; if absent: `auth_kind` attribute/metadata equal to `oauth` => false, anything else including API keys => true; nil auth => true) true: `base_url` or default API URL. false: `base_url` if set and not the default API URL, else CLI chat proxy. `/responses/compact` (`xaiCompactBaseURL`): never the chat proxy (it 404s); default API URL unless explicit custom base. Websocket: default API URL or explicit non-proxy base (chat proxy returns 405 for upgrades).
- Paths: `POST {base}/responses` (stream and non-stream; non-stream actually still goes through the stream-oriented parse, error "stream disconnected before response.completed or response.incomplete" => status 408), `/responses/compact`, `/images/generations`, `/images/edits`, `/videos/generations`, `/videos/edits`, `/videos/extensions`, `/videos` (media: `xai_executor_media.go`, supports `x-idempotency-key` header from metadata key `idempotency_key`). Composer models prefixed `grok-composer-` get an isolated conversation.

**Headers**
- Always: `Content-Type: application/json`, `Authorization: Bearer <access_token or api key>`, `Accept: text/event-stream` (stream) or `application/json`, `Connection: Keep-Alive`, `x-grok-conv-id: <session id>` when known (execution session id, else `prompt_cache_key`, else derived session UUID for provider `xai`; composer models: Claude Code prompt cache id or random UUID).
- CLI chat proxy only (when `using_api=false` AND resolved base is `cli-chat-proxy.grok.com`): `X-XAI-Token-Auth: xai-grok-cli`, `x-grok-client-version: 1.0.44` (const `xaiClientVersionValue`; server answers HTTP 426 if too old; must track the real Grok CLI version), `User-Agent: xai-grok-workspace/1.0.44`, `x-grok-client-identifier: grok-shell`, `x-authenticateresponse: authenticate-response`. Custom `header:*` attrs applied last.

**Request building** (`prepareResponsesRequestTo`): target format `codex` (Responses dialect), then: thinking (provider `xai`), payload rules (executor-aware), `model`=base model, `stream` set, delete `previous_response_id`, `prompt_cache_retention`, `safety_identifier`, `stream_options`, `stop`; Codex multi-agent v2 input rewrite; apply_patch normalisation; tool normalisation: fold or flatten `namespace` tools (dispatcher function tool built when tool count would exceed limit), hard cap `xaiMaxTools=200`, hosted tool handling (`web_search`, `image_generation`, `x_search`; client function `web_search` aliased to `clientfn_web_search`; native `x_search` injected when config `xai.inject-x-search`), tool_choice pruning for removed tools, replace Codex Desktop `codex_app.automation_update` schema with permissive `{"type":"object","properties":{},"additionalProperties":true}` (the real schema makes xAI hang with no SSE), custom tool calls in input converted, reasoning input items normalised, encrypted content sanitised (`sanitizeXAIInputEncryptedContent`), `normalizeCodexInstructions`, image refs `{"image":{"image_url":..}}` -> `{"image":{"url":..}}`; `prompt_cache_key` = session id. Reasoning replay cache (`xai_reasoning_replay.go`): stores encrypted reasoning/assistant message from `response.completed` keyed per session (isolated by downstream API key for client-controlled keys) and re-injects into later input; cleared after compaction.
- Native image generation allowed by model version (`xaiSupportsNativeImageGeneration`, parses `grok-N-M` versions).

**Responses**: SSE (`response.*` events), translated from `codex` to client format via the Codex response translators. Usage: `helps.ParseCodexUsage` style (`response.usage`). `CountTokens`: local tiktoken `o200k_base` count over `instructions`, `input` (message text, function_call name+arguments, outputs, reasoning summary), function tools (name, description, parameters), `text.format` (name, schema); returns usage JSON.

**Errors** (`xaiStatusErr`): 403 whose body indicates bad credentials (`code`/`error.code`/`body.error.code` contains `bad-credentials`, or message "access token could not be validated") is remapped to 401 so refresh-after-unauthorized runs. 429 whose `code`/`error` text contains `free-usage-exhausted` or "included free usage" gets `retryAfter = 24h`. Other statuses pass through (conductor table in Part A section 2.4). 426 means client-version header too old.

**WebSocket executor** (`xai_websockets_executor.go`, `XAIWebsocketsExecutor`; routed by `XAIAutoExecutor`)
- Routing (`XAIAutoExecutor.ExecuteStream`): use websocket only when the DOWNSTREAM request is a websocket (`DownstreamWebsocket(ctx)`) AND the auth enables websockets (attribute or metadata `websockets` true). Otherwise HTTP; if `RequiredUpstreamWebsocket(ctx)` and not ws-capable -> `UpstreamWebsocketReplayRequiredError` (426). Non-stream `Execute` always HTTP. `/responses/compact` over ws -> 400.
- Upstream URL: base (`xaiCreds` base_url or `https://api.x.ai/v1`) + `/responses`, scheme http->ws, https->wss. Handshake headers: `Content-Type: application/json`, `Authorization: Bearer`, `x-grok-conv-id`, custom headers (NO cli-proxy identity headers).
- Frames: body is the prepared Responses body plus `type:"response.create"`, `stream`/`stream_options`/`background` deleted, `store:true`; `instructions` deleted when `previous_response_id` present (continuation). Client `response.append` supported. Warmup = body with `generate:false`; completion logged as warmup.
- Session reuse: reuses the Codex session machinery (`codexWebsocketSessionStore`, `codexWebsocketSession`: per execution session id, one connection, `reqMu` serialises requests, ephemeral session when no execution session id); session key target = (authID, wsURL, proxy); changed target drops upstream previous id. Details of connect/liveness/retry-once-on-send-failure are the Codex ones (Part C, section 8); the dial is retried once (`dial_retry`, `send_retry`) on a stale reused conn.
- Response ids: xAI response ids are remapped so the downstream sees a monotone chain even across reconnects: `xaiWebsocketIDState{downstreamToUpstream map, sequence, transcriptInput, replayCompactedTranscriptOnReset}`. When upstream `previous_response_id` is unknown (new upstream connection) the executor deletes it and prepends the recorded transcript (`prependTranscriptInput`) to `input`; when an upstream response id repeats, downstream id becomes `<upstreamId>-xai-<n>`. Response events are rewritten by `rewriteXAIWebsocketDownstreamIDs` (response.id, previous_response_id, and embedded strings).
- Compaction: input item type `compaction_trigger` is executed specially (`executeCompactionTriggerFromWebsocketContext`, uses HTTP compact path); compacted transcript retained for replay after reset.
- Terminal events: `response.completed`, `response.done`, `error` (and `response.incomplete`/`response.failed` when apply_patch active). Terminate reasons logged: completed, context_done, read_error, unexpected_binary, upstream_error, invalid_tool_arguments. Binary frames are an error.
- `CloseXAIWebsocketSessionsForAuthID(authID, reason)` closes sessions when an auth is removed/disabled.

**OAuth / login** (`internal/auth/xai/xai.go`, `sdk/auth/xai.go`)
- Device authorization grant (RFC 8628) against OIDC discovery: `GET https://auth.x.ai/.well-known/openid-configuration` -> `device_authorization_endpoint`, `token_endpoint` (stored in credential as `token_endpoint`).
- `client_id = b1a00492-073a-47ea-816f-4c329264a828`, `scope = "openid profile email offline_access grok-cli:access api:access"`. Device request: form `client_id`, `scope`. Poll: form `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code`, `client_id`; default interval 5s; max wait 30 min; HTTP timeout 30s; standard errors (`authorization_pending`, `slow_down`, `expired_token`, `access_denied`).
- Refresh: POST token endpoint form `grant_type=refresh_token`, `client_id`, `refresh_token`; lead time 5 minutes (`xaiauth.RefreshLead()`). Executor.Refresh updates metadata: `type=xai`, `auth_kind=oauth`, `access_token`, `refresh_token` (if rotated), `id_token`, `token_type`, `expires_in`, `expired`, `email`, `sub`, `token_endpoint`, default `base_url=https://api.x.ai/v1`, `last_refresh` (RFC3339 UTC); attributes `auth_kind=oauth`, `base_url`.
- Credential JSON (`XAITokenStorage`): `type:"xai"`, `access_token`, `refresh_token`, `id_token`, `token_type`, `expires_in`, `expired`, `last_refresh`, `email`, `sub`, `base_url`, `redirect_uri`, `token_endpoint`, `auth_kind:"oauth"`; extra operator fields: `using_api` (bool), `websockets` (bool). Filename `xai-<sanitised email>.json`, else `xai-<sub>.json`, else `xai-<unix-ms>.json`. API-key xAI is configured in config (`xai-api-key`) not files.

### 6.3 Kimi (Moonshot)

Files: `kimi_executor.go` (1393), `kimi_thinking_replay.go` (484), `helps/kimi_responses.go` (188), `internal/auth/kimi/{kimi.go 668, token.go 154}`, `sdk/auth/kimi.go` (166).

- Provider keys: `kimi` (domain kimi.com), `kimi-ai` and `kimi.ai` (domain kimi.ai); a single `KimiExecutor` (`Identifier()=="kimi"`). `KimiExecutor` embeds a `ClaudeExecutor` for Claude-format traffic.
- Endpoints: base `https://api.kimi.com/coding` (kimi.ai: `https://api.kimi.ai/coding`; overridden by `base_url` attribute/metadata, `helps.ResolveKimiBaseURL`). Chat: `{base}/v1/chat/completions` (if base already ends `/v1`, no extra `/v1`); Responses: `{base}/v1/responses`; Claude source: `ClaudeExecutor` with `base_url` = base minus `/v1` (Kimi exposes an Anthropic-compatible `/v1/messages`). `/responses/compact` -> 501.
- Format routing (`RequestToFormat`): source `claude` -> claude executor path (with Kimi thinking replay cache around it), source `openai-response` -> `executeResponses` (target `openai-response`/codex-multi-agent translation; `NormalizeKimiResponsesInput` keeps parallel function_call outputs contiguous), else target `openai` chat.
- Headers (`applyKimiHeadersWithAuth`): `Content-Type: application/json`, `Authorization: Bearer <access_token>`, `User-Agent: CLIProxyAPI/<version>`, `X-Msh-Platform: CLIProxyAPI`, `X-Msh-Version: <version>`, `X-Msh-Device-Name: <hostname>`, `X-Msh-Device-Model: "<GOOS> <GOARCH>"`, `X-Msh-Device-Id: <id>`, `Accept: text/event-stream` (stream) or `application/json`; custom headers last. Device id: per-credential `device_id` (metadata/storage) first; else contents of kimi-cli's `device_id` file (`~/.local/share/kimi/device_id` linux, `~/Library/Application Support/kimi/device_id` mac, `%APPDATA%\kimi\device_id` windows); else literal `cli-proxy-api-device`.
- Token: `Metadata["access_token"]`, else attributes `access_token` / `api_key`.
- Request mutations (chat): translate to `openai` (with Codex multi-agent v2 handling); `model` = `normalizeKimiUpstreamModel`: lowercase, strip `[1m]`, aliases (`kimi-k2.8*`, `k2.8*`, `kimi-k2.7-code`, `kimi-for-coding`, `for-coding` -> `kimi-for-coding`; `*-highspeed` variants -> `kimi-for-coding-highspeed`), else strip `kimi-` prefix; thinking suffix `(…)` preserved then handled by `ApplyRequestThinking(provider "kimi")`; payload rules; `normalizeKimiToolMessageLinks` (repair assistant/tool message adjacency, drop empty assistant messages, fallback `[reasoning unavailable]` reasoning_content for assistant tool-call turns when thinking is on); `normalizeKimiTools` (inline local `$ref`, strip `$defs/definitions`, root `type:"object"`); `normalizeKimiTemperature` (thinking disabled: keep only 0.6; otherwise only 1.0, else drop `temperature`).
- Response: SSE chat completions (`data:` lines, `[DONE]`), usage via OpenAI parser; non-2xx -> `statusErr{status, body}` (no special retry parsing; conductor table applies).
- Thinking replay (Claude path): `kimi_thinking_replay.go` caches assistant thinking blocks from responses (stream accumulator, byte-budgeted) keyed by scope (session + model family) and restores them into later requests whose assistant content matches; cleared after errors (`shouldClearKimiThinkingReplayAfterError`).
- OAuth: device flow. `client_id = 17e5f671-d194-4dfb-9706-5516cb48c098`. Hosts `https://auth.kimi.com` / `https://auth.kimi.ai`; device: `POST {host}/api/oauth/device_authorization` (form `client_id`); token: `POST {host}/api/oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code` + `device_code` + `client_id`, or `grant_type=refresh_token` + `refresh_token` + `client_id`. Requests carry the `X-Msh-*` headers. Poll default 5s, max 15 min, `slow_down` adds interval, errors `authorization_pending`/`expired_token`/`access_denied`. Refresh: 401/403 => "refresh token rejected"; success updates `access_token`, `refresh_token` (if present), `expired` (RFC3339 from `expires_at`), `last_refresh`, defaults `type`, `domain`, `base_url`. Refresh lead 5 minutes; singleflight per refresh token.
- Credential JSON: `type` (`kimi` | `kimi-ai`), `access_token`, `refresh_token`, `token_type`, `scope`, `timestamp` (unix ms), `domain` (`kimi.com`|`kimi.ai`), `base_url`, `expired` (RFC3339), `device_id`, `last_refresh`. Filename `kimi-<unixms>.json` / `kimi-ai-<unixms>.json`.

### 6.4 Meta (Muse / meta.ai)

Files: `meta_executor.go` (388), `meta_executor_execute.go` (341), `meta_executor_stream.go` (186), `helps/meta_tools.go` (46), `internal/auth/meta/meta.go` (564), `sdk/auth/meta.go` (159).

- Upstream: `{base_url}/responses`, default `base_url = https://api.meta.ai/v1` (from mint response `base_url`). Target format `codex` (Responses dialect). `/responses/compact` -> 501. Non-stream `Execute` sends `stream:true` upstream, collects the SSE and converts the `response.completed`/`response.incomplete` event to a single response (`translateMetaCompleted`, `metaAsCompletedEvent` also accepts a plain response object by wrapping it).
- Headers (`applyMetaAPIHeaders`): `Content-Type: application/json`, `Authorization: Bearer <api key>`, `User-Agent: muse-build/1.3.0 (interactive; macos-aarch64; build ac7280f2aca67769d1455a8847bb502b617d50f6)`, `X-Client-Id: tbh:tui`, `Accept: text/event-stream` + `Cache-Control: no-cache` (stream) or `Accept: application/json`; custom headers last.
- Mutations: thinking (provider `meta`), payload rules, `model`=base model, `stream` set, delete `generate`, `prompt_cache_retention`, `safety_identifier`, `stream_options`, `client_metadata`; apply_patch normalisation; `normalizeCodexInstructions`; encrypted reasoning sanitised; `helps.SanitizeMetaWebSearchTools`; `NormalizeCodexToolIntegerTypes`.
- Credentials: two-stage. Login yields a DCA token (`dca:...`, device client access token) which is exchanged for a long-lived API key. API key resolution (`metaCreds`): attributes `api_key`/`access_token` (ignoring values starting `dca:`), metadata `api_key`/`access_token`, then storage; base url from `base_url`/`api_base_url`. `ShouldPrepareRequestAuth`: true when a DCA token exists but no usable API key; `PrepareRequestAuth` mints one (singleflight on DCA token) and the manager persists it. `Refresh` = re-mint via `POST https://api.meta.ai/muse-code/key` (override env `META_MINT_URL`), headers `Authorization: Bearer <dca token>`, `User-Agent: muse-code/1.0.2`, `Content-Type`/`Accept: application/json`, body `{"dca_token":"<token>"}`; response fields `api_key`, `base_url`, `user_email`, `user_full_name`, `subs_tier_name`, `subs_tier_id`, `is_subs_active`, `has_payment_method`, `require_payment`, `can_subscribe`. No scheduled refresh (RefreshLead nil); recovery on demand (401 / request preparation).
- Errors (`wrapMetaUpstreamError`): 429 with `error.resets_at` (unix seconds) -> `retryAfter = resets_at - now`; subscription quota (429 and message contains "subscription quota"/"quota exhausted", or `error.code` is `rate_limit_exceeded`/contains `quota` with `error.resets_at`) -> credential-scoped rate-limit error; 404 -> retryAfter from `resets_at` else fixed 5 minutes (`metaNotFoundCooldown`). Stream events of type `error`/`response.failed` map to status from `error.code` (400..599) else 502.
- OAuth device flow: `POST https://auth.meta.com/oidc/device/authorization/` (form `client_id=1031625952748946`), poll `POST https://auth.meta.com/oidc/device/token/` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code`, `client_id`; `User-Agent: muse-code/1.0.2`; default poll 5s, max 15 min, `slow_down` +5s, errors `authorization_pending`/`access_denied`/`expired_token`; HTTP timeout 30s. After token, the key is minted immediately.
- Credential JSON (`MetaTokenStorage`): `type:"meta"`, `auth_kind:"oauth"`, `access_token` (= minted API key when available), `dca_token`, `api_key`, `token_type`, `expires_in`, `expired` (empty when an API key exists), `dca_expired`, `dca_expires_at` (unix), `last_refresh`, `base_url`, `email`, `name`; refresh also writes `subs_tier_name`, `subs_tier_id`, `is_subs_active`, `has_payment_method`. Filename `meta-<sanitised email>-<sha256(email)[:8] hex>.json`, or `meta-<sha256(sub)[:8]>.json`, or `meta-oauth.json`.

### 6.5 Devin (Windsurf/Codeium backend, Connect-RPC protobuf)

Files: `devin_executor.go` (2559), `helps/devin_wire.go` (1262), `helps/devin_models.go` (325), `internal/auth/devin/{devin_auth.go 384, user_status.go 377, record.go 132, pkce.go 31}`, `sdk/auth/devin.go` (304), `cmd/fetch_devin_models`.

- Upstream: `https://server.codeium.com` (override `base_url`), chat RPC `POST /exa.api_server_pb.ApiServerService/GetChatMessage` (server-streaming Connect protocol), status RPC `POST /exa.seat_management_pb.SeatManagementService/GetUserStatus` (unary, `Content-Type: application/proto`).
- Headers (chat): `Authorization: Basic <token>-<token>` (session token repeated, literal), `Content-Type: application/connect+proto`, `Connect-Protocol-Version: 1`, `Accept: */*`, `Sentry-Trace: <32hex>-<16hex>-1` (not on unary status/catalog calls), `User-Agent` forcibly EMPTY (header slice set to `[""]` so no UA is sent). Client must disable compression negotiation: `Accept-Encoding: identity` (`NewDevinHTTPClient`, transport `DisableCompression`). Custom headers last.
- Format: executor consumes the internal `interactions` format (Gemini Interactions API shape): any other source is translated `SourceFormat -> interactions` first (`RequestToFormat` = interactions); responses are produced as interactions events and translated back to the client format. `parseInteractionsPayload` extracts system prompt, turns (user/assistant/tool), tools, temperature, max tokens, session/cascade ids, thinking level/budget, images (data URLs), thinking signatures.
- Request wire (hand-rolled protobuf in `BuildDevinGetChatMessageRequest`, wrapped in a Connect envelope `[flag 0x00][u32 BE length][payload]`):
  - F1 ClientMetadata: 1 `"chisel"`, 2 `"3000.10.21"`, 3 session token, 4 `"en"`, 5 OS name (GOOS), 7 `"3000.10.21"`, 12 `"chisel"`, 31 device fingerprint (732 hex chars: random per request when no `device_seed`, else concatenated SHA-256 hex of `"<seed>-<counter>"` truncated to 732).
  - F2 system prompt (sanitised: strips Claude Code attribution/CLI identity lines; configured sensitive words get zero-width obfuscation via `SensitiveWordMatcher`).
  - F3 repeated prompts: 1 message id (uuid), 2 source varint (1 user default; assistant/tool values per code), 3 content, 6 repeated tool calls {1 id, 2 name, 3 arguments}, 7 tool_call_id, 10 images {1 base64, 2 mime default image/png}, 11 thinking text, 12 signature bytes, 18 signature type.
  - F7 varint 5. F8 sampling config {1:1, 2: max tokens (default 128000, clamped to model max), 3: 400, 5: temperature fixed64 double (default 1.0), 7: 40, 8: 0.95 as double of float32}.
  - F10 repeated tools {1 name, 2 description (sanitised), 3 parameters JSON bytes}; Codex `automation_update` dropped.
  - F15 session {1 session uuid, 2 turn index (per-session counter, omitted if 0), 3: 4, 4: 14 on user-turn boundary}. F16 cascade id (prompt-cache key; defaults to session id). F20 varint 1. F21 chat model uid.
- Model uid (`ResolveDevinChatModelUID`): strip `devin/` prefix; names already ending in a known effort suffix pass through; else normalise effort from thinking suffix/budget and map using the catalog (`internal/registry` devin models JSON) and special aliases (examples: `claude-sonnet-4-5` -> `MODEL_PRIVATE_2` / `MODEL_PRIVATE_3` when thinking; `model_gpt_5_2` -> `MODEL_GPT_5_2_<LEVEL>`; `claude-opus-4-6` + thinking -> `claude-opus-4-6-thinking`; `swe-1-7[-medium]`, `glm-5-2[-none|-max][-1m]`); empty -> `swe-2-high`.
- Response (server-streaming): Connect frames `[flag][len]`; flags data 0x00, compressed 0x01 (gzip, max decompressed 64 MiB), end-stream 0x02 (trailer JSON). Max frame 16 MiB. Frame protobuf fields: 1 output id, 2 timestamp, 3 text delta (repeated, joined), 6 tool call delta, 7 usage (contains upstream headers sub-field 8: name/value pairs such as `x-request-id`), 9 thinking delta, 10 signature bytes delta, 17 message id, 21 signature type, 28 response dimension groups (token usage: input_tokens, output_tokens, cached_input_tokens), fixed64 12 latency, varints 4 delta tokens and 5 stop reason. UTF-8 split across frames handled by `UTF8SplitBuffer`. Max 128 tool calls per response.
- Trailer errors (`ParseDevinTrailerError`; JSON `{"error":{"code","message"}}` in the end-stream frame) mapped to HTTP: `invalid_argument` 400 (502 if message has "internal error"), `internal` 502, `unauthenticated` 401, `permission_denied` 403 (429 if message has "high demand"), `resource_exhausted` 429, `unavailable` 503, `canceled` 499, `deadline_exceeded` 504, `failed_precondition` 429 if message mentions quota/credit/acu/exhausted/limit else 400; unknown 502. HTTP non-2xx -> `newDevinStatusError` with `Retry-After` (seconds or HTTP date) on 429.
- Non-stream implemented by consuming all frames into an interactions response (`consumeDevinFramesToInteractions`). `CountTokens` local estimate. Upstream injects ~390-580 prompt tokens of hidden system prompt; usage reflects it.
- Auth: PKCE browser login. `DefaultAppBaseURL https://app.devin.ai`, `DefaultAPIBaseURL https://api.devin.ai`. Authorization URL: `https://app.devin.ai/auth/cli/continue?redirect_uri=<enc>&state=<enc>&prompt=select_account&code_challenge=<enc>&code_challenge_method=S256` (exact param order; with no redirect it appends `cli_pkce_marker=1` for the manual-code headless variant). Redirect `http://127.0.0.1:<port>/callback`, port = `LoginOptions.CallbackPort` or ephemeral (0). Exchange: `POST https://api.devin.ai/auth/cli/token` JSON `{"code","code_verifier"}` -> `{"token":"..."}`; token normalised with prefix `devin-session-token$` (added if it starts with `eyJ`). Profile: `GET https://api.devin.ai/v3/self` (Bearer) -> `user_name`, `user_id`, `org_id`. Status/quota: `GetUserStatus` (email, plan, team, daily/weekly quota remaining percent and reset times, plan start/end) stored in `Quota.Signals` (`plan`, `daily_quota_remaining_percent`, `weekly_quota_remaining_percent`, `*_reset_at`, `plan_start`, `plan_end`) and refreshed by `Executor.Refresh` (which only re-reads status; the session token is never rotated). No scheduled refresh lead (nil).
- Credential JSON: `type:"devin"`, `api_key` and `session_token` (both = session token), `user_name`, `user_id`, `org_id`, `email`, `plan`, `auth_kind:"oauth"`, optional `base_url`, `device_seed` (stable fingerprint seed), `team_id`, `org_name`. Filename `devin-<sanitised user_name|user_id>.json` (non `[A-Za-z0-9_.@-]` => hashed `user-<sha256[:8]>`). Credential lookup order: attributes then metadata for `api_key`, `session_token`, `token`.
