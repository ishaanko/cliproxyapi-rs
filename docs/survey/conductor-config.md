# Conductor, session affinity, registry, config, watcher: Rust port spec

Source root `R` = `/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI` (Go module `github.com/router-for-me/CLIProxyAPI/v8`). Paths are relative to `R`. Verified against the tree; behaviours marked "(code)" were read, not inferred.

Abbreviations: `AUTH` = `sdk/cliproxy/auth`, `SVC` = `sdk/cliproxy`, `REG` = `internal/registry`, `CFG` = `internal/config`, `WAT` = `internal/watcher`.

## 0. Scope, tiers, file map

Tier legend: **CORE** must be ported for a working proxy; **NICHE** port later or skip; **LIST** named only (Home control plane, plugins, redis queue and similar).

| Area | Files | Tier |
|---|---|---|
| Manager state, Register/Update/Remove/Load/persist | `AUTH/conductor.go`, `conductor_lifecycle.go`, `types.go`, `status.go`, `classification.go` | CORE |
| Execution loops (Execute / ExecuteCount / ExecuteStream) | `AUTH/conductor_execution.go`, `conductor_stream.go` | CORE |
| Pick path, retry rounds, wait | `AUTH/conductor_selection.go` | CORE |
| MarkResult, cooldown math, error classification | `AUTH/conductor_cooldown.go`, `cooldown_state.go`, `quota_signals.go`, `errors.go`, `selector.go` (error types) | CORE |
| Selectors RR / WRR / fill-first | `AUTH/selector.go` | CORE |
| Scheduler (incremental index, same semantics as selector) | `AUTH/scheduler.go` | NICHE (optimisation) |
| Session affinity | `AUTH/selector.go` (SessionAffinitySelector), `session_cache.go`, `SVC/session/*` | CORE if `routing.session-affinity` is wanted; LCP part NICHE |
| Request-scoped error rules | `AUTH/conductor_request_scoped_errors.go` | CORE |
| Refresh loop | `AUTH/conductor_refresh.go`, `auto_refresh_loop.go`, `metadata_merge.go` | CORE |
| Model alias / prefix / pools | `AUTH/conductor_models.go`, `oauth_model_alias.go`, `api_key_model_capabilities.go`, `response_model_rewriter.go` | CORE |
| Service wiring | `SVC/builder.go`, `service*.go`, `providers.go` | CORE |
| Model registry | `REG/model_registry.go`, `model_definitions.go`, `model_updater.go`, `models/models.json` | CORE |
| Config | `CFG/*` | CORE |
| Watcher, synthesizer, diff | `WAT/*` | CORE |
| File store | `sdk/auth/filestore.go` (NOT `internal/store`) | CORE |
| Usage | `SVC/usage/*`, `AUTH/error_events.go` | CORE (record) / NICHE (accounting) |
| `internal/credentialweight`, `internal/modelconfig` | small | CORE |
| Home (`conductor_home*.go`, `home_*.go`, `service_home.go`, `internal/home`, `executionregistry`) | ~6000 LoC | LIST |
| Plugins (`pluginhost`, `service_plugins.go`, `home_plugins.go`, PluginScheduler, model routers, interceptors) | LIST |
| Redis queue (`internal/redisqueue`), mDNS discovery (`discovery_advertiser.go`), pprof, wsrelay/aistudio | LIST |
| Git / Postgres / object stores (`internal/store/{gitstore,postgresstore,objectstore,postgres_cooldown_store}.go`) | LIST |
| Antigravity credits fallback (`AUTH/antigravity_credits.go`) | NICHE |
| Codex/xAI websocket session closing on auth removal | NICHE |

Note the file store the task calls "internal/store file store" actually lives in `sdk/auth/filestore.go`; `internal/store` holds only git/postgres/object stores. Sections 12 and 13 cover it.

Port recommendation (YAGNI): implement the **legacy pick path** (filter candidates, keep highest priority tier, call selector). `scheduler.go` only reproduces that result faster with incremental shards; do not port it first. Home mode is a whole second dispatch path guarded by `m.HomeEnabled()`; everything below describes non-Home mode.

---

## 1. Data model

### 1.1 `Auth` (`AUTH/types.go`)

```rust
struct Auth {
  id: String,                       // stable; file auths: path relative to auth-dir ("claude-a@b.json"); lowercased on Windows
  registration_epoch: u64,          // bumped on every Register (incl. re-register after remove); stale epoch writes rejected
  generation: u64,                  // bumped on every mutation (MarkResult, Update, refresh); gates stale scheduler/registry projections
  index: String,                    // runtime only: hex(sha256(seed))[..16 hex chars] (8 bytes), see 1.3
  provider: String,                 // "claude" "codex" "gemini" "gemini-interactions" "vertex" "aistudio" "antigravity" "kimi"* "xai" "devin" "meta" "openai-compatible-<name>" or "openai-compatibility"
  prefix: String,                   // model namespace "teamA"; single path segment, no "/"
  file_name: String,                // runtime only; basename or id
  storage: Option<Box<dyn TokenStorage>>, // runtime only; login flows write the token file via this
  label: String,                    // email for oauth files; compat name for compat; "<provider>-apikey" for config keys
  status: Status,                   // unknown|active|pending|refreshing|error|disabled
  status_message: String,
  disabled: bool,                   // operator disable
  unavailable: bool,                // aggregate cooldown flag
  proxy_url: String,                // per-auth override: "" inherit, "direct"|"none" bypass, else URL
  attributes: BTreeMap<String,String>, // immutable config-ish, string-typed (see 1.2)
  metadata: serde_json::Map,        // mutable provider state = the auth JSON file body (tokens, email, weight, priority, headers, ...)
  quota: QuotaState,                // credential-wide
  last_error: Option<AuthError>,
  created_at, updated_at, last_refreshed_at, next_refresh_after, next_retry_after: DateTime (zero = unset),
  refresh_failures: i32,            // runtime only
  model_states: HashMap<String, ModelState>, // key = canonicalModelKey(selection model)
  runtime: Option<Box<dyn Any>>,    // executor-owned; never serialised
  success: i64, failed: i64,        // counters, runtime only
  recent_requests: [Bucket;20],     // 20 buckets x 600 s (3h20m ring): {bucket_id, success, failed}
}
struct QuotaState { exceeded: bool, reason: String /* ""|"quota"|"credential_quota"|"cloudflare challenge" */,
  next_recover_at: DateTime, backoff_level: i32, observed_at: DateTime, signals: BTreeMap<String,String> }
struct ModelState { status: Status, status_message: String, unavailable: bool, next_retry_after: DateTime,
  last_error: Option<AuthError>, quota: QuotaState, updated_at: DateTime }
struct AuthError { code: String, message: String, retryable: bool, http_status: i32 }
```

JSON tags (used only for the cooldown store and management API): snake_case as in the Go tags, `omitempty` on strings/maps. Zero `time.Time` serialises as `0001-01-01T00:00:00Z`; use `Option<DateTime>` and treat None and that value as unset.

`Clone` is deep for attributes, metadata (shallow values), model_states, quota signals. The manager always hands out clones; internal state is mutated only under `m.mu`.

`Status` semantics: `active` ready; `error` set by any cooldown-causing failure; `disabled` blocks everything; `pending`/`refreshing`/`unknown` are not special-cased by selection (selection looks at `disabled`, `status==disabled`, availability fields, token expiry).

AuthError.code values produced in this layer: `auth_not_found` ("no auth available" or "no auth candidates"), `auth_unavailable` (HTTP 503 and `retryable=true` when a recovery time is known), `provider_not_found`, `executor_not_found`, `empty_stream`, `model_cooldown` (see 5.4), `request_scoped`, `connection_lifecycle`, `transient_transport`, `force_cooldown`, `unauthorized` (refresh failures), `model_not_found`, `home_unavailable`.

### 1.2 Well-known attributes and metadata keys

Attributes (string values):

| Key | Meaning |
|---|---|
| `source` | file path, or `config:<provider>[<12hex>]` for config-synthesised |
| `path` | absolute file path (file auths) |
| `source_backend` | `file`/`config`/`git`/`memory`/`objectstore`/`postgres` |
| `runtime_only` | `"true"`: never persisted (aistudio ws channels) |
| `auth_kind` | `"apikey"` or `"oauth"` (set by synthesizer) |
| `api_key`, `base_url`, `config_index`, `provider_key`, `compat_name` | API-key / compat routing |
| `priority` | decimal int, larger wins; absent = 0; `file_priority="true"` if from JSON `priority` |
| `weight` | decimal int, absent = 1, non-positive normalised to 0 |
| `header:<Name>` | custom upstream header (from config `headers` or JSON `headers`) |
| `excluded_models` | comma list, lowercased, sorted, union of per-key and global exclusions; `excluded_models_hash` |
| `model_aliases` | JSON array of `{name,alias,fork,display-name,force-mapping}` per-auth OAuth aliases |
| `models_hash` | sha256 of routing-relevant model config (change detection) |
| `websockets`, `plan_type`, `codex_alpha_search`, `codex_disable_cloaking`, `fingerprint_profile`, `rebuild_mid_system_message`, `note`, `email`, `domain` | provider-specific |
| `auth_index_seed`, `plugin_virtual`, `virtual_source` | plugin expansion (LIST) |

Metadata keys (JSON file body; legacy hyphen spellings are rewritten to snake_case on load by `NormalizeCredentialMetadata`, canonical wins if both present): `type` (provider), `access_token`, `refresh_token`, `id_token`, `email`, `expired`/`expire`/`expires_at`/`expiresAt`/`expiry`/`expires`, `expires_in`+`timestamp|issued_at`, nested `token{}`, `disabled`, `proxy_url`, `prefix`, `priority`, `weight`, `headers{}`, `excluded_models[]`, `model_aliases[]`, `disable_cooling`, `request_retry`, `request_scoped_errors[]`, `tool_prefix_disabled`, `fingerprint_profile`, `refresh_interval_seconds|refresh_interval`, `last_refresh`, `websockets`, `note`, `label`, `project_id`, `plan_type`, `base_url`, `domain`, `cloak_*`, `timezone`. Legacy renames: `api-key`, `base-url`, `disable-cooling`, `excluded-models`, `fingerprint-profile`, `model-aliases`, `proxy-url`, `request-retry`, `request-scoped-errors`, `tool-prefix-disabled`.

Type coercion helpers: bool accepts bool/"true"/number (`!= 0`); int accepts int/float/json-number/numeric string. `request_retry < 0` = unset.

### 1.3 Derived values

`AuthKind()` order: attribute `auth_kind` normalised (`apikey|api_key|api-key` -> apikey, `oauth|oauth2` -> oauth); then metadata `auth_kind`; then `attributes.api_key` non-empty -> apikey; then any of metadata `access_token|refresh_token|id_token|email|token_type|expires_at|expired` non-empty or non-empty `token{}` -> oauth; else "".

`AuthSourceKind()`: `runtime_only=true` -> memory; `source_backend`; `source` prefixed `config:` -> config; other source -> file; `path` or `file_name` -> file.

`executorKeyFromAuth` (which executor handles the auth): if `compat_name` set: `OpenAICompatibleProviderKey(provider_key or compat_name)`; if provider == "openai-compatibility": key from label (default "openai-compatibility"); `kimi.com`->`kimi`, `kimi.ai`->`kimi-ai`; else lowercase provider. `OpenAICompatibleProviderKey(n)` = `"openai-compatible-" + lower(n)` unless n already has that prefix or is empty/"openai-compatibility".

Auth index seed (`EnsureIndex`, shown in management/usage, must be stable across restarts):
1. attribute `auth_index_seed` -> `"auth_index_seed:" + seed`
2. file path (attr `path`, else `source`, else `file_name`, else `id`) ending `.json`: `"<type>:<abs clean path>"` where type = lower(metadata.type or provider)
3. API key with prefix by provider/compat: `openai-compatibility` (compat_name or provider is openai-compatibility), `gemini-api-key`, `interactions-api-key`, `codex-api-key`, `xai-api-key`, `claude-api-key`, `meta-api-key` then `"<prefix>:<base_url>+<api_key>"`
4. `"id:" + id`
Index = first 8 bytes of sha256(seed) hex.

Expiry (`ExpirationTime`): if `access_token` is a JWT (3 dot parts, base64url, `exp` as number or numeric string) use `exp`; else first parseable of the expire keys (RFC3339, RFC3339Nano, `YYYY-MM-DD HH:MM:SS`, `YYYY-MM-DD HH:MM`, unix s, unix ms when > 1e12); else `expires_in` (>0) + `timestamp|issued_at|issuedAt`; else recurse into `token`/`Token` map. `AccessTokenExpirationTime` prefers the JWT strictly. `access_token` read from `metadata.access_token` else `metadata.token.access_token`.

### 1.4 Execution-facing types (`SVC/executor/types.go`)

```rust
struct Request { model: String, payload: Bytes, format: Format, metadata: Map }
struct Options { stream: bool, alt: String, headers: HeaderMap, query: Query, original_request: Bytes,
  source_format: Format, response_format: Format, metadata: Map<String,Value>,
  request_after_auth_interceptor: Option<..>, web_socket_response_observer: Option<..>,
  execution_lifecycle: Option<..> /*Home*/, proxy_url: String }
struct Response { payload: Bytes, metadata: Map, headers: HeaderMap }
struct StreamChunk { payload: Bytes, err: Option<Error> }
struct StreamResult { headers: HeaderMap, chunks: Receiver<StreamChunk> }
trait ProviderExecutor { identifier(); execute(); execute_stream(); refresh(auth)->Auth; count_tokens(); http_request() }
// optional executor traits: ForAPIKey()->Executor (strip oauth-only config), RequestAuthPreparer{should_prepare,prepare}, ExecutionSessionCloser, RefreshEvaluator{should_refresh(now,auth)}, RefreshLead()
```

Metadata keys (string constants): `requested_model`, `request_path`, `disallow_free_auth`, `auth_selection_model`, `reasoning_effort`, `service_tier`, `generate`, `pinned_auth_id`, `selected_auth_id`, `selected_auth_index`, `selected_auth_callback`, `selected_auth_index_callback`, `execution_session_id`, `derived_session_id`, `canonical_session_id`, `parent_session_id`, `is_fork`, `is_compaction`, `node_kind`, `lcp_*` (fingerprints, min_prefix_length, tail_fingerprints, environment_digest, access_generation, affinity_session_id), `caller_scope`, `session_affinity_provider`, `session_affinity_model`.

