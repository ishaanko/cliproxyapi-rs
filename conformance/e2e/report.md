# E2E differential report

- server: `/home/ishaan/box/cliproxyapirust/tmp/cli-proxy-api-go`
- config layout: `legacy`
- goldens digest: `b779f4ea9c32231a`
- scenarios: 437 total, 437 passed, 0 failed

| scenario | result | what | first difference |
|---|---|---|---|
| `chat.claude.json.text` | PASS | chat client -> claude upstream, json, text |  |
| `chat.claude.json.tool` | PASS | chat client -> claude upstream, json, tool |  |
| `chat.claude.json.thinking` | PASS | chat client -> claude upstream, json, thinking |  |
| `chat.claude.stream.text` | PASS | chat client -> claude upstream, stream, text |  |
| `chat.claude.stream.tool` | PASS | chat client -> claude upstream, stream, tool |  |
| `chat.claude.stream.thinking` | PASS | chat client -> claude upstream, stream, thinking |  |
| `chat.codex.json.text` | PASS | chat client -> codex upstream, json, text |  |
| `chat.codex.json.tool` | PASS | chat client -> codex upstream, json, tool |  |
| `chat.codex.json.thinking` | PASS | chat client -> codex upstream, json, thinking |  |
| `chat.codex.stream.text` | PASS | chat client -> codex upstream, stream, text |  |
| `chat.codex.stream.tool` | PASS | chat client -> codex upstream, stream, tool |  |
| `chat.codex.stream.thinking` | PASS | chat client -> codex upstream, stream, thinking |  |
| `chat.gemini.json.text` | PASS | chat client -> gemini upstream, json, text |  |
| `chat.gemini.json.tool` | PASS | chat client -> gemini upstream, json, tool |  |
| `chat.gemini.json.thinking` | PASS | chat client -> gemini upstream, json, thinking |  |
| `chat.gemini.stream.text` | PASS | chat client -> gemini upstream, stream, text |  |
| `chat.gemini.stream.tool` | PASS | chat client -> gemini upstream, stream, tool |  |
| `chat.gemini.stream.thinking` | PASS | chat client -> gemini upstream, stream, thinking |  |
| `chat.compat.json.text` | PASS | chat client -> compat upstream, json, text |  |
| `chat.compat.json.tool` | PASS | chat client -> compat upstream, json, tool |  |
| `chat.compat.json.thinking` | PASS | chat client -> compat upstream, json, thinking |  |
| `chat.compat.stream.text` | PASS | chat client -> compat upstream, stream, text |  |
| `chat.compat.stream.tool` | PASS | chat client -> compat upstream, stream, tool |  |
| `chat.compat.stream.thinking` | PASS | chat client -> compat upstream, stream, thinking |  |
| `completions.claude.json.text` | PASS | completions client -> claude upstream, json, text |  |
| `completions.claude.stream.text` | PASS | completions client -> claude upstream, stream, text |  |
| `completions.codex.json.text` | PASS | completions client -> codex upstream, json, text |  |
| `completions.codex.stream.text` | PASS | completions client -> codex upstream, stream, text |  |
| `completions.gemini.json.text` | PASS | completions client -> gemini upstream, json, text |  |
| `completions.gemini.stream.text` | PASS | completions client -> gemini upstream, stream, text |  |
| `completions.compat.json.text` | PASS | completions client -> compat upstream, json, text |  |
| `completions.compat.stream.text` | PASS | completions client -> compat upstream, stream, text |  |
| `responses.claude.json.text` | PASS | responses client -> claude upstream, json, text |  |
| `responses.claude.json.tool` | PASS | responses client -> claude upstream, json, tool |  |
| `responses.claude.json.thinking` | PASS | responses client -> claude upstream, json, thinking |  |
| `responses.claude.stream.text` | PASS | responses client -> claude upstream, stream, text |  |
| `responses.claude.stream.tool` | PASS | responses client -> claude upstream, stream, tool |  |
| `responses.claude.stream.thinking` | PASS | responses client -> claude upstream, stream, thinking |  |
| `responses.codex.json.text` | PASS | responses client -> codex upstream, json, text |  |
| `responses.codex.json.tool` | PASS | responses client -> codex upstream, json, tool |  |
| `responses.codex.json.thinking` | PASS | responses client -> codex upstream, json, thinking |  |
| `responses.codex.stream.text` | PASS | responses client -> codex upstream, stream, text |  |
| `responses.codex.stream.tool` | PASS | responses client -> codex upstream, stream, tool |  |
| `responses.codex.stream.thinking` | PASS | responses client -> codex upstream, stream, thinking |  |
| `responses.gemini.json.text` | PASS | responses client -> gemini upstream, json, text |  |
| `responses.gemini.json.tool` | PASS | responses client -> gemini upstream, json, tool |  |
| `responses.gemini.json.thinking` | PASS | responses client -> gemini upstream, json, thinking |  |
| `responses.gemini.stream.text` | PASS | responses client -> gemini upstream, stream, text |  |
| `responses.gemini.stream.tool` | PASS | responses client -> gemini upstream, stream, tool |  |
| `responses.gemini.stream.thinking` | PASS | responses client -> gemini upstream, stream, thinking |  |
| `responses.compat.json.text` | PASS | responses client -> compat upstream, json, text |  |
| `responses.compat.json.tool` | PASS | responses client -> compat upstream, json, tool |  |
| `responses.compat.json.thinking` | PASS | responses client -> compat upstream, json, thinking |  |
| `responses.compat.stream.text` | PASS | responses client -> compat upstream, stream, text |  |
| `responses.compat.stream.tool` | PASS | responses client -> compat upstream, stream, tool |  |
| `responses.compat.stream.thinking` | PASS | responses client -> compat upstream, stream, thinking |  |
| `claude.claude.json.text` | PASS | claude client -> claude upstream, json, text |  |
| `claude.claude.json.tool` | PASS | claude client -> claude upstream, json, tool |  |
| `claude.claude.json.thinking` | PASS | claude client -> claude upstream, json, thinking |  |
| `claude.claude.stream.text` | PASS | claude client -> claude upstream, stream, text |  |
| `claude.claude.stream.tool` | PASS | claude client -> claude upstream, stream, tool |  |
| `claude.claude.stream.thinking` | PASS | claude client -> claude upstream, stream, thinking |  |
| `claude.codex.json.text` | PASS | claude client -> codex upstream, json, text |  |
| `claude.codex.json.tool` | PASS | claude client -> codex upstream, json, tool |  |
| `claude.codex.json.thinking` | PASS | claude client -> codex upstream, json, thinking |  |
| `claude.codex.stream.text` | PASS | claude client -> codex upstream, stream, text |  |
| `claude.codex.stream.tool` | PASS | claude client -> codex upstream, stream, tool |  |
| `claude.codex.stream.thinking` | PASS | claude client -> codex upstream, stream, thinking |  |
| `claude.gemini.json.text` | PASS | claude client -> gemini upstream, json, text |  |
| `claude.gemini.json.tool` | PASS | claude client -> gemini upstream, json, tool |  |
| `claude.gemini.json.thinking` | PASS | claude client -> gemini upstream, json, thinking |  |
| `claude.gemini.stream.text` | PASS | claude client -> gemini upstream, stream, text |  |
| `claude.gemini.stream.tool` | PASS | claude client -> gemini upstream, stream, tool |  |
| `claude.gemini.stream.thinking` | PASS | claude client -> gemini upstream, stream, thinking |  |
| `claude.compat.json.text` | PASS | claude client -> compat upstream, json, text |  |
| `claude.compat.json.tool` | PASS | claude client -> compat upstream, json, tool |  |
| `claude.compat.json.thinking` | PASS | claude client -> compat upstream, json, thinking |  |
| `claude.compat.stream.text` | PASS | claude client -> compat upstream, stream, text |  |
| `claude.compat.stream.tool` | PASS | claude client -> compat upstream, stream, tool |  |
| `claude.compat.stream.thinking` | PASS | claude client -> compat upstream, stream, thinking |  |
| `gemini.claude.json.text` | PASS | gemini client -> claude upstream, json, text |  |
| `gemini.claude.json.tool` | PASS | gemini client -> claude upstream, json, tool |  |
| `gemini.claude.json.thinking` | PASS | gemini client -> claude upstream, json, thinking |  |
| `gemini.claude.stream.text` | PASS | gemini client -> claude upstream, stream, text |  |
| `gemini.claude.stream.tool` | PASS | gemini client -> claude upstream, stream, tool |  |
| `gemini.claude.stream.thinking` | PASS | gemini client -> claude upstream, stream, thinking |  |
| `gemini.codex.json.text` | PASS | gemini client -> codex upstream, json, text |  |
| `gemini.codex.json.tool` | PASS | gemini client -> codex upstream, json, tool |  |
| `gemini.codex.json.thinking` | PASS | gemini client -> codex upstream, json, thinking |  |
| `gemini.codex.stream.text` | PASS | gemini client -> codex upstream, stream, text |  |
| `gemini.codex.stream.tool` | PASS | gemini client -> codex upstream, stream, tool |  |
| `gemini.codex.stream.thinking` | PASS | gemini client -> codex upstream, stream, thinking |  |
| `gemini.gemini.json.text` | PASS | gemini client -> gemini upstream, json, text |  |
| `gemini.gemini.json.tool` | PASS | gemini client -> gemini upstream, json, tool |  |
| `gemini.gemini.json.thinking` | PASS | gemini client -> gemini upstream, json, thinking |  |
| `gemini.gemini.stream.text` | PASS | gemini client -> gemini upstream, stream, text |  |
| `gemini.gemini.stream.tool` | PASS | gemini client -> gemini upstream, stream, tool |  |
| `gemini.gemini.stream.thinking` | PASS | gemini client -> gemini upstream, stream, thinking |  |
| `gemini.compat.json.text` | PASS | gemini client -> compat upstream, json, text |  |
| `gemini.compat.json.tool` | PASS | gemini client -> compat upstream, json, tool |  |
| `gemini.compat.json.thinking` | PASS | gemini client -> compat upstream, json, thinking |  |
| `gemini.compat.stream.text` | PASS | gemini client -> compat upstream, stream, text |  |
| `gemini.compat.stream.tool` | PASS | gemini client -> compat upstream, stream, tool |  |
| `gemini.compat.stream.thinking` | PASS | gemini client -> compat upstream, stream, thinking |  |
| `geminisse.claude.stream.text` | PASS | geminisse client -> claude upstream, stream, text |  |
| `geminisse.codex.stream.text` | PASS | geminisse client -> codex upstream, stream, text |  |
| `geminisse.gemini.stream.text` | PASS | geminisse client -> gemini upstream, stream, text |  |
| `geminisse.compat.stream.text` | PASS | geminisse client -> compat upstream, stream, text |  |
| `err.chat.claude.upstream401` | PASS | chat client -> claude upstream: upstream401 |  |
| `err.chat.claude.upstream400` | PASS | chat client -> claude upstream: upstream400 |  |
| `err.chat.claude.429_failover` | PASS | chat client -> claude upstream: 429_failover |  |
| `err.chat.claude.429_all` | PASS | chat client -> claude upstream: 429_all |  |
| `err.chat.claude.500_failover` | PASS | chat client -> claude upstream: 500_failover |  |
| `err.chat.claude.500_all` | PASS | chat client -> claude upstream: 500_all |  |
| `err.chat.claude.stream_500_failover` | PASS | chat client -> claude upstream: stream_500_failover |  |
| `err.chat.claude.stream_mid_error` | PASS | chat client -> claude upstream: stream_mid_error |  |
| `err.chat.claude.stream_cut_abort` | PASS | chat client -> claude upstream: stream_cut_abort |  |
| `err.chat.claude.stream_cut_clean` | PASS | chat client -> claude upstream: stream_cut_clean |  |
| `err.chat.claude.json_cut_abort` | PASS | chat client -> claude upstream: json_cut_abort |  |
| `err.chat.codex.upstream401` | PASS | chat client -> codex upstream: upstream401 |  |
| `err.chat.codex.upstream400` | PASS | chat client -> codex upstream: upstream400 |  |
| `err.chat.codex.429_failover` | PASS | chat client -> codex upstream: 429_failover |  |
| `err.chat.codex.429_all` | PASS | chat client -> codex upstream: 429_all |  |
| `err.chat.codex.500_failover` | PASS | chat client -> codex upstream: 500_failover |  |
| `err.chat.codex.500_all` | PASS | chat client -> codex upstream: 500_all |  |
| `err.chat.codex.stream_500_failover` | PASS | chat client -> codex upstream: stream_500_failover |  |
| `err.chat.codex.stream_mid_error` | PASS | chat client -> codex upstream: stream_mid_error |  |
| `err.chat.codex.stream_cut_abort` | PASS | chat client -> codex upstream: stream_cut_abort |  |
| `err.chat.codex.stream_cut_clean` | PASS | chat client -> codex upstream: stream_cut_clean |  |
| `err.chat.codex.json_cut_abort` | PASS | chat client -> codex upstream: json_cut_abort |  |
| `err.chat.gemini.upstream401` | PASS | chat client -> gemini upstream: upstream401 |  |
| `err.chat.gemini.upstream400` | PASS | chat client -> gemini upstream: upstream400 |  |
| `err.chat.gemini.429_failover` | PASS | chat client -> gemini upstream: 429_failover |  |
| `err.chat.gemini.429_all` | PASS | chat client -> gemini upstream: 429_all |  |
| `err.chat.gemini.500_failover` | PASS | chat client -> gemini upstream: 500_failover |  |
| `err.chat.gemini.500_all` | PASS | chat client -> gemini upstream: 500_all |  |
| `err.chat.gemini.stream_500_failover` | PASS | chat client -> gemini upstream: stream_500_failover |  |
| `err.chat.gemini.stream_mid_error` | PASS | chat client -> gemini upstream: stream_mid_error |  |
| `err.chat.gemini.stream_cut_abort` | PASS | chat client -> gemini upstream: stream_cut_abort |  |
| `err.chat.gemini.stream_cut_clean` | PASS | chat client -> gemini upstream: stream_cut_clean |  |
| `err.chat.gemini.json_cut_abort` | PASS | chat client -> gemini upstream: json_cut_abort |  |
| `err.chat.compat.upstream401` | PASS | chat client -> compat upstream: upstream401 |  |
| `err.chat.compat.upstream400` | PASS | chat client -> compat upstream: upstream400 |  |
| `err.chat.compat.429_failover` | PASS | chat client -> compat upstream: 429_failover |  |
| `err.chat.compat.429_all` | PASS | chat client -> compat upstream: 429_all |  |
| `err.chat.compat.500_failover` | PASS | chat client -> compat upstream: 500_failover |  |
| `err.chat.compat.500_all` | PASS | chat client -> compat upstream: 500_all |  |
| `err.chat.compat.stream_500_failover` | PASS | chat client -> compat upstream: stream_500_failover |  |
| `err.chat.compat.stream_mid_error` | PASS | chat client -> compat upstream: stream_mid_error |  |
| `err.chat.compat.stream_cut_abort` | PASS | chat client -> compat upstream: stream_cut_abort |  |
| `err.chat.compat.stream_cut_clean` | PASS | chat client -> compat upstream: stream_cut_clean |  |
| `err.chat.compat.json_cut_abort` | PASS | chat client -> compat upstream: json_cut_abort |  |
| `err.claude.claude.upstream401` | PASS | claude client -> claude upstream: upstream401 |  |
| `err.claude.claude.429_failover` | PASS | claude client -> claude upstream: 429_failover |  |
| `err.claude.claude.500_all` | PASS | claude client -> claude upstream: 500_all |  |
| `err.claude.claude.500_failover` | PASS | claude client -> claude upstream: 500_failover |  |
| `err.claude.claude.stream_500_failover` | PASS | claude client -> claude upstream: stream_500_failover |  |
| `err.claude.claude.stream_mid_error` | PASS | claude client -> claude upstream: stream_mid_error |  |
| `err.claude.claude.stream_cut_abort` | PASS | claude client -> claude upstream: stream_cut_abort |  |
| `err.claude.codex.upstream401` | PASS | claude client -> codex upstream: upstream401 |  |
| `err.claude.codex.429_failover` | PASS | claude client -> codex upstream: 429_failover |  |
| `err.claude.codex.500_all` | PASS | claude client -> codex upstream: 500_all |  |
| `err.claude.codex.500_failover` | PASS | claude client -> codex upstream: 500_failover |  |
| `err.claude.codex.stream_500_failover` | PASS | claude client -> codex upstream: stream_500_failover |  |
| `err.claude.codex.stream_mid_error` | PASS | claude client -> codex upstream: stream_mid_error |  |
| `err.claude.codex.stream_cut_abort` | PASS | claude client -> codex upstream: stream_cut_abort |  |
| `err.claude.gemini.upstream401` | PASS | claude client -> gemini upstream: upstream401 |  |
| `err.claude.gemini.429_failover` | PASS | claude client -> gemini upstream: 429_failover |  |
| `err.claude.gemini.500_all` | PASS | claude client -> gemini upstream: 500_all |  |
| `err.claude.gemini.500_failover` | PASS | claude client -> gemini upstream: 500_failover |  |
| `err.claude.gemini.stream_500_failover` | PASS | claude client -> gemini upstream: stream_500_failover |  |
| `err.claude.gemini.stream_mid_error` | PASS | claude client -> gemini upstream: stream_mid_error |  |
| `err.claude.gemini.stream_cut_abort` | PASS | claude client -> gemini upstream: stream_cut_abort |  |
| `err.claude.compat.upstream401` | PASS | claude client -> compat upstream: upstream401 |  |
| `err.claude.compat.429_failover` | PASS | claude client -> compat upstream: 429_failover |  |
| `err.claude.compat.500_all` | PASS | claude client -> compat upstream: 500_all |  |
| `err.claude.compat.500_failover` | PASS | claude client -> compat upstream: 500_failover |  |
| `err.claude.compat.stream_500_failover` | PASS | claude client -> compat upstream: stream_500_failover |  |
| `err.claude.compat.stream_mid_error` | PASS | claude client -> compat upstream: stream_mid_error |  |
| `err.claude.compat.stream_cut_abort` | PASS | claude client -> compat upstream: stream_cut_abort |  |
| `err.responses.claude.upstream401` | PASS | responses client -> claude upstream: upstream401 |  |
| `err.responses.claude.429_failover` | PASS | responses client -> claude upstream: 429_failover |  |
| `err.responses.claude.500_all` | PASS | responses client -> claude upstream: 500_all |  |
| `err.responses.claude.500_failover` | PASS | responses client -> claude upstream: 500_failover |  |
| `err.responses.claude.stream_500_failover` | PASS | responses client -> claude upstream: stream_500_failover |  |
| `err.responses.claude.stream_mid_error` | PASS | responses client -> claude upstream: stream_mid_error |  |
| `err.responses.claude.stream_cut_abort` | PASS | responses client -> claude upstream: stream_cut_abort |  |
| `err.responses.codex.upstream401` | PASS | responses client -> codex upstream: upstream401 |  |
| `err.responses.codex.429_failover` | PASS | responses client -> codex upstream: 429_failover |  |
| `err.responses.codex.500_all` | PASS | responses client -> codex upstream: 500_all |  |
| `err.responses.codex.500_failover` | PASS | responses client -> codex upstream: 500_failover |  |
| `err.responses.codex.stream_500_failover` | PASS | responses client -> codex upstream: stream_500_failover |  |
| `err.responses.codex.stream_mid_error` | PASS | responses client -> codex upstream: stream_mid_error |  |
| `err.responses.codex.stream_cut_abort` | PASS | responses client -> codex upstream: stream_cut_abort |  |
| `err.responses.gemini.upstream401` | PASS | responses client -> gemini upstream: upstream401 |  |
| `err.responses.gemini.429_failover` | PASS | responses client -> gemini upstream: 429_failover |  |
| `err.responses.gemini.500_all` | PASS | responses client -> gemini upstream: 500_all |  |
| `err.responses.gemini.500_failover` | PASS | responses client -> gemini upstream: 500_failover |  |
| `err.responses.gemini.stream_500_failover` | PASS | responses client -> gemini upstream: stream_500_failover |  |
| `err.responses.gemini.stream_mid_error` | PASS | responses client -> gemini upstream: stream_mid_error |  |
| `err.responses.gemini.stream_cut_abort` | PASS | responses client -> gemini upstream: stream_cut_abort |  |
| `err.responses.compat.upstream401` | PASS | responses client -> compat upstream: upstream401 |  |
| `err.responses.compat.429_failover` | PASS | responses client -> compat upstream: 429_failover |  |
| `err.responses.compat.500_all` | PASS | responses client -> compat upstream: 500_all |  |
| `err.responses.compat.500_failover` | PASS | responses client -> compat upstream: 500_failover |  |
| `err.responses.compat.stream_500_failover` | PASS | responses client -> compat upstream: stream_500_failover |  |
| `err.responses.compat.stream_mid_error` | PASS | responses client -> compat upstream: stream_mid_error |  |
| `err.responses.compat.stream_cut_abort` | PASS | responses client -> compat upstream: stream_cut_abort |  |
| `err.gemini.claude.upstream401` | PASS | gemini client -> claude upstream: upstream401 |  |
| `err.gemini.claude.429_failover` | PASS | gemini client -> claude upstream: 429_failover |  |
| `err.gemini.claude.500_all` | PASS | gemini client -> claude upstream: 500_all |  |
| `err.gemini.claude.stream_mid_error` | PASS | gemini client -> claude upstream: stream_mid_error |  |
| `err.gemini.claude.stream_cut_abort` | PASS | gemini client -> claude upstream: stream_cut_abort |  |
| `err.gemini.codex.upstream401` | PASS | gemini client -> codex upstream: upstream401 |  |
| `err.gemini.codex.429_failover` | PASS | gemini client -> codex upstream: 429_failover |  |
| `err.gemini.codex.500_all` | PASS | gemini client -> codex upstream: 500_all |  |
| `err.gemini.codex.stream_mid_error` | PASS | gemini client -> codex upstream: stream_mid_error |  |
| `err.gemini.codex.stream_cut_abort` | PASS | gemini client -> codex upstream: stream_cut_abort |  |
| `err.gemini.gemini.upstream401` | PASS | gemini client -> gemini upstream: upstream401 |  |
| `err.gemini.gemini.429_failover` | PASS | gemini client -> gemini upstream: 429_failover |  |
| `err.gemini.gemini.500_all` | PASS | gemini client -> gemini upstream: 500_all |  |
| `err.gemini.gemini.stream_mid_error` | PASS | gemini client -> gemini upstream: stream_mid_error |  |
| `err.gemini.gemini.stream_cut_abort` | PASS | gemini client -> gemini upstream: stream_cut_abort |  |
| `err.gemini.compat.upstream401` | PASS | gemini client -> compat upstream: upstream401 |  |
| `err.gemini.compat.429_failover` | PASS | gemini client -> compat upstream: 429_failover |  |
| `err.gemini.compat.500_all` | PASS | gemini client -> compat upstream: 500_all |  |
| `err.gemini.compat.stream_mid_error` | PASS | gemini client -> compat upstream: stream_mid_error |  |
| `err.gemini.compat.stream_cut_abort` | PASS | gemini client -> compat upstream: stream_cut_abort |  |
| `route.rr.claude` | PASS | round robin across two keys over four requests |  |
| `route.rr.codex` | PASS | round robin across two keys over four requests |  |
| `route.rr.gemini` | PASS | round robin across two keys over four requests |  |
| `route.rr.compat` | PASS | round robin across two keys over four requests |  |
| `route.fill_first.claude` | PASS | fill-first sticks to one key |  |
| `route.fill_first.compat` | PASS | fill-first sticks to one key |  |
| `route.fill_first.claude_failover` | PASS | fill-first moves on after a 500, then sticks |  |
| `route.wrr.claude` | PASS | weighted round robin 3:1 over eight requests |  |
| `route.wrr.gemini` | PASS | weighted round robin 3:1 over eight requests |  |
| `route.priority.healthy` | PASS | higher priority key serves every request |  |
| `route.priority.fallback` | PASS | lower priority key serves after the preferred key fails |  |
| `route.prefix.optional` | PASS | prefixed key reachable as team/<model>; plain model per force-model-prefix |  |
| `route.prefix.forced` | PASS | prefixed key reachable as team/<model>; plain model per force-model-prefix |  |
| `route.excluded_models` | PASS | excluded models are unroutable and unlisted |  |
| `route.alias.claude` | PASS | configured model alias routes to the upstream name; the plain name is gone |  |
| `route.alias.compat_upstream_name` | PASS | the compat upstream model name is not routable, only its alias |  |
| `route.unknown_model.chat` | PASS | model unknown to the registry |  |
| `route.unknown_model.claude` | PASS | model unknown to the registry |  |
| `route.unknown_model.responses` | PASS | model unknown to the registry |  |
| `route.unknown_model.gemini` | PASS | model unknown to the registry |  |
| `route.thinking_suffix.claude` | PASS | thinking suffix in the model name configures upstream thinking |  |
| `route.thinking_suffix.codex` | PASS | thinking suffix in the model name configures upstream thinking |  |
| `route.thinking_suffix.gemini` | PASS | thinking suffix in the model name configures upstream thinking |  |
| `route.thinking_suffix.compat` | PASS | thinking suffix in the model name configures upstream thinking |  |
| `route.retry.none` | PASS | request-retry 0: one round only, failover within it |  |
| `route.retry.rounds` | PASS | request-retry 3 without transient cooldown: four immediate rounds |  |
| `route.retry.max_credentials` | PASS | max-retry-credentials 1 caps attempts per round |  |
| `route.cooling.disabled_500` | PASS | disable-cooling: failed keys stay eligible |  |
| `route.cooling.disabled_429` | PASS | disable-cooling with 429s |  |
| `route.cooling.500_second_request` | PASS | a 500 cools the key; the next request uses the other key |  |
| `route.cooling.401_second_request` | PASS | a 401 cools the key for a long time |  |
| `route.scoped.stop` | PASS | request-scoped-errors stop rule returns the 429 without failover or cooldown |  |
| `route.scoped.continue_cooldown` | PASS | request-scoped-errors continue-and-cooldown rotates keys on a 400 |  |
| `route.400_context_length` | PASS | context_length_exceeded is a request fault and is returned as is |  |
| `route.upstream_html_500` | PASS | non-JSON upstream error body |  |
| `route.upstream_garbage_200` | PASS | 200 with a body that is not JSON |  |
| `route.bootstrap.off` | PASS | stream fails when every key fails before the first payload |  |
| `route.bootstrap.retry1` | PASS | bootstrap-retries 1 calls the executor again before the first byte |  |
| `route.keepalive.stream` | PASS | keepalive-seconds emits SSE comments while upstream stalls mid-stream |  |
| `route.keepalive.nonstream` | PASS | nonstream-keepalive-interval writes blank lines before the JSON body |  |
| `route.keepalive.nonstream_error` | PASS | an error after a keep-alive newline is delivered with status 200 |  |
| `route.passthrough.off` | PASS | upstream response headers are not forwarded by default |  |
| `route.passthrough.on` | PASS | passthrough-headers forwards filtered upstream headers |  |
| `route.passthrough.on_error` | PASS | passthrough-headers on an upstream error |  |
| `route.headers.custom.claude` | PASS | configured static and client-copied upstream headers |  |
| `route.headers.custom.codex` | PASS | configured static and client-copied upstream headers |  |
| `route.headers.custom.gemini` | PASS | configured static and client-copied upstream headers |  |
| `route.headers.custom.compat` | PASS | configured static and client-copied upstream headers |  |
| `route.client_headers.claude` | PASS | which client headers reach a Claude upstream |  |
| `route.client_headers.codex` | PASS | which client headers reach a Codex upstream |  |
| `route.client_headers.gemini` | PASS | which client headers reach a Gemini upstream |  |
| `route.client_headers.compat` | PASS | which client headers reach an OpenAI-compatible upstream |  |
| `route.compact.claude` | PASS | POST /v1/responses/compact |  |
| `route.count_tokens.claude_dialect.claude` | PASS | Claude count_tokens against each family |  |
| `route.count_tokens.gemini_dialect.claude` | PASS | Gemini countTokens against each family |  |
| `route.compact.codex` | PASS | POST /v1/responses/compact |  |
| `route.count_tokens.claude_dialect.codex` | PASS | Claude count_tokens against each family |  |
| `route.count_tokens.gemini_dialect.codex` | PASS | Gemini countTokens against each family |  |
| `route.compact.gemini` | PASS | POST /v1/responses/compact |  |
| `route.count_tokens.claude_dialect.gemini` | PASS | Claude count_tokens against each family |  |
| `route.count_tokens.gemini_dialect.gemini` | PASS | Gemini countTokens against each family |  |
| `route.compact.compat` | PASS | POST /v1/responses/compact |  |
| `route.count_tokens.claude_dialect.compat` | PASS | Claude count_tokens against each family |  |
| `route.count_tokens.gemini_dialect.compat` | PASS | Gemini countTokens against each family |  |
| `route.images.unsupported_model` | PASS | image generation with a non-image model |  |
| `route.completions.string_prompt_defaults` | PASS | legacy completions without a prompt |  |
| `rich.chat.claude.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> claude |  |
| `rich.chat.claude.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> claude |  |
| `rich.chat.codex.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> codex |  |
| `rich.chat.codex.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> codex |  |
| `rich.chat.gemini.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> gemini |  |
| `rich.chat.gemini.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> gemini |  |
| `rich.chat.compat.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> compat |  |
| `rich.chat.compat.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: chat -> compat |  |
| `rich.responses.claude.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> claude |  |
| `rich.responses.claude.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> claude |  |
| `rich.responses.codex.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> codex |  |
| `rich.responses.codex.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> codex |  |
| `rich.responses.gemini.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> gemini |  |
| `rich.responses.gemini.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> gemini |  |
| `rich.responses.compat.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> compat |  |
| `rich.responses.compat.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: responses -> compat |  |
| `rich.claude.claude.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> claude |  |
| `rich.claude.claude.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> claude |  |
| `rich.claude.codex.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> codex |  |
| `rich.claude.codex.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> codex |  |
| `rich.claude.gemini.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> gemini |  |
| `rich.claude.gemini.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> gemini |  |
| `rich.claude.compat.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> compat |  |
| `rich.claude.compat.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: claude -> compat |  |
| `rich.gemini.claude.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> claude |  |
| `rich.gemini.claude.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> claude |  |
| `rich.gemini.codex.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> codex |  |
| `rich.gemini.codex.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> codex |  |
| `rich.gemini.gemini.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> gemini |  |
| `rich.gemini.gemini.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> gemini |  |
| `rich.gemini.compat.json` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> compat |  |
| `rich.gemini.compat.stream` | PASS | multi-turn request with system prompt, image, tool round trip and sampling options: gemini -> compat |  |
| `interactions.json` | PASS | Interactions request translated to generateContent |  |
| `interactions.stream` | PASS | streaming Interactions request |  |
| `interactions.errors` | PASS | invalid Interactions bodies |  |
| `auth.missing.chat` | PASS | no credentials on chat |  |
| `auth.missing.models` | PASS | no credentials on /v1/models |  |
| `auth.missing.claude` | PASS | no credentials on /v1/messages |  |
| `auth.missing.gemini` | PASS | no credentials on a Gemini route |  |
| `auth.missing.gemini_models` | PASS | no credentials on /v1beta/models |  |
| `auth.invalid.bearer` | PASS | wrong bearer key |  |
| `auth.invalid.x_api_key` | PASS | wrong x-api-key |  |
| `auth.invalid.query_key` | PASS | wrong ?key= |  |
| `auth.invalid.empty_bearer` | PASS | empty bearer token |  |
| `auth.valid.raw_authorization` | PASS | key without the Bearer scheme |  |
| `auth.valid.lowercase_bearer` | PASS | lowercase bearer scheme |  |
| `auth.valid.x_api_key` | PASS | x-api-key on /v1/messages |  |
| `auth.valid.x_goog_api_key` | PASS | x-goog-api-key on a Gemini route |  |
| `auth.valid.query_key` | PASS | ?key= on a Gemini route |  |
| `auth.valid.query_auth_token` | PASS | ?auth_token= on chat |  |
| `auth.valid.second_key` | PASS | the second configured client key |  |
| `auth.precedence.invalid_then_valid` | PASS | an invalid Authorization header does not mask a valid x-api-key |  |
| `auth.options.preflight` | PASS | OPTIONS bypasses auth with 204 |  |
| `auth.open_access` | PASS | no client keys configured: everything is allowed |  |
| `models.openai` | PASS | GET /v1/models, OpenAI shape |  |
| `models.claude_by_version_header` | PASS | Anthropic-Version header selects the Claude shape |  |
| `models.claude_by_user_agent` | PASS | claude-cli User-Agent selects the Claude shape |  |
| `models.codex_client_catalog` | PASS | ?client_version= returns the Codex client catalog |  |
| `models.gemini_list` | PASS | GET /v1beta/models |  |
| `models.gemini_get` | PASS | GET /v1beta/models/<id> |  |
| `models.gemini_get_missing` | PASS | GET /v1beta/models/<unknown> |  |
| `models.trailing_slash` | PASS | GET /v1/models/ redirects |  |
| `models.after_all_keys_unauthorized` | PASS | models of a provider whose keys all got 401 are suspended and unlisted |  |
| `models.after_all_keys_rate_limited` | PASS | quota-limited models stay listed |  |
| `misc.root` | PASS | GET / |  |
| `misc.healthz` | PASS | GET /healthz |  |
| `misc.not_found` | PASS | unknown route with and without credentials |  |
| `misc.method_not_allowed` | PASS | GET on a POST route |  |
| `misc.management_panel` | PASS | GET /management.html with the panel disabled |  |
| `misc.keep_alive_route` | PASS | /keep-alive only exists with a local password |  |
| `misc.oauth_callbacks` | PASS | provider redirect landing routes |  |
| `misc.bad_bodies.chat` | PASS | malformed chat bodies |  |
| `misc.bad_bodies.responses` | PASS | malformed Responses bodies |  |
| `misc.bad_bodies.claude` | PASS | malformed Claude bodies |  |
| `misc.bad_bodies.gemini` | PASS | malformed Gemini requests |  |
| `misc.chat_with_responses_body` | PASS | a Responses-shaped body (input, no messages) on the chat endpoint is converted |  |
| `misc.stream_flag_variants` | PASS | what counts as stream:true per dialect |  |
| `misc.gemini_alt_variants` | PASS | alt query parameter handling for streamGenerateContent |  |
| `misc.safe_mode.example_keys` | PASS | template client keys put the server in safe mode |  |
| `ws.fallback.claude.text` | PASS | one response.create over the client websocket |  |
| `ws.fallback.codex.text` | PASS | one response.create over the client websocket |  |
| `ws.fallback.gemini.text` | PASS | one response.create over the client websocket |  |
| `ws.fallback.compat.text` | PASS | one response.create over the client websocket |  |
| `ws.fallback.codex.thinking` | PASS | reasoning events over the websocket |  |
| `ws.fallback.codex.two_turns` | PASS | response.append merges the previous turn into the next upstream request |  |
| `ws.fallback.codex.tool_roundtrip` | PASS | function call output continues a tool-calling turn |  |
| `ws.fallback.codex.prewarm` | PASS | generate:false is answered locally without an upstream call |  |
| `ws.fallback.client_errors` | PASS | invalid client frames produce error frames and keep the socket open |  |
| `ws.fallback.codex.upstream_400` | PASS | upstream HTTP error before the first event |  |
| `ws.fallback.codex.upstream_401` | PASS | upstream HTTP error before the first event |  |
| `ws.fallback.codex.upstream_429` | PASS | upstream HTTP error before the first event |  |
| `ws.fallback.codex.upstream_500` | PASS | upstream HTTP error before the first event |  |
| `ws.fallback.codex.stream_cut` | PASS | upstream stream ends before response.completed |  |
| `ws.fallback.codex.mid_error` | PASS | upstream failure event after some output |  |
| `ws.upstream.text` | PASS | response.create forwarded over an upstream websocket |  |
| `ws.upstream.thinking` | PASS | reasoning events over both websockets |  |
| `ws.upstream.two_turns` | PASS | second turn continues on the same upstream socket |  |
| `ws.upstream.previous_response_id` | PASS | previous_response_id is passed through to the upstream socket |  |
| `ws.upstream.error_400` | PASS | upstream error frame |  |
| `ws.upstream.error_401` | PASS | upstream error frame |  |
| `ws.upstream.error_429` | PASS | upstream error frame |  |
| `ws.upstream.error_500` | PASS | upstream error frame |  |
| `ws.upstream.mid_error` | PASS | upstream response.failed after some output |  |
| `ws.upstream.cut_abort` | PASS | upstream drops its websocket mid-turn |  |
| `ws.upstream.close_clean` | PASS | upstream closes its websocket cleanly mid-turn |  |
| `ws.upstream.non_codex_model_falls_back` | PASS | a Claude model on a websocket-enabled setup still goes over HTTP upstream |  |
| `ws.handshake.missing_key` | PASS | upgrade without credentials |  |
| `ws.handshake.invalid_key` | PASS | upgrade with a wrong key |  |
| `ws.handshake.query_key` | PASS | key in the query string |  |
| `ws.handshake.codex_backend_path` | PASS | /backend-api/codex/responses alias |  |
| `ws.handshake.turn_state_echo` | PASS | x-codex-turn-state is echoed on the upgrade response |  |
| `ws.handshake.plain_get` | PASS | GET /v1/responses without an upgrade |  |
| `mgmt.auth.missing_key` | PASS | no management key |  |
| `mgmt.auth.invalid_key` | PASS | wrong management key |  |
| `mgmt.auth.raw_authorization` | PASS | management key without Bearer |  |
| `mgmt.auth.x_management_key` | PASS | X-Management-Key header |  |
| `mgmt.auth.client_key_rejected` | PASS | a proxy client key is not a management key |  |
| `mgmt.auth.disabled_without_secret` | PASS | no secret-key: management routes 404 |  |
| `mgmt.auth.ip_ban` | PASS | five failures ban the client even for the right key |  |
| `mgmt.config.v0_get` | PASS | GET /v0/management/config |  |
| `mgmt.config.v8_get` | PASS | GET /v8/management/config |  |
| `mgmt.config.v8_subtree` | PASS | v8 config subtrees by key path |  |
| `mgmt.config.yaml_v0` | PASS | GET /v0/management/config.yaml |  |
| `mgmt.config.yaml_v8` | PASS | GET /v8/management/config.yaml |  |
| `mgmt.config.v8_write` | PASS | PUT, PATCH and DELETE on the v8 config tree |  |
| `mgmt.config.v8_write_errors` | PASS | invalid v8 config writes |  |
| `mgmt.config.yaml_put_invalid` | PASS | invalid PUT /v0/management/config.yaml |  |
| `mgmt.config.scalars` | PASS | v0 scalar settings: GET, PUT, PATCH |  |
| `mgmt.keys.api_keys_crud` | PASS | client api-keys: list, replace, patch, delete; the new key authenticates |  |
| `mgmt.keys.provider_lists` | PASS | provider key lists with their auth-index |  |
| `mgmt.keys.claude_key_edit` | PASS | patch, delete and re-add a claude key |  |
| `mgmt.auth_files.list` | PASS | credential listing (v0 auth-files, v8 credentials, pagination) |  |
| `mgmt.auth_files.lifecycle` | PASS | upload a disabled credential file, inspect, patch and delete it |  |
| `mgmt.auth_files.errors` | PASS | invalid credential file requests |  |
| `mgmt.usage.api_key_usage` | PASS | per-key success/failed counters after mixed requests |  |
| `mgmt.usage.queue` | PASS | usage events popped from the queue |  |
| `mgmt.cooldown.reset` | PASS | cooldown state is visible in credentials; reset clears it |  |
| `mgmt.logs.disabled` | PASS | log endpoints with logging-to-file off |  |
| `mgmt.model_definitions` | PASS | static model definitions per channel |  |
| `mgmt.api_call` | PASS | api-call substitutes $TOKEN$ with the credential and proxies the request |  |
| `mgmt.oauth_status` | PASS | OAuth session endpoints without a started login |  |
| `mgmt.plugins` | PASS | plugin endpoints with plugins disabled |  |
| `mgmt.unknown_route` | PASS | unknown management paths |  |
