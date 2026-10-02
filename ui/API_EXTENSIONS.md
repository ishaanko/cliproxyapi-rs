# Management API extensions

The UI runs against the stock v8 management API. Everything below is additive
and optional: when an endpoint answers 404, 405 or 501 the UI hides the matching
panels and shows "not available" empty states. Nothing else breaks.

All paths are relative to `/v8/management`, use the same management-key auth as
the rest of the group, and return `Cache-Control: no-store`.

Why these exist: v8 only exposes the destructive usage queue
(`/observability/usage/queue` pops events) and per-credential counters. A UI
that polls the queue would steal events from other consumers (Redis protocol
clients, Home). These endpoints are read-only views over the same usage records.

## Shared rules for the two usage endpoints

- **Gate.** Both require `observability.usage.usage-statistics-enabled` (YAML path `observability.usage.usage-statistics-enabled`, the same flag the usage queue plugin checks). When it is false, both answer **404** with `{"error":"usage_statistics_disabled"}`, which the UI treats as "not available".
- **Recording point.** Record at the point the usage record is built, in the same place the usage-queue plugin builds its payload (`redisqueue/plugin.go`, `HandleUsage`), once per usage record. Do this independently of whether the queue is drained or whether the queue retention has expired the item.
- **Memory only.** State lives in process memory and resets on restart.
- **Timestamps** are UTC RFC 3339 (`2026-10-01T22:45:39Z`, fractional seconds allowed).
- **Tokens block** is always `{ "input_tokens", "output_tokens", "reasoning_tokens", "cached_tokens", "total_tokens" }`, all integers (0 when unknown).

## `GET /observability/usage/summary`

Aggregates since process start. No query parameters.

```json
{
  "since": "2026-10-02T05:45:47Z",
  "totals":      { "requests": 900, "failed": 46, "tokens": { ... } },
  "models":      [ { "model": "gpt-5", "requests": 148, "failed": 7, "tokens": { ... } } ],
  "credentials": [ { "auth_index": "2272fbe37d906486", "provider": "codex", "source": "margaret@example.com", "requests": 148, "failed": 5, "tokens": { ... } } ],
  "api_keys":    [ { "api_key": "sk-...", "requests": 460, "failed": 24, "tokens": { ... } } ],
  "hourly":      [ { "hour": "2026-10-01T23:00:00Z", "requests": 31, "failed": 1, "tokens": { ... } } ]
}
```

- `since` is the process start time (equal to the requests endpoint's `started_at`).
- `failed` is the record's `failed` flag; `tokens` is its token block.
- `models[].model` is the client-visible alias, falling back to the upstream model name, then `"unknown"`.
- `credentials[].auth_index` matches `auth_index` of `/credentials` entries; records without one aggregate under `"unknown"`. `provider` and `source` are optional hints from the latest record.
- `api_keys[].api_key` is the raw client key (the usage record already carries it); the UI masks it for display. Records without one aggregate under `"unknown"`.
- `hourly` is exactly 24 buckets, oldest first, zero filled, aligned to the top of each UTC hour, ending with the current hour.
- Arrays other than `hourly` are sorted by `requests` descending.

## `GET /observability/requests`

Non-destructive ring buffer of the most recent usage records, for the live
request views.

Query:

| Name | Meaning |
| --- | --- |
| `limit` | Clamped to 1..1000, default 100. Non-numeric values use the default. |
| `after` | Non-negative integer `seq`. A negative or non-integer value answers `400 {"error":"invalid_after"}`. |

Semantics:

- Without `after`: the **newest** `limit` events, ordered oldest first. `has_more` is false.
- With `after`: the **oldest** `limit` events with `seq > after`, ordered oldest first, so a client can page forward without gaps. `has_more` is true when further events with larger `seq` remain.
- Clients cursor on the **last returned event's `seq`**, not on the envelope `seq`, and keep requesting while `has_more` is true. If `after` is older than the oldest retained event, return what is retained (the client accepts the gap).

Response:

```json
{
  "seq": 1234,
  "started_at": "2026-10-02T05:45:47Z",
  "capacity": 1000,
  "has_more": false,
  "events": [
    {
      "seq": 1233,
      "timestamp": "2026-10-01T22:45:39Z",
      "latency_ms": 4590,
      "ttft_ms": 327,
      "source": "alan@example.com",
      "auth_index": "096b0cc18553e7c6",
      "auth_type": "oauth",
      "provider": "antigravity",
      "executor_type": "antigravity",
      "model": "gemini-3-pro-preview",
      "alias": "",
      "endpoint": "POST /v1/responses",
      "api_key": "sk-...",
      "request_id": "90b380dd",
      "failed": false,
      "stream": false,
      "fail": { "status_code": 0, "body": "" },
      "tokens": { "input_tokens": 6200, "output_tokens": 84, "reasoning_tokens": 0, "cached_tokens": 3400, "total_tokens": 6284 }
    }
  ]
}
```

- Envelope: `seq` is the highest assigned sequence number (0 when empty), `started_at` is the process start time and doubles as an instance id. A client that sees a different `started_at`, or an envelope `seq` below its cursor, discards its buffer and starts over.
- `seq` is a monotonically increasing integer starting at 1 per process. The buffer keeps the newest `capacity` events (1000).
- Event fields are exactly the usage-queue record fields listed above, plus `seq`. Do **not** include `response_headers`, `access_token_sha256`, session ids or any token material. `endpoint` is `"<METHOD> <path>"`.
- `ttft_ms` is 0 for non-streaming requests; `latency_ms` is the full duration. `fail` is `{status_code, body}` of the upstream failure (empty when the request succeeded); `body` is truncated to 2 KiB.
- `request_id` must match the id accepted by `/observability/logs/requests/<id>` so the UI can open the request log from a row.

The UI polls every 2 to 3 seconds with `after`, so responses are tiny.

## `GET /oauth/auth-url?provider=codex&flow=device` (extra query parameter)

The Go server only offers the Codex device-code flow from the CLI
(`-codex-device-login`). Optional parameter on the existing endpoint: when
`flow=device` is given with `provider=codex`, start the device-code flow and
answer with the shape the other device providers already use:

```json
{ "status": "ok", "flow": "device", "url": "https://auth.openai.com/codex/device", "user_code": "ABCD-EFGH", "state": "codex-1790919733797016525", "expires_in": 900 }
```

- `state` must pass `ValidateOAuthState` (non-empty, at most 128 characters, only `[A-Za-z0-9_.-]`, no `/`, `\` or `..`).
- Register the session under provider `codex`, so `/oauth/status`, `/oauth/session` and the "credential saved completes pending sessions of that provider" rule apply unchanged.
- `expires_in` is always present; default 900 seconds when the upstream does not say.
- The UI sends `is_webui=true` as it does for browser flows. A server that ignores `flow` keeps returning the browser flow response (no `flow` field) and the UI renders whatever it receives.

## Dev shim

`ui/dev/shim.ts` implements the first two endpoints on top of the Go server by
draining its usage queue into a ring buffer and aggregates, and serves
`ui/dist` at `/` and `/management.html`. It is the executable reference for the
response shapes above (`bun dev/shim.ts`, `SHIM_DEMO=1` adds synthetic history).
It does not implement the usage-statistics gate.