Optional error traits executors implement on their errors (duck-typed in Go; make them a `trait ExecError`): `status_code() -> i32`, `retry_after() -> Option<Duration>`, `is_credential_scoped() -> bool`, `is_request_scoped() -> bool`, `response_body() -> Option<Bytes>`, `headers() -> Option<HeaderMap>`, `direct_response()`. `RequestTerminatedError{status, headers, body}` is a plugin-terminated request (never retried).

---

## 2. End-to-end request algorithm

### 2.1 Handler layer (`sdk/api/handlers/handlers_execution.go`, `handlers_routing.go`, `handlers_stream.go`)

```
fn handle(entry_protocol, model_name, raw_json, stream):
  route_decision = plugin model router (LIST; none by default)
  # providers for model:
  base = parse_thinking_suffix(model).name           # "m(8192)" -> "m"; suffix = trailing "(...)" using LAST "(" and requires ending ")"
  if base == "auto": resolve to GetFirstAvailableModel("") from registry (newest `created`)   # skipped in Home
  providers = registry.GetModelProviders(base)       # providers with count>0 sorted by client count desc, then name asc
  if empty and base != model: providers = registry.GetModelProviders(model)
  if empty and lower(name) != name: retry lowercase
  if providers empty: 400 {"error":{"message":"unknown provider for model <m>","type":"invalid_request_error","code":"model_not_found","param":"model"}}
  providers = entry-protocol adjust: protocol=interactions => put "gemini-interactions" first;
              protocols other than {interactions,openai,openai-response,claude,gemini} drop "gemini-interactions"
  image-only models (gpt-image-*, grok-imagine-image*) => 503 unless the images endpoint
  meta = request metadata; meta[requested_model]=original model name; set reasoning_effort, service_tier, generate
  opts = Options{ stream, original_request, source_format=entry, response_format, headers, query, metadata }
  req = Request{ model: normalized_model (suffix kept), payload }
  interceptors (before auth; plugins; LIST)
  resp = manager.Execute(ctx, providers, req, opts)      # or ExecuteStream / ExecuteCount
  on error: enrichAuthSelectionError (adds "(providers=..., model=...; last upstream error: ...)" to auth_not_found/auth_unavailable,
            leaves model_cooldown untouched); status = error.StatusCode() else 500; Retry-After etc from error.Headers()
```

Streaming handler adds one more failover layer (`handlers_stream.go:495-590`): it reads the first deliverable chunk; if the stream yields an error chunk before any payload and `requests.streaming.bootstrap-retries` (default **0**, forced 0 in Home) remaining, and the error status is 0, 401, 402, 403, 408, 429 or >=500, it calls `manager.ExecuteStream` again (full new pick) and re-reads. If that second call fails with an auth-unavailable error and the original error status was >= 500, the original error is reported. Keep-alive: `requests.streaming.keepalive-seconds` (default 0 = off) emits SSE comment keep-alives before first byte; `nonstream-keepalive-interval` emits blank lines for non-stream responses.

### 2.2 Manager entry (`AUTH/conductor_execution.go:122-320`)

```
fn Execute(ctx, providers, req, opts):
  ctx = with_request_proxy_url(opts.proxy_url)
  (req, opts) = session::enrich(req, opts)              # section 6.5: derives canonical session metadata
  providers = normalize(lowercase, trim, dedupe)        # empty => Error{provider_not_found,"no provider supplied"}
  if HomeEnabled: return execute_home(...)               # LIST
  (default_retry, max_cred, max_wait) = retrySettings()  # request-retry, max-retry-credentials, max-retry-interval(seconds->Duration), all clamped >=0
  retry_model = authSelectionModel(opts, req.model)       # metadata auth_selection_model else req.model
  last_err = None; preferred_upstream = None
  for attempt in 0.. :                                    # attempt == retry round number
    round_attempted = {}                                  # auth ids tried this round, shared via opts tracker
    match execute_mixed_once(ctx, providers, req, opts, max_cred, attempt, default_retry):
      Ok(resp) => return resp
      Err(e) =>
        if e is RequestTerminated or RequestStop: return unwrap(e)         # never retried
        if e carries an "upstream execution attempt" marker: preferred_upstream = e
        last_err = e
        (wait, retry) = shouldRetryAfterError(ctx, opts, e, attempt, providers, retry_model, max_wait, default_retry, round_attempted)
        if !retry: break
        waitForCooldown(ctx, wait, max_wait)?             # ctx cancel => return ctx.err
  if ctx.err: return ctx.err
  last_err = preferred(last_err, preferred_upstream)      # prefer the last REAL upstream error over synthesized "no auth"
  if providers contains "antigravity" and credits fallback applies: try antigravity credits path   # NICHE
  return last_err                                         # if none at all: Error{auth_not_found,"no auth available"}
```

`ExecuteCount` is identical (calls `execute_count_mixed_once`, no credits fallback). `ExecuteStream` has the same outer loop but `attempt` is advanced manually after a successful wait, and a bootstrap failure is returned as a stream (`streamErrorResult`) so the handler can still send headers; see 5.

### 2.3 One round: `execute_mixed_once` (`conductor_execution.go:483-693`)

```
fn execute_mixed_once(ctx, providers, req, opts, max_cred, retry_round, default_retry):
  route_model = authSelectionModel(opts, req.model)
  (exec_model, restore) = (req.model, true) if auth_selection_model != req.model else ("", false)
  opts = ensure metadata requested_model (do not overwrite)
  tried = { ids of auths with effective_request_retry < retry_round }   # round filter, 3.4; round 0 excludes nothing
  attempted = {}
  loop:
    if max_cred > 0 and attempted.len() >= max_cred:
        return last_err (preferred over upstream_err) or Error{auth_not_found,"no auth available"}
    (auth, executor, provider) = pick_next_mixed(ctx, providers, route_model, opts, tried)   # section 3
    on pick error: if last_err.is_some(): return preferred(last_err, upstream_err)           # non-Home: pick failure after >=1 attempt returns the last upstream error
                   else return pick_error
    opts.metadata[selected_auth_id/index] = auth.id/index (publishSelectedAuthMetadata)
    tried.insert(auth.id)
    exec_ctx = ctx with: per-auth RoundTripper (proxy), requested-model alias, fresh "upstream attempt" marker
    (models, pooled, alias_result, routing) = preparedExecutionModelsWithAlias(auth, route_model)   # section 8
    if models.is_empty(): continue                       # every upstream model of this auth is blocked; NOT counted in attempted
    attempted.insert(auth.id)
    auth = prepareRequestAuth(exec_ctx, executor, auth)  # executor RequestAuthPreparer (e.g. mint Meta key); persists
    on prepare error: MarkResult(fail, model=selectionModelKey) ; last_err = e ; continue
    executor = executor.ForAPIKey() if api-key auth and executor supports it
    auth_err = None; did_refresh_401 = false
    for upstream_model in models:                        # normally 1; >1 only for an openai-compat alias pool
       result_model = stateModelForExecution(auth, route_model, upstream_model, pooled)
       exec_req = req with model = upstream_model (or exec_model if restore)
       exec_opts = opts + canonical/parent session ids (from pick), ensure canonical session metadata
       (exec_req, exec_opts) = request-after-auth interceptor (plugins; may Terminate => return RequestTerminated)
       attach resolved model info (API-key model capabilities) to exec_req.metadata
       resp = executor.execute(exec_ctx, auth, exec_req, exec_opts)
       if err:
          if exec_ctx cancelled: return ctx.err
          if tryRefreshAfterUnauthorized(auth, err, did_refresh_401): auth = refreshed; did_refresh_401 = true; resp = executor.execute(...) again   # section 9.4
       result = Result{auth.id, provider, model: result_model, route_model, success: err.is_none(), options: exec_opts}
       if err:
          result.error = resultErrorFromError(err)        # 4.1
          result.retry_after = err.retry_after()
          result.credential_scope = err.is_credential_scoped()
          (action, ok) = matchRequestScopedErrorAction(auth, err, cfg)       # section 7
          applyRequestScopedActionToResult(action, ok, &result)
          if compact-availability-neutral(err): recordAvailabilityNeutralResult  # /responses/compact quirk: no cooldown unless 401/402/403/429/credential-scope/cloudflare/invalid_grant
          else MarkResult(result)
          if ok and action in {stop, stop-and-cooldown}: return RequestStop(err)
          if !ok and (compact-request-fault(err) or isRequestInvalidError(err)): return err          # request fault: no failover
          auth_err = err
          if result.credential_scope: break                # skip remaining pooled models, go to next auth
          continue                                         # next pooled upstream model
       MarkResult(success)
       rewriteForceMappedResponse(resp, alias_result)     # section 8.5
       return resp
    # all models of this auth failed
    last_err = auth_err   (same request-scoped-stop / request-invalid checks as above first)
    continue                                              # next auth in same round
```

Properties to preserve:
1. A round visits each eligible auth at most once (`tried`), bounded by `max-retry-credentials`.
2. Failover within a round is immediate (no sleep). Sleeping happens only between rounds (3.5).
3. Request-invalid errors (client faults) and request-scoped `stop` return at once; they never rotate credentials and (request-scoped) never cool them.
4. 401 handling: refresh once per auth per attempt (`did_refresh_401`), then retry the same auth before failing over.

### 2.4 Pseudo-code of `MarkResult` consumers

After every attempt `MarkResult` (section 4.3) mutates the auth under lock, persists the auth (file store; skipped for config/runtime_only auths and when ctx has `skip_persist`), updates scheduler shards, persists the cooldown store if records changed, projects model suspend/quota state into the registry (`ApplyClientModelProjections`, section 10.4), fires `hook.OnResult`, publishes an error event and calls `selector.OnResult` (session affinity bind/unbind).

---

## 3. Selection

### 3.1 Candidate filtering (`pickNextMixedLegacy`, `AUTH/conductor_selection.go:2041`)

```
candidates = [a for a in m.auths if
    a != nil and !a.disabled
    and (pinned_auth_id == "" or a.id == pinned_auth_id)           # opts.metadata["pinned_auth_id"] (websocket/session pinning)
    and eligibility.allows(a)                                      # see below
    and executorKey(a) in provider_set (canonicalSchedulingProvider)
    and a.id not in tried
    and executor registered for executorKey(a)
    and (route_model == "" or authSupportsRouteModel(a, route_model))]
if candidates.is_empty(): Error{auth_not_found,"no auth available"}
```

- `eligibility`: `required_auth_kind` (ctx, e.g. "apikey"), `credential_policy` (`codex_alpha_search_v1`: provider codex AND (oauth OR api key with attribute `codex_alpha_search=true`)), `disallow_free_auth` (metadata; excludes codex auths with `plan_type=free`).
- `authSupportsRouteModel`: model key = strip thinking suffix; true if `registry.ClientSupportsModel(auth.id, key)` (case-insensitive exact match against the models registered for that client) OR the selection model key for that auth (alias resolved) differs and the client supports that. This is why prefixes and force-model-prefix work: the registry only contains the IDs each auth is allowed to serve (section 10).
- Mixed vs single provider: same code; with several providers the candidate set is the union and the selector is invoked with provider string `"mixed"`.

### 3.2 Availability and priority

```
for c in candidates:
   check_model = selectionModelForAuth(c, route_model)   # prefix stripped, OAuth alias resolved (8.2)
   (blocked, reason, next) = isAuthBlockedForModel(c, check_model, now)
   if !blocked: bucket[priority(c)].push(c)
   else: if reason==Cooldown: cooldown_count++ ; if reason!=Disabled and next>now: earliest=min(earliest,next)
         if hasUnauthorizedAuthFailure(c): unauthorized_count++
if buckets empty:
   if cooldown_count == len(auths) and earliest set: return ModelCooldownError(route_model, provider, earliest-now, cause=latest candidate error)   # HTTP 429 + Retry-After
   if unauthorized_count == len(auths): return TerminalAuthError(auth_unavailable 503, non-retryable)
   return AuthUnavailableError(earliest, now, cause)    # 503 if earliest>now else plain auth_unavailable
chosen = bucket[max priority], sorted by id ascending      # ONLY the highest priority tier that has any available auth
```

`priority(a)` = int attribute `priority` (invalid/absent = 0). Tier chosen = max. Lower tiers are used only when the whole higher tier is blocked or already `tried`. Session affinity gets all tiers (3.7).

`isAuthBlockedForModel(auth, model, now)` returns `(blocked, reason, next_time)`:
1. nil -> blocked/other. `disabled || status==disabled` -> blocked/Disabled, no time.
2. `hasUnauthorizedAuthFailure`: `unavailable && status==error && next_refresh_after zero && (last_error.status==401 || last_error.code=="unauthorized")` -> blocked/Other.
3. access token expiry known and `<= now` -> blocked/Other (token must be refreshed first).
4. `quota.exceeded && quota.reason=="credential_quota" && quota.next_recover_at > now` -> blocked/Cooldown until that time.
5. If `model != ""`:
   - if `model_states` non-empty: among states whose `canonicalModelKey(key) == canonicalModelKey(model)`: any `status==disabled` -> blocked/Disabled; for each, `availabilityBlock(state.unavailable, state.quota.exceeded, state.next_retry_after, state.quota.next_recover_at, now)`; a blocked state with no time returns blocked immediately; else the blocked state with the latest time wins (ties prefer Cooldown). If at least one state matched return that result; **if none matched the model is considered available** (aggregate flags are ignored when per-model states exist).
   - else (no states at all): `availabilityBlock` on the aggregate fields `unavailable/quota.exceeded/next_retry_after/quota.next_recover_at`.
6. `model == ""`: aggregate check, except when states exist and `quota.reason != "credential_quota"` and `!unavailable`, `quota.exceeded` is ignored.

`availabilityBlock(unavailable, quota_exceeded, next_retry, next_recover, now)`:
```
if !unavailable and !quota_exceeded: (false, None, zero)
next = max of {next_retry, next_recover} that are > now
if next set: (true, quota_exceeded ? Cooldown : Other, next)
elif either time non-zero (all in the past): (false)          # expired cooldown => available
else: (true, Other, zero)                                     # flagged unavailable with no deadline => blocked indefinitely
```
(A zero deadline with `unavailable=true` arises from terminal states such as refresh-failure 401 handling; such an auth stays blocked until an Update/refresh clears it.)

`canonicalModelKey(m)` = `parse_suffix(trim(m)).model_name` (or m if empty). Per-model state keys always use this canonical key, so `claude-x(8192)` and `claude-x` share state.

### 3.3 Selectors (`AUTH/selector.go`)

All receive the already-filtered, ID-sorted, highest-tier slice (`Pick(ctx, provider, model, opts, auths)` re-runs the availability filter unless ctx carries `prevalidated`; the Manager path passes prevalidated lists, so ignore the second check in a port).

