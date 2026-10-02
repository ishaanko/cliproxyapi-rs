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

## `GET /observability/usage/summary`

In-memory aggregates since process start. No query parameters.

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

- `tokens` is `{ "input_tokens", "output_tokens", "reasoning_tokens", "cached_tokens", "total_tokens" }`, all integers.
- Counted per usage record, the same records that feed the usage queue (`failed` is the record's `failed` flag; `tokens` is its token block).
- `models[].model` is the client-visible alias, falling back to the upstream model name, then `"unknown"`.
- `credentials[].auth_index` matches `auth_index` of `/credentials` entries. `provider` and `source` are optional hints taken from the latest record.
- `api_keys[].api_key` is the raw client key (the usage record already carries it); the UI masks it for display.
- `hourly` is exactly 24 buckets, oldest first, zero filled, aligned to the top of each UTC hour, ending with the current hour.
- Arrays are sorted by `requests` descending except `hourly`.
- Counting must not depend on the usage queue being drained. If usage statistics are disabled the endpoint may answer 404.

## `GET /observability/requests`

Non-destructive ring buffer of the most recent usage records, for the live
request views.

Query:

| Name | Meaning |
| --- | --- |
| `limit` | Max events returned, 1 to 1000, default 100. |
| `after` | A `seq` value. Only events with `seq > after` are returned, oldest first. Without it, the newest `limit` events are returned, oldest first. |

Response:

```json
{
  "seq": 1234,
  "capacity": 1000,
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
      "model": "gemini-3-pro-preview",
      "alias": "",
      "endpoint": "POST /v1/responses",
      "api_key": "sk-...",
      "request_id": "90b380dd",
      "failed": false,
      "stream": false,
      "tokens": { "input_tokens": 6200, "output_tokens": 84, "cached_tokens": 3400, "reasoning_tokens": 0, "total_tokens": 6284 }
    }
  ]
}
```

- Each event is the existing usage-queue record (same field names) plus a monotonically increasing integer `seq` starting at 1 per process. Extra fields are ignored by the UI.
- `seq` in the envelope is the highest assigned sequence number. If it is lower than the `seq` the client last saw (restart), the client discards its buffer and refetches.
- The buffer holds the newest `capacity` events (1000 suggested). `after` older than the oldest retained event returns what is retained.
- `request_id` should match the id used by `/observability/logs/requests/<id>` so the UI can open the request log file from a row.

The UI polls every 2 to 3 seconds with `after`, so responses are tiny.

## `GET /oauth/auth-url?provider=codex&flow=device` (extra query parameter)

The Go server only offers the Codex device-code flow from the CLI
(`-codex-device-login`). Optional parameter on the existing endpoint: when
`flow=device` is given with `provider=codex`, start the device-code flow and
answer with the shape the other device providers already use:

```json
{ "status": "ok", "flow": "device", "url": "https://auth.openai.com/codex/device", "user_code": "ABCD-EFGH", "state": "...", "expires_in": 900 }
```

Status polling and cancellation reuse `/oauth/status` and `/oauth/session`. A
server that ignores the parameter keeps returning the browser flow response
(no `flow` field), and the UI renders whatever it receives.

## Dev shim

`ui/dev/shim.ts` implements the first two endpoints on top of the Go server by
draining its usage queue into a ring buffer and aggregates, and serves
`ui/dist` at `/` and `/management.html`. It is the executable reference for the
response shapes above (`bun dev/shim.ts`, `SHIM_DEMO=1` adds synthetic history).