- **RoundRobinSelector** (default): state `last_picked: map["<provider>:<canonicalModel>"] -> auth_id`, max 4096 keys (when the cap is hit and the key is new the whole map is cleared). Pick = first candidate with `id > last_picked[key]` in sorted order, wrapping to index 0; store the pick's id. Identity-based so that shrinking candidate sets (retries, cooldowns) do not skew rotation. For provider `codex` and a downstream websocket request, candidates are first narrowed to those with `websockets` enabled (attribute `websockets` bool, else metadata), if any exist.
- **WeightedRoundRobinSelector** (`routing.strategy: weighted-round-robin`): drop weight<=0 candidates first; smooth WRR (nginx style). State per key (same key format, but the model part uses the *route* model when provided via ctx so alias pools share one accumulator): `current[id]: i64`, `weights[id]`. Pick: for each candidate `current[id] += w`, `total += w` (saturating i64), pick max `current` (first wins ties in ID order), then `current[picked] -= total`. Accumulators reset only when a credential present in both old and new weight maps has a *changed* weight; shrinking candidate sets keep credits; pruned only past 1024 entries per map. Weight source (`authWeight`): attribute `weight` (parse string; empty => 1; invalid => 0), else metadata `weight` (`credentialweight.ParseValue`), else 1. `credentialweight`: default 1, max 1,000,000, `<=0` normalises to 0 (excluded), `>max` is a validation error, non-integer floats are errors, strings parsed as ints.
- **FillFirstSelector** (`fill-first`): `available[0]` (lowest id in the top tier). Deterministic, burns one account.
- Selector construction (`SVC/service_config.go`): strategy strings accepted case-insensitively: `weighted-round-robin|weightedroundrobin|wrr`, `fill-first|fillfirst|ff`, anything else RR. If `routing.session-affinity` is true the chosen selector becomes the fallback of a `SessionAffinitySelector`. Rebuilt (replaced with `Manager.SetSelector`) on config reload only when the normalised tuple `(strategy, session_affinity, ttl, subagents)` changes; the old selector's `Stop()` is called.

### 3.4 Per-credential retry overrides and round exclusion

`effective_request_retry(auth) = metadata.request_retry if >= 0 else global request-retry (clamped >=0)`. At round r > 0 an auth is excluded (pre-seeded into `tried`) when `effective_request_retry(auth) < r`. Round 0 excludes nothing. So `request-retry: 3` means up to 4 rounds (0..3); a credential with override 0 participates only in round 0. Config overrides arrive as metadata: `request-retry` on keys/groups/compat providers, auth JSON `request_retry`.

### 3.5 Round-to-round retry decision (`shouldRetryAfterErrorWithAttempted`, `conductor_selection.go:1337`)

```
fn should_retry(err, attempt, providers, model, max_wait, ...) -> (wait, bool):
  if status == 200 or isRequestInvalidError(err) or isRequestStopError(err): return (0,false)
  if !isRequestRetryRoundError(err) : return (0,false)
         # = status in {403,408,429,500,502,503,504} OR isTransientTransportError(err)
  if !retryAllowed(attempt, ...): return (0,false)
         # exists auth: not disabled, matches pinned id/eligibility/provider/route-model support,
         #   effective_request_retry(auth) > attempt, and retryRoundAvailabilityForAuth(auth, selection_model) is eligible
  (wait, found) = closestCooldownWait(...)   # min over eligible auths of time until available
  if found:
      if wait > 0 and (max_wait <= 0 or wait > max_wait): return (0,false)    # waiting too long => stop
      return (wait, true)
  if err.retry_after is Some(ra): if ra < 0 or (ra > 0 and (max_wait<=0 or ra>max_wait)) return (0,false) else return (ra, true)
  return (0, true)                                                             # immediate next round
```

`retryRoundAvailabilityForAuth(auth, model, now)` -> (eligible, next_time): available => (true, zero). Blocked with Disabled or no deadline => (false). Blocked by active `credential_quota` => eligible iff `credentialRetryRoundStateEligible(auth.last_error, true)`. Else examine matching per-model states: any blocked state with no deadline, or whose `last_error` status is not retry-round eligible -> (false); otherwise (true, next). If no model-state match: eligible iff aggregate last_error is retry-round eligible. `credentialRetryRoundStateEligible(err, quota_exceeded)`: no error => `quota_exceeded`; else status in `{403,408,429,500,502,503,504}`. Meaning: a credential cooling because of a 401/402/404 or invalid_grant is NOT worth waiting for.

`closestCooldownWait` special case: if the auth was already attempted in the round that just failed with **429** and cooling is enabled for it, its wait is `max(next - now, 10s)` (and 10s if no deadline) so a 429 never triggers a zero-wait next round. Otherwise `next.is_zero() => return (0,true)` immediately; negative wait skipped.

`waitForCooldown(wait, max_wait)`: sleep `wait + rand[0, min(wait/4, 2s, max_wait - wait))` (never beyond `max_wait` when `max_wait > 0`); `wait <= 0` returns instantly; respects ctx cancel. Defaults when unset in config: request-retry 0, max-retry-credentials 0 (= all), max-retry-interval 0 (= never wait); note `config.example.yaml` shows 3 / 0 / 30 but the **code defaults are 0 / 0 / 0**.

### 3.6 Failure at pick time

Pick errors are surfaced per 3.2. `restoreModelCooldownErrorModel` rewrites the model name in `model_cooldown` back to the client's requested model. `warnLogAuthUnavailable` logs; no state change.

### 3.7 Session affinity (`SessionAffinitySelector`, only when `routing.session-affinity: true`)

Wraps the configured selector. Manager gives it candidates from **all** priority tiers (`availableAuthsForSelector` with `allPriorities`), it passes only the **highest** tier to the fallback. An existing binding outranks priority.

```
Pick(provider, model, opts, auths):
  set metadata session_affinity_provider/model
  (explicit_id, explicit_fallback) = extract explicit session ids (headers/body/metadata, 6.2)
  if explicit_id empty: try pickLCP (6.4); if handled return it
  else clear lcp_* metadata; set parent_session_id from explicit_fallback
  (primary, fallback) = explicit ids, else derived: metadata derived_session_id => "derived:<id>", else hash of first system/user/assistant messages (extractMessageHashIDs)
  if primary empty: return fallback_selector.Pick(highest-tier available)
  primary = BoundSessionIdentity(primary)   # >256 bytes => prefix(190 bytes, utf8-safe) + "#" + sha256hex
  metadata[canonical_session_id] = primary
  available_all = availability filter across all tiers ; fallback_auths = highest tier of that
  key = provider::primary::canonicalModel ; fkey = provider::fallback::model (if fallback != primary)
  if cache.GetAndRefresh(key) = id and id in available_all: bind(id); return it
  elif cache hit but unavailable: auth = fallback_selector.Pick(fallback_auths); bind; return
  elif fkey cache hit and in available_all (and (!subagent or subagent_affinity)): bind(id); return    # child/fork inherits parent's credential
  else auth = fallback_selector.Pick(fallback_auths); bind(auth.id)
bind(id): if fkey and !subagent and !fork: cache.SetAliases(id, key, fkey) else cache.Set(key, id)

OnResult(res):   (called from MarkResult)
  skip if error is request-scoped/connection/transient (shouldSkipCredentialCooldown)
  success => cache.Touch(key/fkey -> auth) ; failure => cache.CompareAndDelete(key, auth) (and fkey)   # failover rebinding on next pick
  LCP path: success => matcher.Touch; failure => matcher.RemoveFingerprintsBefore(namespace, fps, auth, generation)
```

`SessionCache` (`AUTH/session_cache.go`): TTL map (`routing.session-affinity-ttl`, default 1h, min 1s when set; invalid/<=0 => 1h), max 65536 entries with LRU eviction, `Get` does not refresh TTL, `GetAndRefresh` does, aliases (max 64 per entry) so a group of keys sharing one auth are invalidated together, periodic cleanup goroutine, `InvalidateAuth(id)` on auth Remove/Update-with-changed-credentials. `session-affinity-subagents` (default true): child sessions (those with a parent id and not fork) bind to the parent's credential; false distributes them via the fallback.

---

## 4. Error classification, cooldown, backoff

### 4.1 Classifying an executor error into `Result.error` (`resultErrorFromError`)

```
result_err = clone(err as *Error) or Error{message: err.to_string()}
if result_err.http_status == 0: result_err.http_status = err.status_code() (0 if none)
match:
  isExplicitModelNotFoundError(err, "")               => code = "model_not_found" (unless already set and not request_scoped)
  isRequestScopedError(err) || isRequestInvalidError  => code = "request_scoped"
  isConnectionLifecycleError(err)                     => code = "connection_lifecycle"
  isTransientTransportError(err)                      => code = "transient_transport"
```

Predicates (all Go; port verbatim):

- **isRequestInvalidError(err)** (client fault, never rotates/cools): true if executor says `is_request_scoped`; false if cloudflare-challenge, invalid_grant, or model-support error; else `clienterror.IsRequestFault(status, err)`:
  - 402 and 429 => false always. 401 with JSON body type `authentication_error` => false. JSON body code `model_not_found|model_not_found_error` => false.
  - JSON body (paths `error.code`, `code`, `response.error.code`, `body.error.code`) lowercased in {`cyber_policy`,`context_length_exceeded`,`message_too_big`,`string_above_max_length`,`invalid_prompt`,`invalid_value`,`unsupported_value`,`invalid_request_error`,`previous_response_not_found`} => true; or type (`error.type`,`type`,`response.error.type`,`body.error.type`) in {`invalid_request`,`invalid_request_error`,`bad_request_error`,`invalid_prompt`} => true.
  - message contains "item with id" + "not found" + "items are not persisted when `store` is set to false" => true.
  - else status in {400, 409, 413, 422} => true.
  - Also re-evaluated against the raw `Error.message` if the error is an `*Error` (to avoid the "code: message" prefix breaking JSON parse).
- **Model support error** (`isModelSupportError`): explicit model-not-found, or status in {400,404,422} and message contains any of `model_not_supported`, `requested model is not supported`, `requested model is unsupported`, `requested model is unavailable`, `model is not supported`, `model not supported`, `unsupported model`, `model unavailable`, `not available for your plan`, `not available for your account`.
- **Cloudflare challenge**: message contains `challenge-platform`, `cf-mitigated`, `cloudflare challenge`, or (`just a moment` and `cloudflare`); only when status < 500 (or 0).
- **invalid_grant**: message (or code) contains `invalid_grant` and status in {0,400,401}.
- **Connection lifecycle** (no cooldown, but credential rotation still allowed): websocket close codes 1000/1001/1006, or (no HTTP status) `context canceled`, `deadline exceeded`, EOF/unexpected EOF, strings `websocket: close 1000|1001|1006`, `unexpected eof`.
- **Transient transport** (no cooldown, retry-round eligible): only when no HTTP status and not ctx cancel/deadline: EOF/UnexpectedEOF, DNS timeout/temporary, any net timeout, errno in {ECONNREFUSED, ECONNRESET, ECONNABORTED, ETIMEDOUT, EHOSTUNREACH, ENETUNREACH, EPIPE}, any `net.OpError`, or message contains: `tls: tls handshake`, `tls handshake timeout`, `wsarecv`, `wsasend`, `a connection attempt failed`, `connection refused`, `connection reset`, `i/o timeout`, `no such host`, `server misbehaving`, `network is unreachable`, `no route to host`, `broken pipe`, `connection aborted`, `use of closed network connection`, `unexpected eof`.
- **Explicit model-not-found** (`isExplicitModelNotFoundError`, used to code results and by `isModelSupportError`): structured JSON body with code/type/error identifiers matching model-not-found shapes (`model_not_found`, `not_found_error` with a message that names the requested model exactly, "does not exist", "no such model", missing-model phrases); the match against the requested model is exact and suffix-tolerant. Port the identifier checks conservatively: treat as model-not-found when status is 404 (or 400) AND the JSON body code is `model_not_found` OR the message names the requested model with "not found|does not exist|unknown model".
- `shouldSkipCredentialCooldown(result_err)`: false if `code=="force_cooldown"`; else `isRequestScopedResultError || isConnectionLifecycleResultError || isTransientTransportResultError` (the `*Result*` variants test `code` first, then message fallback only when no HTTP status).
- `isUnauthorizedError`: status 401, or text contains `status 401` / `401 unauthorized`.

### 4.2 Constants (`conductor_refresh.go:27-41`)

| Name | Value |
|---|---|
| `quotaBackoffBase` | 1 s |
| `quotaBackoffMax` | 30 min |
| `minQuotaCooldownFloor` | 10 s (floor for provider Retry-After on 429) |
| `transientErrorCooldown` | 60 s (overridable by `transient-error-cooldown-seconds`) |
| terminal-auth cooldowns 401/402/403, invalid_grant | 30 min |
| 404 and model-support errors | 12 h (or Retry-After if given) |
| Cloudflare challenge | `nextQuotaCooldown` with 10 s minimum |
| `refreshCheckInterval` | 5 s (loop default; Service passes 15 min) |
| `refreshMaxConcurrency` | 16 workers (`oauth.auth-auto-refresh-workers`) |
| `refreshPendingBackoff` | 1 min |
| `refreshFailureBackoff` | 5 min |
| `invalidGrantBackoffBase/Max` | 1 min / 30 min |
| `refreshIneffectiveBackoff` | 30 s |
| `modelQuotaExceededWindow` (registry) | 5 min |
| cooldown wait jitter cap | 2 s |

Backoff ladder (`nextQuotaCooldown(level, disabled)`): `level<0 -> 0`; disabled => `(0, level)`; `d = 1s << level` (use saturating shift; Go overflows past ~level 33); if `d >= 30min` return `(30min, level)` (level frozen at the cap), else `(d, level+1)`. Sequence 1s, 2s, 4s, ... 1024s (17m4s), then 30m forever.

`quotaCooldownAfterFailure(quota, now)`: if `quota.next_recover_at > now` reuse `(quota.next_recover_at, quota.backoff_level)` (a burst of concurrent failures escalates once per window); else `(now + nextQuotaCooldown(level).0, level')`.

### 4.3 `MarkResult` (`conductor_cooldown.go:743-1032`)

Preamble: `ResultPolicy.apply` hook (plugin/host may rewrite the Result); `model_key = canonicalModelKey(result.model)` (if empty derive from route model via `selectionModelKeyForAuth`). Under `m.mu`: find auth (ignore unknown id); `recordRecentRequest`; `success++/failed++`.

**Success**: if `quota.reason=="credential_quota"` and still in the future -> keep credential cooldown. Else if `model_key != ""`: `state = ensure(model_key)`; reset it (`status=active`, clear unavailable/next_retry/last_error/status_message, clear quota cooldown fields); `updateAggregatedAvailability`; if no model has an error: `auth.last_error=None; status_message=""; status=active`. Else (no model key) `clearAuthStateOnSuccess` (everything cleared).

**Failure with model_key** (usual case), unless `shouldSkipCredentialCooldown(error)`:
```
disable = cooldownDisabledForAuth(auth)  # 4.5 ; forced false when error.code == "force_cooldown"
state = ensure(model_key); state.unavailable = true; state.status = error; state.updated_at = now
prev = state.next_retry_after
state.last_error = auth.last_error = clone(error); state.status_message = auth.status_message = error.message
status = error.http_status
if isModelSupportResultError:   next = disable ? zero : (retry_after>0 ? now+ra : now+12h)
elif cloudflare challenge:      (next, level) = nextCloudflareCooldown(state.quota.backoff_level): >=10s ladder; status_message="cloudflare challenge";
                                state.quota = {exceeded:true, reason:"cloudflare challenge", next_recover_at: next, backoff_level: level}
elif invalid_grant:             next = disable ? zero : now+30m
else match status:
   401|402|403: next = disable ? zero : now+30m
   404:         next = disable ? zero : (ra>0 ? now+ra : now+12h)
   429:
       level = state.quota.backoff_level
       if result.credential_scope: level = (auth.quota.exceeded && auth.quota.reason=="credential_quota") ? auth.quota.backoff_level : 0
       if !disable:
           if retry_after is Some: cooldown = max(ra, 10s); next = now+cooldown                 # level unchanged
           else: quota_for = state.quota (or for credential scope: auth.quota if already credential_quota else {next=zero, level=0});
                 (next, level) = quotaCooldownAfterFailure(quota_for, now)
           credential_next = next
           if state.quota.exceeded and state.quota.next_recover_at > next: next = state.quota.next_recover_at
       state.next_retry_after = next
       state.quota = {exceeded:true, reason:"quota", next_recover_at: next, backoff_level: level}     # only cooldown fields; signals untouched
       if result.credential_scope and !disable:
           for every OTHER model state: unavailable=true, status=error;
               other_next = max(credential_next, other.quota.next_recover_at if exceeded);
               other.next_retry_after = max(other_next, other.next_retry_after if live)  # propagation only extends
               other.quota = {exceeded:true, reason:"credential_quota", next_recover_at: other_next, backoff_level: level}
           auth.unavailable = true
           auth_next = max(credential_next, auth.quota.next_recover_at if already credential_quota)
           auth.quota = {exceeded:true, reason:"credential_quota", next_recover_at: auth_next, backoff_level: level}; auth.next_retry_after = auth_next
   408|500|502|503|504|520..526: next = ra>0 ? now+ra : (cooldown_seconds_cfg<0 ? zero : now+(cooldown_seconds_cfg==0 ? 60s : seconds)) ; disabled => zero ; state.unavailable = !next.is_zero()
   default (any other status incl. 0): same as transient WITHOUT retry-after hint
if disable and next==0 and quota.next_recover_at==0: state.unavailable=false; state.quota.exceeded=false
if error.code=="force_cooldown" and next==0: next = now+60s; unavailable=true
# monotonic: never shorten a live cooldown
if next != 0 and prev > next and prev > now: next = prev
auth.status = error ; updateAggregatedAvailability(auth, now)
```
Failure **without** model_key => `applyAuthFailureState` (same status table applied to the auth-level fields with `status_message` values `unauthorized`, `payment_required` (402 and 403), `not_found`, `quota exhausted`, `transient upstream error`, `request failed`, `cloudflare challenge`, `invalid_grant`; same never-shorten and force_cooldown rules).

`updateAggregatedAvailability(auth, now)`: if active `credential_quota` => `unavailable=true`, return. No model states => clear aggregate. Else `auth.unavailable = all states unavailable (status==disabled, or unavailable with next_retry>now; expired states are cleared in passing)`; `auth.next_retry_after = earliest next_retry if all unavailable else zero`; aggregate quota: any state exceeded => `exceeded=true, reason="quota", next_recover_at=min over exceeded states (or later existing), backoff_level=max`; else if auth-level quota still in future keep it; else clear.

Post-lock: `scheduler.upsertAuthResult`, persist cooldown records if changed (only when `routing.cooldown.save-cooldown-status`), then registry projection: for every model of the client, `ClientModelProjection{model_id, suspended, suspend_reason, quota_exceeded}` computed from auth/model state (`clientModelProjectionForAuth`): suspended if disabled, active credential_quota, state disabled/unavailable/next_retry in future, or no states and `auth.unavailable && next_retry>now`; `quota_exceeded` if the state's quota is exceeded and not expired. Applied with `(client epoch, auth.generation)`; stale ones rejected.

`quota.signals` (`quota_signals.go`): for providers `claude`, `codex`, `devin` only, response headers of the **current** response (from request ctx) are filtered to provider-relevant names (<=64 headers, values <=512 bytes, no control chars, deterministic retention rank) and **replace** `signals` + `observed_at` (never merged, never touched by cooldown writes). Responses with no quota headers leave the previous snapshot. Skipped for count-tokens (`SkipQuotaObservation`). Used by management/UI only.

### 4.4 Credential-scope (`Result.credential_scope`)

Set by the executor via `is_credential_scoped()` on the error. Today: Claude unified 5h/7d limit errors and entitlement errors, Meta rate limit, OpenAI-compat `statusErr.credentialScoped`, Codex `usage_limit_reached` unless `oauth.providers.codex.model-level-cooling`, Claude analogously with `oauth.providers.claude.model-level-cooling`. Effect: cooldown applied to the whole credential (all models) and the inner pooled-model loop `break`s.

### 4.5 Disabling cooldown (`quotaCooldownDisabledForAuthWithConfig`)

Precedence: Home enabled => disabled; auth metadata `disable_cooling` (bool, both spellings); OpenAI-compat provider entry `disable-cooling` (only for compat auths); `cfg.DisableCooling` (`routing.cooldown.disable-cooling`); process-global flag (`SetQuotaCooldownDisabled`, set from config at startup/reload). Config API-key `disable-cooling` flows in as metadata via the synthesizer. When disabled: statuses never set a deadline, `unavailable` stays false (credential is still eligible immediately), but `force_cooldown` request-scoped actions still cool. On config change `clearDisabledCooldownStates` wipes cooldown state for newly-disabled auths.

### 4.6 `ResetQuota(ctx, auth_id)` (management)

Clears aggregate and per-model cooldown/quota state and resumes registry suspension for all the client's models; returns the models touched. Persisted afterwards.

### 4.7 Cooldown persistence (`cooldown_state.go`, only with `routing.cooldown.save-cooldown-status: true`, never in Home)

`CooldownStateStore { load() -> Vec<CooldownStateRecord>; save(Vec<..>) }`. Record `{provider, auth_id, model (""= auth-level), status, next_retry_after, reason, quota, last_error, updated_at}`. File store: one `<authfile>.cds` JSON per auth next to the auth file (`{version, auth_id, provider, updated_at, records[]}`; for auths outside the dir uses `cooldown dir`); Postgres variant exists (LIST). `RestoreCooldownStates` on startup and after config apply: only unexpired records are restored into registered auths; disabled-cooling/disabled auths skipped. Saved after any MarkResult that changes records, on store swap (old store flushed first) and in `ApplyConfigWithCooldownStateStore`.

---

## 5. Streams and failover

`executeStreamMixedOnce` is structurally the same pick/try loop as 2.3 but each attempt runs `executeStreamWithModelPool`:

```
for (idx, exec_model) in models:
   stream = executor.execute_stream(...)                       # returns StreamResult or Err
   on Err: (401 refresh once & retry) ; validateStreamResult (nil/None chunks => Error{empty_stream, retryable})
           MarkResult(failure) immediately (same request-scoped / credential-scope handling as non-stream)
           request-scoped stop => return RequestStop ; request-invalid => return err
           credential_scope => return err (no more pooled models) ; else continue to next pooled model
   bootstrap: readStreamBootstrap(ctx, chunks): consume chunks until the FIRST chunk with non-empty payload; buffer them.
              if a chunk.err arrives first, or the channel closes with no payload ("empty_stream" retryable error), it is a BOOTSTRAP FAILURE:
              401 => refresh+retry once; then MarkResult(failure), drain/discard the stream, and either try next pooled model (idx < last) or return
              a streamBootstrapError (carries upstream headers) so the caller can fail over to the next credential
   success: return wrapStreamResult(buffered, remaining)
```

Failover rule: **only before the first payload chunk is delivered**. After that, errors travel in-band (`chunk.err`), `wrapStreamResult` records a failure `Result` via `MarkResult` (once) and forwards the error chunk; there is no replay on another credential. A clean end records success (skipped for Claude OAuth when request was cancelled by the client). Force-mapped streams run chunks through a `StreamRewriter` that rewrites the `model` field back to the client alias (also line-wise on `data:` SSE lines); a final `Finish()` flushes any pending buffer.

If all credentials fail at bootstrap, `ExecuteStream` returns `Ok(StreamResult)` containing only the error (`streamErrorResult(headers, err)`) when the final error is a `streamBootstrapError`, so response headers from the failed upstream can still be forwarded; otherwise returns `Err`.

Codex-only: `oauth.providers.codex.stream-bootstrap-buffering` makes the Codex executor itself hold handshake/keepalive frames up to 48 frames/1 MiB (or `stream-bootstrap-timeout`) so an `server_is_overloaded` rejection smuggled inside HTTP 200 becomes a bootstrap failure (executor-level; see executor survey).

---

## 6. Session identity (`SVC/session`)

### 6.1 `Enrich(req, opts)` (`identity.go:232`)
Runs at the top of every Execute*: normalises/derives session metadata into `opts.metadata`: `canonical_session_id`, `parent_session_id`, `derived_session_id` (`"ctx:v1:" + sha256` of a canonical root built from caller scope + format-specific normalised messages: system/instructions truncated to 50 runes, first turns) when no explicit identity exists, `caller_scope`. Port the observable contract: explicit IDs win; otherwise derived deterministic ID from the first conversation turns.

### 6.2 Explicit session ID priority (`session/info.go`, `ExtractSessionInfo`)
1. `X-Claude-Code-Session-Id` header; 2. Claude Code `metadata.user_id` (JSON or legacy `..._session_<uuid>` string) - also yields parent session and agent id (subagent detection); 3. `Session-Id`/`Session_id`; 4. `X-Http-Session-Id`; 5. `X-Session-ID`/`X-Session-Affinity`/`X-Slot-Session-Id`; 6. `X-Conversation-Id`/`X-Thread-Id`/`X-Client-Request-Id`; 7. Gemini `cachedContent`; 8. OpenAI `thread_id`; 9. body `session_id`/`sessionId`; 10. `prompt_cache_key` (prefix `pck:`), `conversation.id` (`conv:`), `metadata.user_id` (`user:`); 11. `conversation_id`/`chat_id`; 12. metadata `execution_session_id`. Values pass `NormalizeExplicitID` (printable, bounded) and `BoundSessionIdentity`. `SessionInfo{session_id, parent_session_id, agent_name("main" default), client_type("generic"), caller_scope, is_fork, is_compaction, is_subagent, node_kind}`; self-parent cleared.

### 6.3 Hash fallback
When nothing explicit and no derived ID: hash (sha256 of up to 100-char truncations of first `system`, first `user`, first `assistant` message contents; also Responses `input` shapes) => primary id.

### 6.4 LCP (longest-common-prefix) matcher (`session/lcp.go`, 1700 LoC) - NICHE
Used when no explicit ID: conversation turns are canonicalised per protocol (messages / responses / gemini contents / interactions), each turn fingerprinted (sha256 over role, part kind/mime/size/digest/value; values >16 KiB sparsely sampled 12 KiB head/middle/tail; ISO-8601 timestamps, UUIDs and `<think>` blocks normalised away), then a Merkle-prefix index (`MerklePrefixMatcher`, TTL = affinity TTL, caps 1024 turns/4096 groups/262144 prefixes) maps `(provider|model|callerScope namespace, fingerprint prefix) -> auth id` and session id, detects forks (diverging history from a known prefix) and compaction (overlap >= 2 turns within a 32-turn window). Bind on success, remove on failure (`RemoveFingerprintsBefore(generation)`). Skip in the first port: session affinity with explicit IDs + hash fallback covers Claude Code/Codex/OpenCode/pi.

### 6.5 Subagent detection
`isSubagentSession(primary, fallback)`: a session with a distinct parent id from Claude metadata/agent id is a subagent (affects 3.7 binding).

---

## 7. Request-scoped error rules (`conductor_request_scoped_errors.go`)

`RequestScopedErrorRule { status: int, match: [str], match-regexr: [str], action: str }`. Rules source order (first non-empty wins):
1. auth `metadata.request_scoped_errors` (auth JSON, or synthesizer from config key/group)
2. OAuth auths: `cfg.oauth.request-scoped-errors[provider]`
3. API-key auths: the config entry located by `attributes.config_index` for claude/codex/xai/meta/gemini/interactions; compat auths by compat entry; Vertex keys do not carry rules.

Matching: first rule where `rule.status == error.status_code` and (any `match` substring is contained in the response body, or any regex matches; empty match lists never match) and action normalises to one of:
- `stop`: return error to client, **no cooldown** (marks `result.error.code = request_scoped`)
- `stop-and-cooldown`: return error, cool the credential/model (`code = force_cooldown`)
- `continue`: rotate to next credential without cooling (`request_scoped`)
- `continue-and-cooldown`: rotate and cool (`force_cooldown`)
Body = executor `response_body()` if present, else `*Error.message`, else `err.to_string()`. Config sanitisation lowercases action/channel, drops rules with status<=0, no match/regex, or empty action. Rules are evaluated BEFORE the built-in classification; if they match, `isRequestInvalidError` is skipped. Stop errors are wrapped (`RequestStop`) so the outer loop does not retry rounds.

---

## 8. Model names: prefixes, aliases, pools, force mapping, exclusions

### 8.1 Where each transformation applies

| Concept | Applies to | Config | Listing effect | Routing effect |
|---|---|---|---|---|
| Prefix | any auth with `prefix` (file `prefix`, key/group/compat `prefix`) | `prefix`, `routing.force-model-prefix` | registers `prefix/model` (and `model` unless force) in registry | `rewriteModelForAuth` strips `"<prefix>/"` before alias/upstream |
| OAuth alias | OAuth/file auths per channel | `oauth.model-alias[channel]`, per-auth JSON `model_aliases` | renames (or forks) model IDs shown | alias -> upstream name; state key = upstream |
| API-key alias | `models[]` of key/group/compat | `name`/`alias` | ID shown = alias (or name if alias empty) | alias -> `name` |
| Pool | compat `models[]` with repeated alias | duplicates of same `alias` | one entry | rotate among names, fall through on failure |
| force-mapping | OAuth alias or `models[]` entry | `force-mapping: true` | none | response `model` field rewritten back to alias |
| Excluded models | per key / per OAuth provider / per auth JSON | `excluded-models` | filtered from registered list | unregistered => not routable |
| OAuth settings | per channel | `oauth.settings[channel][]` | sets `context_length`/`max_context_length` | none |
| Thinking suffix `(x)` | any model string | n/a | stripped for matching | preserved onto resolved upstream name unless config name already has a suffix |

### 8.2 Resolution order for an (auth, route_model) pair (`executionModelCandidatesWithAlias`)

```
requested = rewriteModelForAuth(route_model, auth)            # strip "prefix/" if present
alias_result = resolveExecutionAliasResultForRequested(auth, requested):
    if home force-mapping attributes (LIST): ...
    if isConfiguredModelRoutingAuth(auth):                    # api-key auth, OR config-sourced auth with compat_name
        resolveAPIKeyModelAliasWithResult(cfg, auth, requested)
    else:
        resolveOAuthModelAliasWithResult(auth, requested)     # channel from provider+kind; per-auth model_aliases first, then global table
upstream_for_pool = requested (for configured-routing auths) else alias_result.upstream_model
candidates:
    if attribute home_upstream_model (LIST) -> [that]
    elif compat auth and pool = resolveModelAliasPoolFromConfigModels(upstream_for_pool, compat.models) non-empty:
         pool of 1 => it ; pool>1 => rotate(pool, nextModelPoolOffset(key "auth|provider|basemodel", len))   # per-key counter, wraps at 2_147_483_640
    else resolved = applyAPIKeyModelAlias(auth, upstream_for_pool) (api-key auths only; per-auth alias table, else config scan); fallback requested
pooled = candidates.len() > 1
models = candidates filtered by isAuthBlockedForModel(auth, stateModelForExecution(...))   # blocked pool members skipped
```

State key (`stateModelForExecution`): if pooled: the concrete upstream model; else the route model, except when the selection model equals the upstream (then upstream). For OAuth aliases the state key is the upstream model (so `alias->base` shares one cooldown). For API-key auths without a pool the state key is the prefix-stripped client model.

`modelAliasLookupCandidates(requested)`: `[requested, base_without_suffix]` (second only if different); first candidate that matches wins. Matching is case-insensitive. Table construction (`compileOAuthModelAliasTable`): per channel (lowercased) reverse map `alias(lower) -> {upstream name, config alias, force_mapping}`; entries with empty name/alias, or `name == alias` (case-insens) skipped; first entry per alias wins.

`resolveUpstreamModelFromAliasTable` result rules:
- hit on alias with `target == base_requested` (alias maps to the same model): if `!force_mapping` => no mapping; else `{upstream: target+suffix, force: true, original_alias: alias}`.
- otherwise upstream = `target` if target already has a suffix else `target + "(" + request suffix + ")"`; `original_alias = requested` normally, or the configured alias if `force_mapping`.
Per-auth `model_aliases` (attribute JSON) are consulted before the global table, so they override. Channels for `OAuthModelAliasChannel(provider, kind)`: api-key => ""; `gemini` => "" (no OAuth); `vertex`, `claude`, `codex`, `aistudio`, `antigravity`, `kimi`/`kimi-ai`/`kimi.ai`/`kimi.com` (own name), `xai`, `meta`, and any other provider key (plugin) => its own name.

### 8.3 Config model entry matching (API-key / compat)

`resolveModelAliasResultFromConfigModels(requested, models)`: for each lookup candidate, first entry with `alias == candidate` (case-insens): if `name == base requested`: no mapping unless `force-mapping`; else `{upstream: name+suffix, force, original_alias: requested or alias if force}`. `resolveModelAliasPoolFromConfigModels`: all distinct (case-insens) `name`s whose alias matches, in config order; if none, fall back to entries whose *name* matches (returns that name). No config entry => passthrough.

Locating the config entry for an auth (`resolveAPIKeyConfig`): config-sourced auths use `attributes.config_index` when the entry at that index still matches api key/base-url; else first entry matching (key AND base-url [case-insens]) AND prefix AND proxy; else any entry matching key+base; else any entry with same key. Compat: by `config_index` (not disabled) else by name match against `compat_name`/`provider_key`/provider (skipping `disabled`).

### 8.4 Listing: `applyModelPrefixes`, `applyExcludedModels` (`SVC/service_models.go`)

```
register_models_for_auth(auth):                      # called on add/modify and on model-catalog refresh
  if auth.disabled: registry.UnregisterClient(id); return
  provider = lower(auth.provider); compat detection => provider = "openai-compatibility"
  excluded = cfg.oauth.excluded-models[provider] if !apikey   # oauth only
  if attributes.excluded_models non-empty: excluded = split(",")   # synthesizer pre-merged per-account + global; OVERRIDES the global list
  base models by provider (static catalog or config):
     gemini / gemini-interactions: entry.models (config) else catalog gemini ; apikey => excluded = entry.excluded-models
     vertex: catalog vertex else entry.models
     aistudio, antigravity, kimi*, devin: catalog
     claude: entry.models else catalog claude
     codex: apikey => entry.models (or codex-pro catalog if none, with support_configuration_update=false) ; oauth => by attributes.plan_type: pro->codex-pro, plus->codex-plus, team|business|go->codex-team, free->codex-free, else pro   # catalog + WithCodexBuiltins(image models)
     xai, meta: entry.models else catalog
     openai-compat (default branch): compat.models => ModelInfo list (see below); none => only plugin models; none => unregister
  models = applyExcludedModels(models, excluded)         # case-insensitive wildcard on model ID; '*' matches any substring; exact when no '*'
  models = applyOAuthModelAliasForAuth(cfg, provider, kind, attributes, models)   # per-auth aliases first then global (dedup by alias key, per-auth wins)
  models = appendPluginModels(...)                       # LIST
  models = applyOAuthSettings(...)                       # max-context-length overrides
  models = applyModelPrefixes(models, auth.prefix, cfg.force-model-prefix)
  registry.RegisterClient(auth.id, provider_key, models)
  then manager.ReconcileRegistryModelStates(auth.id); scheduler.RefreshSchedulerEntry(auth.id)
```

`applyModelPrefixes(models, prefix, force)`: no prefix => unchanged. Else for each model: emit the base model unless (`force` and `prefix != model.id`); always emit a clone with `id = prefix + "/" + base_id`, `metadata_model_id = base_id`. Dedup by id, order preserved.

`applyOAuthModelAliasEntries(aliases, models)`: forward map `lower(name) -> [alias entries]` (entries with name==alias skipped). For each model: if it has alias entries: keep the original only if any entry has `fork:true`; add one clone per alias (`id = alias`, `metadata_model_id` = original id, `display_name` overridden if given, `name` rewritten via `rewriteModelInfoName` for Gemini-style `models/<id>` names); if no alias was actually added and not fork keep the original. Dedup by lowercase id. So `fork` = expose both; otherwise the alias *replaces* the model in listings.

Config model -> `ModelInfo` (`buildConfiguredModelInfo`): `id = alias or name`, `metadata_model_id = name`, `object="model"`, `created = now`, `owned_by = provider-specific ("openai"/"anthropic"/"google"/compat name)`, `type` = provider type (compat: `"openai-compatibility"`, or `"openai-image"` when `image: true`), `display_name = display-name || (name for key models | alias for compat) || alias`, `user_defined = true` for key models, `context_length = max_context_length = max-context-length` if >0, `is_compat`. `buildConfigModels` dedups by lowercase alias (pool duplicates register once), resolves thinking via `modelconfig.ResolveModelInfo(name, type, thinking)` (static capability of the suffix-free upstream name, explicit `thinking` overrides; `NormalizeThinkingSupport` lowercases levels, `none` => `zero_allowed`, `auto` => `dynamic_allowed`), and copies static `native_capabilities`. Compat models default `thinking.levels = [low, medium, high]` unless declared (not for image models); `input-modalities`/`output-modalities` normalised lowercase; `explicit_*` flags track whether config declared them.

### 8.5 Force mapping response rewrite

`rewriteForceMappedResponse(resp, alias_result)`: if `force_mapping && original_alias != ""`, replace the model field(s) in the JSON payload with `original_alias` (`rewriteModelInResponse`, handles OpenAI `model`, Claude `message.model`, Responses `response.model`, Gemini `modelVersion`). Streams: `StreamRewriter` (SSE aware, buffers partial frames). Without force mapping the upstream's own model name is returned.

### 8.6 Excluded models semantics recap
- API-key auths: per-key/group `excluded-models` (lowercased, deduped) only; global `oauth.excluded-models` never applies.
- OAuth auths: `oauth.excluded-models[provider]` UNION per-auth JSON `excluded_models` (merged by the synthesizer into `attributes.excluded_models`; that attribute fully replaces the global list at registration).
- Wildcards: `*` anywhere; comparison on lowercased trimmed ID; applies to the pre-alias, pre-prefix catalog ID.
- Changing `oauth.excluded-models` triggers an immediate rebuild of auths for affected providers (watcher `DiffOAuthExcludedModelChanges`).

---

## 9. Refresh loop (`conductor_refresh.go`, `auto_refresh_loop.go`)

### 9.1 When an auth needs refresh (`shouldRefresh`)
```
if hasUnauthorizedAuthFailure or hasDisabledInvalidGrantFailure: false
if now < next_refresh_after: false
if runtime implements RefreshEvaluator: return evaluator.should_refresh(now, auth)
last = last_refreshed_at or metadata last_refresh/lastRefresh/last_refreshed_at (time-parsed)
expiry = ExpirationTime()
if preferred_interval (metadata|attributes refresh_interval_seconds/refresh_interval, number=seconds or Go duration string):
     due if expiry <= now or expiry-now <= interval; due if last zero or now-last >= interval
lead = ProviderRefreshLead(provider, runtime)           # runtime.RefreshLead() else registered per-provider factory; None => never auto-refresh
if lead is None: false
if lead <= 0: due only when expiry known and now > expiry
if expiry known: due when expiry - now <= lead
elif last known: due when now - last >= lead
else true
```
Provider leads: claude 4h, codex 24h, antigravity 30m, kimi 5m, xai 5m, meta none, devin none, others none (API-key auths never refresh: `nextRefreshCheckAt` returns unschedule for `AuthKind == apikey`).

### 9.2 Scheduler (`authAutoRefreshLoop`)
Min-heap keyed by next check time; one dispatcher goroutine + `N` workers (default 16, `oauth.auth-auto-refresh-workers`) fed by a bounded job channel. `StartAutoRefresh(ctx, interval)` (Service passes **15 min**; this interval is only the re-check delay for evaluator-based auths, missing executors, and a full job queue). On Register/Update/executor-registration `queueReschedule(id)` recomputes `nextRefreshCheckAt`; Remove unschedules. `handleDueAuth`: recompute; if not due re-heap at next time; if executor missing re-check in `interval`; else `markRefreshPending` (dedupe: a job per id; sets `next_refresh_after = now + 1 min` pending backoff) and enqueue. Worker calls `refreshAuthForRequestAtEpoch`.

### 9.3 `refreshAuthForRequest(id, failed_access_token)`
Per-auth mutex (serialises 401-triggered and background refresh so one refresh_token isn't used twice). Reload auth; abort if registration epoch changed or disabled+invalid_grant; **if the current access token differs from `failed_access_token` another caller already refreshed => return current**. `exec.refresh(clone)`:
- Error: ctx canceled => return. Else classify:
  - disabled && invalid_grant: `status=disabled`, `next_refresh_after=zero`, `status_message="disabled (invalid grant)"`, unschedule.
  - disabled otherwise: `next_refresh_after = now+5m`.
  - no valid access token: `unavailable=true, status=error`; 401 => `next_refresh_after=zero` (terminal, `"unauthorized"`); invalid_grant => `refresh_failures++`, `next_refresh_after = now + min(1m<<(failures-1, shift<=10), 30m)`; else `next_refresh_after = now+5m`, `"token expired"`.
  - access token still valid: keep status; `next_retry = now + 5m` (or invalid_grant backoff), clamped to the token expiry.
  - `last_error = refreshErrorFromError` (`code="unauthorized"` for 401).
- Success: merge via `MergeRefreshedAuth(base, existing, updated)` (preserves concurrent edits; executor runtime preserved), `last_refreshed_at=now`, clear `next_refresh_after/last_error/status_message/unavailable/refresh_failures`, `status=active` if error/empty, clear unauthorized model states; if it STILL needs refresh set `next_refresh_after = now+30s` (anti-spin). Persist via store; failure to persist is logged Warn (restart would hit invalid_grant). Then re-project registry model states.

### 9.4 401 recovery during a request (`tryRefreshAfterUnauthorized`)
Conditions: not already tried this attempt, error not request-scoped, error is 401, auth has a refresh credential (`refresh_token` or `token.refresh_token`). Calls `refreshAuthForRequest(id, failed_access_token)`; on success the request is retried once on the refreshed auth before the failure is recorded.

`ForceRefreshAuth(id)` / `ForceRefreshAll()` serve the management API (results `{id, success, error}` using the same worker pool size).

### 9.5 Persist / Update semantics (`conductor_lifecycle.go`)
- `Register`: normalise metadata keys, validate weights (reject), assign id if empty (uuid), `generation=1`, bump `registration_epoch` (monotonic per id even across Remove), `EnsureIndex`, store clone, rebuild API-key alias tables (unless `WithDeferredAPIKeyModelAliasRebuild` ctx), scheduler upsert, reschedule refresh, persist (skipped by `WithSkipPersist` ctx; skipped for config api-key auths, `runtime_only`, plugin virtual auths, and auths without metadata), fire hook.
- `Update` modes: replace, refresh (merge), prepare (merge); stale registration epoch rejected. Keeps `success/failed/recent_requests`, `index`; if credentials changed (`CredentialsChanged`) and the auth had unauthorized state: reset to active and clear unauthorized model states; active `credential_quota` cooldown is preserved across updates; Meta mints persist synchronously before install.
- `persist` is serialised per id and drops writes whose `(epoch, generation)` is older than the last persisted.
- `Remove(id)`: runtime only (callers delete the file); tombstones the epoch in the scheduler, invalidates session-affinity bindings, unschedules refresh.
- `Load()`: replace all auths from `store.List()`, skipping invalid weights; epochs bumped; tombstones for ids that disappeared.

---

## 10. Model registry (`REG/model_registry.go`)

### 10.1 `ModelInfo` (JSON as served and as stored in models.json)

Public JSON keys: `id`, `object`, `created`, `owned_by`, `type`, `display_name`, `name` (Gemini-style `models/...`), `version`, `description`, `inputTokenLimit`, `outputTokenLimit`, `supportedGenerationMethods`, `context_length`, `max_completion_tokens`, `supported_parameters`, `supportedInputModalities`, `supportedOutputModalities`, `supports_web_search`, `thinking`, `config{override_header{}}`. models.json additionally carries internal keys read via custom unmarshal: `native_capabilities{web_search: bool?}` (tri-state), `support_configuration_update`. Internal-only (not serialised): `metadata_model_id`, `explicit_thinking`, `explicit_input_modalities`, `max_context_length`, `user_defined`, `is_compat`.

`ThinkingSupport` JSON/YAML: `{min, max, zero_allowed|zero-allowed, dynamic_allowed|dynamic-allowed, levels[]}` (JSON snake_case, YAML kebab-case). Budget models use min/max; level models use `levels`.

### 10.2 models.json shape (`REG/models/models.json`, embedded via `go:embed`)
Top-level object with arrays: `claude, gemini, vertex, aistudio, codex-free, codex-team, codex-plus, codex-pro, kimi, antigravity, xai, meta` (+ optional `devin`; the file also has a legacy `gemini-cli` array that the struct ignores). Counts today: claude 18, gemini 14, vertex 21, aistudio 16, codex-free 5, codex-team 9, codex-plus 9, codex-pro 9, kimi 10, antigravity 12, xai 12, meta 5. Example entry (claude): `{"id":"claude-haiku-4-5-20251001","object":"model","created":1759276800,"owned_by":"anthropic","type":"claude","display_name":"Claude 4.5 Haiku","context_length":200000,"max_completion_tokens":64000,"thinking":{"min":1024,"max":128000,"zero_allowed":true},"supportedInputModalities":["text","image"],"supportedOutputModalities":["text"]}`. Validation: no null entry, no empty id, no duplicate id within a section; empty sections only warn. Other embedded catalogs: `codex_client_models.json` (client-facing `/v1/models?client_version=` payload, own updater), `devin_models.json` (own updater).

### 10.3 Updater (`REG/model_updater.go`)
Embedded catalog loaded in `init()` (parse failure only warns). `StartModelsUpdater(ctx)` (once): refresh immediately then every **3 h**. URLs tried in order: `https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json`, `https://models.router-for.me/models.json`; 30 s timeout each; status must be 200; parse + validate; on total failure keep current data. Detect changed providers by comparing JSON (plus native capabilities/config-update flag) per section; mapping: gemini change => `gemini` + `gemini-interactions`; codex tiers => `codex`; kimi => kimi, kimi-ai, kimi.ai, kimi.com. Store replaced regardless; callback `SetModelRefreshCallback` (pending changes buffered until a callback is set) triggers `Service.registerModelRefreshCallback`: re-register models for all non-disabled auths of the changed providers (rebuilds model availability from scratch, dropping registry suspension state). Started from `cmd/server/main.go` (not when `--local-model`; skipped in Home mode; Codex-client and Devin updaters still run).

### 10.4 Registry state and operations
`models: id -> ModelRegistration{info, info_by_provider, count, last_updated, quota_exceeded_clients: client->time, providers: provider->count, suspended_clients: client->reason}`; per client: `client_models` (ordered raw ids, duplicates allowed and counted), `client_model_infos`, `client_providers`, epoch (bumped each RegisterClient), generation. All mutate under one RwLock; `generation` and `registration_epoch` atomics invalidate the per-handler list cache.

- `RegisterClient(client, provider, models)`: lowercases provider; empty/ID-less list => unregister the client. Diffs against the previous registration (added/removed/provider-change) adjusting `count` and `providers` counters; `info` is last-registered, `info_by_provider[provider]` keeps per-provider capability variants; epoch++ and generation reset to 0; hook `OnModelsRegistered` async.
- `UnregisterClient`, `SuspendClientModel/ResumeClientModel(client, model, reason)`, `SetModelQuotaExceeded/Clear...`, `ClientSupportsModel` (case-insens), `IsModelSuspendedForClient`, `GetModelProviders(model)` (above), `GetModelCount`, `GetModelInfo(model, provider)` (provider-specific first, else global), `GetModelsAndEpochForClient`, `CleanupExpiredQuotas`.
- `ApplyClientModelProjections(client, epoch, generation, projections)`: rejected if client unknown, `epoch != current`, `generation < current`, or none of the projected models is owned/registered; otherwise sets/clears `suspended_clients[client]=reason` and `quota_exceeded_clients[client]=now` per projection and invalidates caches. This is how conductor cooldown state hides a model from `/v1/models` when ALL its clients are cooling.

### 10.5 Availability (`modelRegistrationAvailability`)
```
available_clients = registration.count
expired_clients  = clients in quota_exceeded_clients with quota_time + 5min > now   # "still within quota window"
suspended: reason == "quota" (case-insens) => cooldown_suspended; other reasons => other_suspended
  quota_and_other = other-suspended clients that are also within the quota window
effective = available_clients - expired_clients - other_suspended + quota_and_other   (clamped >= 0)
available = effective > 0 || (available_clients > 0 && (expired_clients > 0 || cooldown_suspended > 0) && other_suspended == 0)
expires_at = earliest quota-window recovery (cache TTL for list responses)
```
Note the last clause: a model whose only problem is quota/cooldown suspension stays LISTED (only hard-suspended models disappear); `expires_at` makes the cache refresh when a window lapses.

### 10.6 `/v1/models` and sibling list endpoints
Route `GET /v1/models` (`internal/api/server_routes.go:592`, `unifiedModelsHandler`):
1. User-Agent identifies grok shell => Grok-format list.
2. Query has `client_version` => Codex client catalog (`openaiHandler.OpenAIModels` => `codexClientModelsResponse(clientVersion)`, built from `codex_client_models.json` merged with registry models; honours `client.codex.enable-apply-patch`).
3. Home mode => Home list (LIST).
4. `Anthropic-Version` header present OR User-Agent starts with `claude-cli` => Claude format; else OpenAI format.

`GetAvailableModels(handlerType)`: iterate `models`, keep those where `modelRegistrationAvailability` is true, convert each `ModelInfo` to a map per handler type, **unordered** (Go map iteration; sort by `id` for deterministic Rust output, clients do not rely on order), cached per handler type until `expires_at`.
- `openai`: `{id, object:"model", owned_by, [created>0], [type], [display_name], [version], [description], [context_length], [max_context_length], [max_completion_tokens], [supported_parameters]}`; the HTTP handler then reduces each entry to **only** `id, object, created, owned_by` and wraps `{"object":"list","data":[...]}`.
- `claude`: `{id, object:"model", owned_by, created_at(RFC3339 from created, when >0), type:"model", display_name (fallback id), max_input_tokens (context_length or 200000), max_tokens (max_completion_tokens or 64000)}` (the handler then builds the Anthropic list shape via `claudemodels.BuildResponse`, optionally un-cloaking IDs unless `oauth.providers.claude.claude-code.disable-cloaking-model-list`).
- `gemini` (`GET /v1beta/models`): `{name (name or id), version, displayName, description, inputTokenLimit, outputTokenLimit, supportedGenerationMethods, supportedInputModalities, supportedOutputModalities}`.
- default: `{id, object, [owned_by], [type], [created]}`.
So the served list = union over all registered auth clients of each client's (excluded-filtered, alias-renamed, prefixed) model IDs, minus models whose every client is hard-suspended. Two clients registering the same ID with different providers collapse to one entry (`info` = last registered).

`GetFirstAvailableModel(handler)`: newest `created` among available models with count>0 (used for model `auto`).

---

## 11. Service wiring (`SVC`)

### 11.1 Builder (`builder.go`) and Service.Run (`service_lifecycle.go`)
`NewBuilder().WithConfig(cfg).WithConfigPath(p)...Build()`: requires config and path; validates credential weights; normalises plugins config; creates token provider (file), API-key provider, watcher factory, sdkAuth manager (authenticators: codex, claude, antigravity, kimi x3, xai, devin, meta), access manager, **core manager** `coreauth.NewManager(tokenStore, selector, nil)` with selector from routing state, RoundTripperProvider, `SetConfig(cfg)`, `SetOAuthModelAlias(cfg.oauth.model-alias)`; token store base dir = `auth-dir`; cooldown store = the token store if it provides one.

`Run(ctx)` order (non-Home):
1. `usage.StartDefault`; `ensureAuthDir` (mkdir 0755).
2. `applyRetryConfig` (`SetRetryConfig(request-retry, max-retry-interval s, max-retry-credentials)` + `SetTransientErrorCooldownSeconds`); `configureCooldownStateStore`.
3. `coreManager.Load()` (store.List), `registerConfigAPIKeyAuths` (synthesise + register + model registration), optional `RestoreCooldownStates`, `registerAvailableExecutors` (one executor per provider that has auths, plus baseline set), `StartAutoRefresh(15m)`.
4. Token/API-key providers `Load` (counters only; no real work).
5. Websocket gateway (aistudio), `api.NewServer(...)`, plugin runtime sync, server start goroutine.
6. Pprof/discovery apply; hooks.
7. File watcher: `ensureAuthUpdateQueue` (chan cap 256 + consumer goroutine), `watcher.Start`: watches the config file and auth dir, initial `reloadClients(true,...)`.
8. `registerModelRefreshCallback`.
Shutdown (30 s budget): watcher cancel, `StopAutoRefresh` (also stops selector), watcher stop, ws gateway, auth queue, pprof, discovery, HTTP server stop, plugins, usage stop.

### 11.2 Executor registration (`service_executors.go`)
`provider -> executor`: gemini, gemini-interactions, vertex, aistudio (needs ws gateway; per auth id), antigravity, claude, kimi/kimi-ai/kimi.ai/kimi.com => KimiExecutor, xai => XAIAutoExecutor, codex => CodexAutoExecutor (HTTP or WS), devin, meta, anything else (including `openai-compatible-*`) => OpenAI-compat executor (per provider key). Disabled auths never (re)bind executors. Registration `RegisterExecutor` reschedules refresh for matching auths and closes execution sessions of a replaced executor.

### 11.3 Config reload path (`service_config.go`)
`applyConfigUpdateWithAuthSynthesis(cfg, synthesize)`: validate weights (reject whole update on error), commit (sequence number guards against out-of-order commits), then `applyConfigRuntime` under a mutex: (1) rebuild selector if routing state changed, (2) `applyRetryConfig`, (3) `ApplyConfigWithCooldownStateStore` (swap cooldown store, flushing the old one), `SetConfig` (clones config; clears cooldown for newly cooling-disabled auths; rebuilds API-key alias tables), `SetOAuthModelAlias`, (4) pprof/discovery, (5) server clients (access keys), (6) plugins, (7) `registerAvailableExecutors(forceReplace)`, (8) when invoked by the API (not the watcher) re-synthesise config API-key auths, (9) `RestoreCooldownStates` if enabled. The watcher path passes `synthesize=false` because the watcher itself dispatches auth add/modify/delete updates.

### 11.4 Auth update handling (`handleAuthUpdates`)
Consumes `AuthUpdate{action add|modify|delete, id, auth, revision}`: drop stale (`revision <= processed`), coalesce by id (last wins), then for add/modify: `prepareCoreAuthForModelRegistration` (ensure executor, `Register` or `Update` unless `isStaleCoreAuth` (epoch or generation older)), carry over `created_at`, `last_refreshed_at`, `next_refresh_after` and `model_states` from the existing auth, queue model registration (`registerModelsForAuth` then `ReconcileRegistryModelStates` then scheduler refresh), executed in tasks grouped by phase/category (config API keys vs files). Delete: `UnregisterClient(id)` + `manager.Remove(id)` + close codex/xai websocket sessions. After any batch `RefreshAPIKeyModelAlias()` once.

`ReconcileRegistryModelStates(id)`: for the auth's registered models, preserve active cooldown/quota states, reset stale/expired, migrate legacy alias-keyed states to their target (selection) key, and **prune model_states for models no longer reachable**; re-projects to the registry.

---

## 12. Watcher, synthesizer, diff (`WAT`)

### 12.1 Events
- Watches the config file and `auth-dir` (non-recursive; only direct `*.json`). fsnotify ops: config = Write|Create|Rename; auth json = Create|Write|Remove|Rename.
- Config events: debounce 150 ms, then `reloadConfigIfChanged`: read file, ignore empty, sha256 vs `last_config_hash` (skip if equal), `LoadConfig`, resolve `auth-dir` (tilde expansion; or the store-fixed mirrored dir), diff old vs new (log), reload clients. After success the hash is recomputed from disk (the loader may rewrite the file: bcrypt secret hashing, conflicting-legacy cleanup) and config persisted to remote store if configured.
- Auth file events (serialised by `auth_rescan_mu`): Remove/Rename debounced 1 s per path; after 50 ms stat (with 3 retries of 25 ms for known files) to treat atomic-replace as modify; unchanged content hash => skip; else `addOrUpdateClient` (read, hash, synthesise with `SynthesizeAuthFile`, compute per-path add/modify/delete vs `current_auths`, dispatch) or `removeClient`.
- `reloadClients(rescan_auth, affected_oauth_providers, force_refresh)`: drop `current_auths` of providers whose `oauth.excluded-models` changed (forces re-add), count API keys, optionally rescan files/hashes, call the server reload callback (applies config to the Service), then `refreshAuthState(force)`: snapshot = config-synthesised + file-synthesised + runtime auths; changes made during the scan win; compute add (new id), modify (`force` or not `authEqual` after zeroing volatile fields: created/updated/last_refreshed/next_refresh_after/runtime/quota.next_recover_at), delete (missing id); stamp monotonically increasing per-id `revision`; enqueue to the Service queue (dispatcher coalesces pending updates per id, preserving order, drops those with revision older than the latest).
- `force_auth_refresh` is true when `force-model-prefix`, `oauth.model-alias`, `oauth.settings` or retry config changed (so every auth is re-registered and model lists rebuilt).
- `DispatchRuntimeAuthUpdate` (runtime-only auths such as aistudio channels) and `DispatchPersistedAuthUpdate` (logins/management writes) enter the same revisioned pipeline. Server config-reload requests are debounced 1 s (`serverUpdateDebounce`).

### 12.2 `FileSynthesizer` (`synthesizer/file.go`) for each `*.json` in auth-dir
```
metadata = parse JSON (invalid => skip silently); NormalizeCredentialMetadata; ValidateAuthWeight (error => skip file with warning)
provider = lower(trim(metadata.type)); "gemini" => "gemini-cli" => file ignored (provider "" or gemini-cli returns nothing)
(plugin AuthParser may claim the file first: LIST)
id = path relative to auth-dir (full path if not under it); lowercase on Windows
label = metadata.email else provider
prefix = trim(metadata.prefix, "/"), ignored if it still contains "/"
disabled = metadata.disabled (bool); status = disabled ? disabled : active
Auth{ id, file_name = basename, provider, label, prefix, status, disabled, proxy_url = metadata.proxy_url,
      attributes: {source=path, path=path, source_backend="file"}, metadata, created_at = updated_at = now }
ApplyAuthPriorityMetadata:  metadata.priority (number or integer string) => attributes.priority + file_priority="true"
ApplyAuthWeightMetadata:    metadata.weight => attributes.weight (validated, normalised)
note => attributes.note ; headers{} => attributes["header:Name"]
model_aliases/model-aliases => attributes.model_aliases (sanitised)
excluded_models/excluded-models + cfg.oauth.excluded-models[provider] => ApplyAuthExcludedModelsMeta(kind="oauth")
fingerprint_profile => attribute
kimi*: base_url/domain attributes resolved; codex: plan_type = metadata.plan_type else JWT id_token claim else default
```
(Auth `attributes.auth_kind` is set to `"oauth"` only via `ApplyAuthExcludedModelsMeta`, so file auths always carry it.)

### 12.3 `ConfigSynthesizer` (`synthesizer/config.go`)
One auth per key entry, order: gemini, interactions, claude, codex, xai, meta, openai-compat, vertex. Common fields: `id = "<kind>:<first 12 hex of sha256(kind \0 key \0 base \0 proxy \0 prefix \0 sorted headers)>"` (`StableIDGenerator.Next`; repeated identical tuples get `-1`, `-2` suffixes), `provider`, `label` (`<provider>-apikey`, compat: compat name), `prefix`, `status=active`, `proxy_url`, attributes `source=config:<name>[<12hex>]`, `config_index`, `api_key`, `base_url`, `priority` (only if != 0), `weight` (if set; <=0 stored as 0), `models_hash` (sha256 over normalised models via `modelconfig.Compute*ModelsHash`), `header:*` from `headers`, `auth_kind=apikey`, `excluded_models`/`excluded_models_hash`; metadata: `disable_cooling`, `request_retry` (>=0 only), `request_scoped_errors`. Provider specifics: Claude `rebuild_mid_system_message`, `fingerprint_profile`; Codex/xAI/Meta `websockets`, Codex `codex_alpha_search`, `codex_disable_cloaking`; compat `compat_name`, `provider_key`, one auth per `api-key-entries[]` (id kind `openai-compatibility:<name>`, key includes base+proxy) or one keyless auth when none; disabled providers skipped; Vertex id kind `vertex:apikey`, no request-scoped rules. Entries with empty key AND empty base-url skipped.

### 12.4 `diff` package
Pure functions used for logging and change detection: `BuildConfigChangeDetails(old,new)` (human strings per changed field), `DiffOAuthExcludedModelChanges` (-> affected providers), model hash helpers (`ComputeExcludedModelsHash`: sha256 of sorted lowercase joined list), `ComputeOpenAICompatModelsHash`, `oauth_model_alias`/`oauth_settings`/`oauth_request_scoped_errors` summaries, `BuildAuthChangeDetails(old,new)` (debug only). Only the excluded-models diff affects behaviour.

---

## 13. Stores

### 13.1 `Store` trait (`AUTH/store.go`)
`list() -> Vec<Auth>`, `save(&Auth) -> path`, `delete(id)`. `GetTokenStore()` returns the globally registered store, default `FileTokenStore`. `main.go` picks Postgres / object store / git store / file store from env/flags (the three non-file stores LIST: `internal/store/postgresstore.go`, `objectstore.go`, `gitstore.go`; each mirrors auth JSON and config into a local working dir that the watcher watches, `AuthDir()` returned so the watcher locks to that dir; `PersistConfig/PersistAuthFiles` push changes back).

### 13.2 `FileTokenStore` (`sdk/auth/filestore.go`)
- `set_base_dir(auth-dir)`. `list`: recursive `WalkDir`, `*.json` (case-insens), each parsed via `read_auth_files` (invalid files silently skipped; `type: gemini` skipped; empty skipped). `id = relative path from base dir` (lowercased on Windows). Built Auth: provider = `metadata.type` (default "unknown"), label (`label`, else email, else `project_id`), prefix/proxy_url/disabled as in 12.2, `status`, `created_at = updated_at = file mtime`, attributes `path/source/source_backend=file/email`, custom headers; priority/weight via the helper functions (plugin-parsed path only; the plain path does not apply priority/weight here - the synthesizer does).
- `save(auth)`: normalise keys, validate weight; path = `attributes.path`, else `file_name` (absolute or under base dir), else `id` (absolute or under base dir); a **disabled auth is not created** if the file does not exist unless ctx has `auth_creation_intent` (login/migration). Under a mutex: mkdir 0700; if `storage` present: set `metadata.disabled = auth.disabled`, call `storage.SetMetadata`, `storage.SaveTokenToFile(path)` (provider-specific token struct); else write `json(metadata)` (with `disabled`) - if the file exists and is semantically JSON-equal skip the write, else truncate+write 0600. Afterwards set attributes `path/source/source_backend` and `file_name`.
- `delete(id)`: path as-is if absolute or contains a path separator else under base dir; ENOENT ignored.

### 13.3 Persistence rules recap
Config-sourced API-key auths are never written; `runtime_only`; plugin virtual auths; auths with nil metadata. Watcher-originated updates use `skip_persist` so the on-disk file stays the source of truth (no write-back loop). Token refresh writes the file via `UpdateRefreshedAuth` -> `persist`; the watcher sees the write, hash differs from last, re-synthesises, `authEqual` ignores volatile fields so no spurious modify.

---

## 14. Usage (`SVC/usage`)
In-process pub/sub: `Record` published once per upstream attempt by executors (`PublishRecord(ctx, record)`; request/trace ids filled from ctx or uuid), queued on an unbounded slice and dispatched by one goroutine to registered `Plugin{HandleUsage(ctx, record)}` instances (panic-recovered, named plugins replace by name). Record fields: `request_id, trace_id, provider, base_url, executor_type, model, alias, api_key (client key), session_id, parent_session_id, auth_id, auth_index, access_token_sha256, auth_type, source, reasoning_effort, service_tier (default "default", "auto"), request/response_service_tier, response_model, generate: Option<bool> (None=true), stream, requested_at, latency, ttft, failed, fail{status_code, body}, detail{input, output, reasoning, cached, cache_read, cache_creation, total, token_breakdown, response_service_tier}, response_headers`. Context carriers: requested model alias, reasoning effort, service tier, generate flag, stream flag, request id, trace id.

Token accounting (`accounting.go`): `TokenBreakdown{schema_version=2, quality complete|inconsistent|unclassified, total, input{total, uncached, cache_read, cache_write}, output{total, non_reasoning, reasoning}, unclassified}` with validity invariants (`input.total = uncached+cache_read+cache_write`, `output.total = non_reasoning+reasoning`, `total = input+output+unclassified`, complete => unclassified 0). Provider semantics select the constructor: **subset** (OpenAI/Codex/xAI/Kimi/compat: `input_tokens` includes cache, reasoning included in output), **independent** (Claude/Anthropic: input excludes cache read/write, reasoning separate), **separate_reasoning** (Gemini/Vertex/AI Studio/Antigravity/Interactions: input includes cache, reasoning not in `output`), unknown => unclassified. Inconsistent inputs yield `quality=inconsistent`, total preserved. Port as a pure module with table-driven tests. The aggregation store, `usage-statistics-enabled`, `redis-usage-queue-retention-seconds` (default 60, max 3600) and the management endpoints belong to the redis-queue/management layer (LIST).

`AUTH/error_events.go`: `publishErrorEvent(result, auth)` emits per-failure events for the request-log/management UI (NICHE).

---

## 15. Config schema (v8 layout with legacy aliases)

### 15.1 Loading pipeline (`CFG/config_load.go`, `config_v8.go`)
```
LoadConfig(path):
  read file (missing/empty/invalid + optional(cloud) => empty Config with defaults)
  validateCredentialWeightYAML (flattened v8 view; errors abort)
  defaults pre-set (below); yaml decode through Config.UnmarshalYAML (flattenV8: v8 -> legacy field names; presence wins, incl. false/0/[])
  validateTrustedProxies (each entry exact IP or CIDR, no whitespace)
  credential-concurrency defaults; credential-in-flight validate; codex live-media-relay validate; credential weights validate
  management secret-key: if not bcrypt ($2a$|$2b$|$2y$ prefix) => bcrypt(default cost) and write back (preserving comments) into whichever of management/remote-management exists
  clamps: pprof.addr empty=>127.0.0.1:8316; logs-max-total-size-mb<0=>0; error-logs-max-files<0=>10; redis-usage-queue-retention-seconds <=0 =>60, >3600=>3600; max-retry-credentials<0=>0
  plugins normalise (dir default "plugins") + resolve dir (error only if plugins.enabled)
  sanitisers (drop/normalise entries): gemini/interactions keys (dedupe by key|base|proxy|prefix|headers; drop empty key+base), vertex (drop empty key, dedupe key|base, drop models lacking name or alias), codex/xai (drop entries without base-url), meta (drop empty key or "dca:" prefix; default base https://api.meta.ai/v1), claude (normalise prefix/headers/excluded/cloak/fingerprint), openai-compat (drop without base-url), oauth excluded/alias/settings/request-scoped-errors maps, payload raw rules (invalid JSON dropped)
  NormalizeConfigLayout(non-migrating): remove ONLY legacy fields that conflict with a present v8 field; rewrite file if changed (0600)
```
Common normalisers: `normalizeModelPrefix` (trim spaces and "/", empty if still contains "/"), `NormalizeHeaders` (trim, drop empty key/value), `NormalizeExcludedModels` (lowercase, trim, dedupe, empty => nil).

Pre-set defaults (before decode): `host ""`, `logging-to-file false`, `logs-max-total-size-mb 0`, `error-logs-max-files 10`, `usage-statistics-enabled false`, `redis-usage-queue-retention-seconds 60`, `disable-cooling false`, `save-cooldown-status false`, `transient-error-cooldown-seconds 0`, `disable-image-generation false`, `ws-auth true`, `pprof.enable false`, `pprof.addr 127.0.0.1:8316`, `discovery.enabled false`, `discovery.service-type _ai-gateway._tcp`, `discovery.subtypes [_chat-completions,_responses,_messages,_generate-content,_interactions]`, panel repo `https://github.com/router-for-me/Cli-Proxy-API-Management-Center`, `credential-in-flight` defaults. **Not defaulted in code (Go zero values)**: `port` (0; `cmd/server/main.go` falls back to 8317 for display/start, Home normalises 0 to 8317), `request-retry` 0, `max-retry-credentials` 0, `max-retry-interval` 0, `routing.strategy` "" (=> round-robin), `auth-dir` "" (=> `~/.cli-proxy-api` at use via `ResolveAuthDir`). Port these as `Option`/zero with the same resolution points, NOT the sample file values.

### 15.2 v8 <-> legacy path map (`buildV8Paths`)
Legacy (runtime struct) name -> v8 YAML path. A v8 value present (even false/0/empty) overrides the legacy twin; `config-version` must be integer 8 if present.

| Legacy | v8 |
|---|---|
| `host`, `port`, `trusted-proxies`, `tls`, `commercial-mode`, `discovery` | `server.host/port/trusted-proxies/tls/commercial-mode/discovery` |
| `remote-management` | `management` |
| `api-keys` (list of client keys) | `access.api-keys` |
| `credential-concurrency`, `credential-in-flight` | `credentials.concurrency`, `credentials.in-flight` |
| `force-model-prefix` | `routing.force-model-prefix` |
| `request-retry`, `max-retry-credentials`, `max-retry-interval` | `routing.retry.*` |
| `disable-cooling`, `save-cooldown-status`, `transient-error-cooldown-seconds` | `routing.cooldown.*` |
| `proxy-url`, `passthrough-headers`, `nonstream-keepalive-interval`, `streaming`, `payload` | `requests.*` |
| `auth-dir`, `auth-auto-refresh-workers` | `oauth.auth-dir`, `oauth.auth-auto-refresh-workers` |
| `oauth-model-alias`, `oauth-excluded-models`, `oauth-request-scoped-errors`, `oauth-settings` | `oauth.model-alias`, `oauth.excluded-models`, `oauth.request-scoped-errors`, `oauth.settings` |
| `ws-auth` | `oauth.providers.aistudio.ws-auth` |
| `codex.{disable-codex-cloaking,stream-bootstrap-buffering,stream-bootstrap-timeout,orphan-delegation-compatibility,model-level-cooling,response-steering}` | `upstream.codex.*` (shared by OAuth and API-key credentials) |
| `codex` (rest), `codex-header-defaults` | `oauth.providers.codex`, `...codex.header-defaults` |
| `claude`, `claude-code`, `disable-claude-cloak-mode`, `claude-header-defaults` | `upstream.claude`, `upstream.claude` (`claude-code.disable-cloaking-model-list` -> `upstream.claude.disable-cloaking-model-list`), `upstream.claude.disable-claude-cloak-mode`, `upstream.claude.header-defaults` |
| `antigravity`, `antigravity-signature-cache-enabled`, `antigravity-signature-bypass-strict`, `quota-exceeded.antigravity-credits` | `oauth.providers.antigravity`, `...signature-cache-enabled`, `...signature-bypass-strict`, `...antigravity-credits` |
| `xai`, `devin` | `upstream.xai`, `oauth.providers.devin` |
| `disable-image-generation`, `gpt-image-2-base-model`, `video-result-auth-cache-ttl` | `multimedia.*` |
| `debug`, `logging-to-file`, `logs-max-total-size-mb`, `request-log`, `error-logs-max-files` | `observability.logs.*` |
| `usage-statistics-enabled`, `redis-usage-queue-retention-seconds` | `observability.usage.*` |
| `pprof` | `observability.pprof` |
| `routing` | `routing` (same path both layouts; `routing: null` means defaults) |
| client codex options | `client.codex.*` (also accepted from `oauth.providers.codex.optimize-multi-agent-v2`, `providers.codex...`, `codex...`; canonical wins by presence) |
| historical shared upstream spellings | `oauth.providers.{codex,claude,xai}.<shared field>` (e.g. `oauth.providers.claude.claude-code.disable-cloaking-model-list`) are aliases of the `upstream.*` paths above (`v8SharedPaths`; empty historical containers via `v8SharedStructPaths`); canonical wins by presence, then the historical OAuth field, then the legacy global. Aliases are accepted on load, in management paths/bodies (`ProjectV8ConfigAliases`/`NormalizeV8ConfigAliases`) and rewritten to canonical on migration |
| `plugins` | `plugins` (unchanged) |
| `quota-exceeded.{switch-project,switch-preview-model}` | legacy-only, no v8 twin (compat-only, accepted at root `quota-exceeded`) |

API-key families: legacy `gemini-api-key`, `interactions-api-key`, `vertex-api-key`, `codex-api-key`, `claude-api-key`, `xai-api-key`, `meta-api-key`, `openai-compatibility` <-> v8 `api-keys.<gemini|interactions|vertex|codex|claude|xai|meta|openai-compatibility>`. v8 form is a list of **groups** `{name, base-url, <shared fields>, keys: [..]}`; `expandV8Groups` flattens to one legacy entry per key: group fields allowed = `name, base-url, keys` + shared `priority, prefix, proxy-url, headers, models, excluded-models, disable-cooling, request-retry, request-scoped-errors`; key-level non-null values override the group value (null/absent = inherit; explicit `false`/`0`/`""`/`[]` override; maps/lists replace wholesale); `base-url` inside a key is an error; unknown group field is an error; `keys` must be a list. For `openai-compatibility` the group's `keys` become `api-key-entries` and the group is otherwise kept. The root key `api-keys` is a list of strings (legacy client keys) OR a mapping (v8 upstream groups); when it is a mapping the legacy list is dropped. Reverse (`groupLegacyKeys`): one group per legacy entry named `<provider>-<n>`.

**Migration** = a successful v8 management write (`SaveConfigPreserveComments(..., migrateV8=true)`) which moves all legacy-only fields, sets `config-version: 8`, comments out unknown fields. Plain load only strips legacy fields that conflict with a present v8 field. `ValidateV8Config` (management writes) rejects legacy field names (error "legacy field X is not accepted by v8; use Y"), unknown root sections (allowed roots: `config-version, api-keys, plugins, quota-exceeded, client` + the first segment of every v8 path), unknown API-key provider names, and unknown fields (strict decode). `OAuthOnlyFields`: any setting present under `oauth.providers.*` (v8 path) is tracked and **zeroed for API-key executions** by `cfg.ForAPIKey()` (so OAuth-only options such as `codex-header-defaults`, `ws-auth`, live-media-relay and antigravity never affect API-key routes); the shared `upstream.*` settings are not OAuth-only. A save (v0 or v8) of a document that is already in any v8 layout (`IsV8ConfigLayout`: declared version, a v8-only root or path, historical aliases) writes the latest layout; legacy-only files keep theirs (historical client fields stay at their legacy path). `expandV8Groups` drops the transient `auth_index`/`auth-index` fields of key entries.

### 15.3 YAML reference (v8 layout; Go type, default, notes)

```yaml
config-version: 8                      # int; if present must be 8
server:
  host: ""                             # string, default "" (all interfaces)
  port: 8317                           # int; code zero-value 0, runtime fallback 8317
  trusted-proxies: []                  # [IP|CIDR], validated; restart required
  tls: { enable: false, cert: "", key: "" }
  commercial-mode: false               # bool; disables heavy request logging/middleware
  discovery:                           # NICHE mDNS
    enabled: false
    service-name: ""                   # "" => CPA-<ShortID>
    service-type: "_ai-gateway._tcp"
    subtypes: [_chat-completions, _responses, _messages, _generate-content, _interactions]
    interfaces: { include: [], exclude: [] }
    auth-required: true                # *bool
    advertise-management: false
management:                            # legacy remote-management
  allow-remote: false                  # bool
  secret-key: ""                       # "" disables management API (404); plaintext hashed with bcrypt at load and written back
  disable-control-panel: false
  disable-auto-update-panel: false
  base-url: ""                         # TUI client mode only
  panel-github-repository: "https://github.com/router-for-me/Cli-Proxy-API-Management-Center"
access:
  api-keys: []                         # [string] client keys for the proxy API (Home forces nil)
credentials:                           # Home-owned; local values ignored in Home (LIST)
  concurrency: {...}                   # lifecycle-config-revision, observation-barrier-revision, cpa-heartbeat-timeout 3s, cpa-cancel-bound 5s, reclaim-grace 5s, cleanup-interval 5s, release-flush-interval 250ms, release-max-backoff 2s, busy-retry-min 250ms, busy-retry-max 1s, max-limit 1000000
  in-flight: { snapshot-interval: 2s, stale-after: 10s, max-part-bytes: 262144, max-part-count: 64, max-revision-bytes: 16777216, max-aggregate-groups: 100000, max-details: 10000, max-string-bytes: 256, staging-retention: 1m }   # stale-after >= 3 x snapshot-interval
routing:
  strategy: round-robin                # round-robin | weighted-round-robin | fill-first (aliases wrr, ff, ...); unknown => round-robin
  session-affinity: false
  session-affinity-ttl: "1h"           # Go duration; invalid or <=0 => 1h; <1s => 1s
  session-affinity-subagents: true     # *bool; only read when session-affinity true
  force-model-prefix: false
  retry:
    request-retry: 0                   # example shows 3; code default 0
    max-retry-credentials: 0           # 0 = try all
    max-retry-interval: 0              # seconds; example 30; <=0 never wait
  cooldown:
    disable-cooling: false
    save-cooldown-status: false        # .cds files next to auth files
    transient-error-cooldown-seconds: 0  # 0 => 60s; -1 disables transient cooldowns
requests:
  proxy-url: ""                        # socks5/http/https; per-entry "direct"|"none" bypass
  passthrough-headers: false
  nonstream-keepalive-interval: 0      # seconds
  streaming: { keepalive-seconds: 0, bootstrap-retries: 0 }
  payload:                             # default / default-raw / override / override-raw: [{models:[{name,protocol,from-protocol,headers{},match[],not-match[],exist[],not-exist[]}], params:{path: value}}]; filter: [{models:[..], params:[paths]}]
client:
  codex: { enable-apply-patch: false, optimize-multi-agent-v2: false }
api-keys:                              # v8 upstream groups; see 15.2
  gemini|interactions|vertex|codex|claude|xai|meta:
    - name: <string>
      base-url: <string>               # required for codex/xai (entries without base-url dropped); optional elsewhere
      priority: 0                      # int; larger preferred
      prefix: ""                       # no "/" allowed
      proxy-url: ""
      headers: {Name: value}           # value starting "$" copies that header from the client request (omitted if absent)
      disable-cooling: null            # *bool override
      request-retry: null              # *int; <0 or null inherit
      request-scoped-errors: [{status, match[], match-regexr[], action}]
      models: [{name, alias, display-name, max-context-length, force-mapping, is-compat, thinking{min,max,zero-allowed,dynamic-allowed,levels[]}, support-configuration-update (codex-style only)}]
      excluded-models: []
      keys:
        - api-key: <string>
          weight: 1                    # *int; 1..1000000, <=0 excludes under WRR, >1e6 = load error
          # key-level overrides of any shared field; provider extras:
          #   codex/xai/meta: websockets, alpha-search (codex only), disable-codex-cloaking (codex only)
          #   claude: rebuild-mid-system-message, cloak{mode: auto|always|never, strict-mode, sensitive-words[], cache-user-id}, fingerprint-profile ("" | claude-code-cli | legacy oauth-cli), experimental-cch-signing (deprecated)
  openai-compatibility:
    - name: <string>                   # becomes provider key "openai-compatible-<lower(name)>"
      disabled: false
      prefix: ""
      base-url: <string>               # required
      priority: 0
      support-prompt-cache-key: false
      disable-cooling: null
      request-retry: null
      request-scoped-errors: []
      headers: {}
      keys: [{api-key, weight, proxy-url}]        # legacy name api-key-entries
      models: [{name, alias, display-name, max-context-length, force-mapping, image, input-modalities[], output-modalities[], is-compat, use-max-completion-tokens, thinking{}}]   # repeated alias = pool
oauth:
  auth-dir: "~/.cli-proxy-api"         # "~" expanded; empty => same default
  auth-auto-refresh-workers: 0         # >0 overrides 16
  model-alias: {<channel>: [{name, alias, fork, display-name, force-mapping}]}     # channels: vertex aistudio antigravity claude codex kimi xai meta + plugin keys
  settings: {<channel>: [{name, alias, max-context-length}]}
  excluded-models: {<channel>: [patterns]}
  request-scoped-errors: {<channel>: [rule]}
  providers:
    aistudio: { ws-auth: true }        # legacy top-level ws-auth, default true (Home forces false)
    codex: { response-steering, disable-codex-cloaking, stream-bootstrap-buffering, stream-bootstrap-timeout ("0"|"20s"|seconds|none|off...), orphan-delegation-compatibility, model-level-cooling, live-media-relay{...}, header-defaults{user-agent, beta-features} }
    claude: { model-level-cooling, disable-claude-cloak-mode, claude-code{disable-cloaking-model-list}, header-defaults{user-agent, package-version, runtime-version, os, arch, timeout, timezone, stabilize-device-profile} }
    antigravity: { sensitive-words[], connection-pool{enabled, idle-conn-timeout 30s (cap 210s), max-idle-conns-per-host 2}, antigravity-credits, signature-cache-enabled, signature-bypass-strict }
    xai: { inject-x-search: false }
    devin: { sensitive-words[] }
multimedia:
  disable-image-generation: false      # false | true | "chat" | "passthrough"
  gpt-image-2-base-model: ""           # must start "gpt-"; else gpt-5.4-mini
  video-result-auth-cache-ttl: "3h"
observability:
  logs: { debug: false, logging-to-file: false, logs-max-total-size-mb: 0, error-logs-max-files: 10, request-log: false }
  usage: { usage-statistics-enabled: false, redis-usage-queue-retention-seconds: 60 }
  pprof: { enable: false, addr: "127.0.0.1:8316" }
plugins: { enabled: false, dir: plugins, store-sources: [], store-auth: [...], auth-revision: 0, configs: {<id>: {enabled, priority, ...free-form}} }   # LIST
```

Key-level semantics worth testing: `priority` omitted vs 0 are identical; `weight` is key-level only (not in the shared group field list, so a group-level `weight` is rejected as an unsupported group field); `models: null` inherit vs `models: []` clear; `proxy-url: direct|none` bypasses global proxy; empty `proxy-url` falls back to `requests.proxy-url`.

Runtime clone: `Config.CloneForRuntime()` deep-copies via reflection (including yaml.Node) so manager snapshots are immutable. Home overrides (`forceHomeRuntimeConfig`): api-keys nil, usage stats on, disable-cooling true, save-cooldown false, ws-auth false, remote management off, panel off, plugin store auth cleared.

---

## 16. Rust design notes and hazards

1. **State ownership**: one `Manager` with `RwLock<HashMap<AuthId, Auth>>`; MarkResult is the only writer on the hot path. Hand out `Arc<Auth>` snapshots (clone-on-write) instead of Go's deep clones; keep `generation`/`registration_epoch` semantic for registry projection and persistence ordering.
2. **Time**: model "unset" as `None`; every comparison above treats zero as unset. Use `chrono::DateTime<Utc>`/`Instant` for monotonic waits; persisted records use RFC3339.
3. **Error model**: define `enum ExecError` with trait accessors (`status`, `retry_after`, `credential_scoped`, `request_scoped`, `body`, `headers`, `direct_response`) and a `Marker::UpstreamAttempt` bit; implement `isRequestInvalidError` etc. on `(status, body_text)` pairs so classification is a pure function with table tests (use the lists in 4.1 verbatim).
4. **Streams**: bootstrap = await first non-empty chunk with `tokio::select!` on cancellation; keep the buffered prefix and splice it back before the remaining receiver; the post-bootstrap wrapper task records exactly one `Result` (failure on first error chunk, success on clean end, nothing on client cancel for Claude OAuth).
5. **Cancellation**: every `ctx.Err()` check in Go maps to a `CancellationToken`; client cancel must return immediately and must not mark credentials (connection-lifecycle codes).
6. **Selection determinism**: sort candidates by `id` before every selector; RR is identity-based (`last_picked` id), WRR uses i64 saturating arithmetic.
7. **Defaults that differ from `config.example.yaml`**: `request-retry` 0, `max-retry-interval` 0, `routing.strategy` "", `auth-dir` "", `port` 0. Honour code semantics; optionally ship a sample config separately.
8. **Quiet gotchas**: (a) pick failure after a failed attempt returns the *last upstream error*, not "no auth available"; (b) an expired access token blocks selection until refreshed (so refresh must run before first use after downtime); (c) per-model states never match => model considered available even if aggregate `unavailable`; (d) `attributes.excluded_models`, when present, replaces the global OAuth list; (e) `force-model-prefix` hides unprefixed IDs only for prefixed auths; (f) alias state is keyed by upstream name; (g) `model_cooldown` is HTTP 429 with `Retry-After`, `auth_unavailable` is 503; (h) the sleeper adds jitter bounded by `max-retry-interval`; (i) streaming failover after the first byte does not exist; (j) a request-scoped `stop` rule suppresses all retry rounds.
9. **Suggested crate layout**: `auth-core` (types, classify, cooldown, selectors, manager), `registry` (ModelInfo, registry, catalog+updater), `config` (serde-yaml + v8 flatten via `serde_yaml::Value` rewrite, then typed structs; keep a loss-preserving YAML writer separate and optional), `watcher` (notify crate, debounce 150 ms/1 s/50 ms, sha256 hash gates, revision stamps), `synth`, `filestore`.
10. **Testing priorities (E2E first)**: scripted fake upstream returning 429/401/5xx/stream-bootstrap errors to assert failover order and cooldown windows (`Retry-After` floor 10 s, ladder 1s..30m, never-shorten), plus config fixtures (legacy-only, v8-only, mixed with conflicting fields) round-tripped through the loader.

Out of scope here (LIST only): Home dispatch (`conductor_home*.go`, `home_*`, `service_home.go`, `internal/home`), plugin host (`pluginhost`, `PluginScheduler`, model routers, interceptors, `service_plugins.go`), Redis RESP usage queue (`internal/redisqueue`), mDNS (`discovery_advertiser.go`), pprof server, wsrelay/AI Studio gateway, antigravity credits fallback, git/postgres/object stores, Codex Live media relay, management API handlers.
