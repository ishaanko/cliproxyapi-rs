# E2E differential report

- server: `target/release/cliproxy`
- config layout: `legacy`
- goldens digest: `8b6a10ae47df576b`
- scenarios: 1061 total, 1061 passed, 0 failed

| scenario | result | what | first difference |
|---|---|---|---|
| `chat.claude.json.text` | PASS | chat client -> claude upstream, json, text |  |
| `chat.claude.json.tool` | PASS | chat client -> claude upstream, json, tool |  |
| `chat.claude.json.thinking` | PASS | chat client -> claude upstream, json, thinking |  |
| `chat.claude.json.parallel` | PASS | chat client -> claude upstream, json, parallel |  |
| `chat.claude.json.mixed` | PASS | chat client -> claude upstream, json, mixed |  |
| `chat.claude.json.length` | PASS | chat client -> claude upstream, json, length |  |
| `chat.claude.json.cached` | PASS | chat client -> claude upstream, json, cached |  |
| `chat.claude.stream.text` | PASS | chat client -> claude upstream, stream, text |  |
| `chat.claude.stream.tool` | PASS | chat client -> claude upstream, stream, tool |  |
| `chat.claude.stream.thinking` | PASS | chat client -> claude upstream, stream, thinking |  |
| `chat.claude.stream.parallel` | PASS | chat client -> claude upstream, stream, parallel |  |
| `chat.claude.stream.mixed` | PASS | chat client -> claude upstream, stream, mixed |  |
| `chat.claude.stream.length` | PASS | chat client -> claude upstream, stream, length |  |
| `chat.claude.stream.cached` | PASS | chat client -> claude upstream, stream, cached |  |
| `chat.codex.json.text` | PASS | chat client -> codex upstream, json, text |  |
| `chat.codex.json.tool` | PASS | chat client -> codex upstream, json, tool |  |
| `chat.codex.json.thinking` | PASS | chat client -> codex upstream, json, thinking |  |
| `chat.codex.json.parallel` | PASS | chat client -> codex upstream, json, parallel |  |
| `chat.codex.json.mixed` | PASS | chat client -> codex upstream, json, mixed |  |
| `chat.codex.json.length` | PASS | chat client -> codex upstream, json, length |  |
| `chat.codex.json.cached` | PASS | chat client -> codex upstream, json, cached |  |
| `chat.codex.stream.text` | PASS | chat client -> codex upstream, stream, text |  |
| `chat.codex.stream.tool` | PASS | chat client -> codex upstream, stream, tool |  |
| `chat.codex.stream.thinking` | PASS | chat client -> codex upstream, stream, thinking |  |
| `chat.codex.stream.parallel` | PASS | chat client -> codex upstream, stream, parallel |  |
| `chat.codex.stream.mixed` | PASS | chat client -> codex upstream, stream, mixed |  |
| `chat.codex.stream.length` | PASS | chat client -> codex upstream, stream, length |  |
| `chat.codex.stream.cached` | PASS | chat client -> codex upstream, stream, cached |  |
| `chat.gemini.json.text` | PASS | chat client -> gemini upstream, json, text |  |
| `chat.gemini.json.tool` | PASS | chat client -> gemini upstream, json, tool |  |
| `chat.gemini.json.thinking` | PASS | chat client -> gemini upstream, json, thinking |  |
| `chat.gemini.json.parallel` | PASS | chat client -> gemini upstream, json, parallel |  |
| `chat.gemini.json.mixed` | PASS | chat client -> gemini upstream, json, mixed |  |
| `chat.gemini.json.length` | PASS | chat client -> gemini upstream, json, length |  |
| `chat.gemini.json.cached` | PASS | chat client -> gemini upstream, json, cached |  |
| `chat.gemini.stream.text` | PASS | chat client -> gemini upstream, stream, text |  |
| `chat.gemini.stream.tool` | PASS | chat client -> gemini upstream, stream, tool |  |
| `chat.gemini.stream.thinking` | PASS | chat client -> gemini upstream, stream, thinking |  |
| `chat.gemini.stream.parallel` | PASS | chat client -> gemini upstream, stream, parallel |  |
| `chat.gemini.stream.mixed` | PASS | chat client -> gemini upstream, stream, mixed |  |
| `chat.gemini.stream.length` | PASS | chat client -> gemini upstream, stream, length |  |
| `chat.gemini.stream.cached` | PASS | chat client -> gemini upstream, stream, cached |  |
| `chat.compat.json.text` | PASS | chat client -> compat upstream, json, text |  |
| `chat.compat.json.tool` | PASS | chat client -> compat upstream, json, tool |  |
| `chat.compat.json.thinking` | PASS | chat client -> compat upstream, json, thinking |  |
| `chat.compat.json.parallel` | PASS | chat client -> compat upstream, json, parallel |  |
| `chat.compat.json.mixed` | PASS | chat client -> compat upstream, json, mixed |  |
| `chat.compat.json.length` | PASS | chat client -> compat upstream, json, length |  |
| `chat.compat.json.cached` | PASS | chat client -> compat upstream, json, cached |  |
| `chat.compat.stream.text` | PASS | chat client -> compat upstream, stream, text |  |
| `chat.compat.stream.tool` | PASS | chat client -> compat upstream, stream, tool |  |
| `chat.compat.stream.thinking` | PASS | chat client -> compat upstream, stream, thinking |  |
| `chat.compat.stream.parallel` | PASS | chat client -> compat upstream, stream, parallel |  |
| `chat.compat.stream.mixed` | PASS | chat client -> compat upstream, stream, mixed |  |
| `chat.compat.stream.length` | PASS | chat client -> compat upstream, stream, length |  |
| `chat.compat.stream.cached` | PASS | chat client -> compat upstream, stream, cached |  |
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
| `responses.claude.json.parallel` | PASS | responses client -> claude upstream, json, parallel |  |
| `responses.claude.json.mixed` | PASS | responses client -> claude upstream, json, mixed |  |
| `responses.claude.json.length` | PASS | responses client -> claude upstream, json, length |  |
| `responses.claude.json.cached` | PASS | responses client -> claude upstream, json, cached |  |
| `responses.claude.stream.text` | PASS | responses client -> claude upstream, stream, text |  |
| `responses.claude.stream.tool` | PASS | responses client -> claude upstream, stream, tool |  |
| `responses.claude.stream.thinking` | PASS | responses client -> claude upstream, stream, thinking |  |
| `responses.claude.stream.parallel` | PASS | responses client -> claude upstream, stream, parallel |  |
| `responses.claude.stream.mixed` | PASS | responses client -> claude upstream, stream, mixed |  |
| `responses.claude.stream.length` | PASS | responses client -> claude upstream, stream, length |  |
| `responses.claude.stream.cached` | PASS | responses client -> claude upstream, stream, cached |  |
| `responses.codex.json.text` | PASS | responses client -> codex upstream, json, text |  |
| `responses.codex.json.tool` | PASS | responses client -> codex upstream, json, tool |  |
| `responses.codex.json.thinking` | PASS | responses client -> codex upstream, json, thinking |  |
| `responses.codex.json.parallel` | PASS | responses client -> codex upstream, json, parallel |  |
| `responses.codex.json.mixed` | PASS | responses client -> codex upstream, json, mixed |  |
| `responses.codex.json.length` | PASS | responses client -> codex upstream, json, length |  |
| `responses.codex.json.cached` | PASS | responses client -> codex upstream, json, cached |  |
| `responses.codex.stream.text` | PASS | responses client -> codex upstream, stream, text |  |
| `responses.codex.stream.tool` | PASS | responses client -> codex upstream, stream, tool |  |
| `responses.codex.stream.thinking` | PASS | responses client -> codex upstream, stream, thinking |  |
| `responses.codex.stream.parallel` | PASS | responses client -> codex upstream, stream, parallel |  |
| `responses.codex.stream.mixed` | PASS | responses client -> codex upstream, stream, mixed |  |
| `responses.codex.stream.length` | PASS | responses client -> codex upstream, stream, length |  |
| `responses.codex.stream.cached` | PASS | responses client -> codex upstream, stream, cached |  |
| `responses.gemini.json.text` | PASS | responses client -> gemini upstream, json, text |  |
| `responses.gemini.json.tool` | PASS | responses client -> gemini upstream, json, tool |  |
| `responses.gemini.json.thinking` | PASS | responses client -> gemini upstream, json, thinking |  |
| `responses.gemini.json.parallel` | PASS | responses client -> gemini upstream, json, parallel |  |
| `responses.gemini.json.mixed` | PASS | responses client -> gemini upstream, json, mixed |  |
| `responses.gemini.json.length` | PASS | responses client -> gemini upstream, json, length |  |
| `responses.gemini.json.cached` | PASS | responses client -> gemini upstream, json, cached |  |
| `responses.gemini.stream.text` | PASS | responses client -> gemini upstream, stream, text |  |
| `responses.gemini.stream.tool` | PASS | responses client -> gemini upstream, stream, tool |  |
| `responses.gemini.stream.thinking` | PASS | responses client -> gemini upstream, stream, thinking |  |
| `responses.gemini.stream.parallel` | PASS | responses client -> gemini upstream, stream, parallel |  |
| `responses.gemini.stream.mixed` | PASS | responses client -> gemini upstream, stream, mixed |  |
| `responses.gemini.stream.length` | PASS | responses client -> gemini upstream, stream, length |  |
| `responses.gemini.stream.cached` | PASS | responses client -> gemini upstream, stream, cached |  |
| `responses.compat.json.text` | PASS | responses client -> compat upstream, json, text |  |
| `responses.compat.json.tool` | PASS | responses client -> compat upstream, json, tool |  |
| `responses.compat.json.thinking` | PASS | responses client -> compat upstream, json, thinking |  |
| `responses.compat.json.parallel` | PASS | responses client -> compat upstream, json, parallel |  |
| `responses.compat.json.mixed` | PASS | responses client -> compat upstream, json, mixed |  |
| `responses.compat.json.length` | PASS | responses client -> compat upstream, json, length |  |
| `responses.compat.json.cached` | PASS | responses client -> compat upstream, json, cached |  |
| `responses.compat.stream.text` | PASS | responses client -> compat upstream, stream, text |  |
| `responses.compat.stream.tool` | PASS | responses client -> compat upstream, stream, tool |  |
| `responses.compat.stream.thinking` | PASS | responses client -> compat upstream, stream, thinking |  |
| `responses.compat.stream.parallel` | PASS | responses client -> compat upstream, stream, parallel |  |
| `responses.compat.stream.mixed` | PASS | responses client -> compat upstream, stream, mixed |  |
| `responses.compat.stream.length` | PASS | responses client -> compat upstream, stream, length |  |
| `responses.compat.stream.cached` | PASS | responses client -> compat upstream, stream, cached |  |
| `claude.claude.json.text` | PASS | claude client -> claude upstream, json, text |  |
| `claude.claude.json.tool` | PASS | claude client -> claude upstream, json, tool |  |
| `claude.claude.json.thinking` | PASS | claude client -> claude upstream, json, thinking |  |
| `claude.claude.json.parallel` | PASS | claude client -> claude upstream, json, parallel |  |
| `claude.claude.json.mixed` | PASS | claude client -> claude upstream, json, mixed |  |
| `claude.claude.json.length` | PASS | claude client -> claude upstream, json, length |  |
| `claude.claude.json.cached` | PASS | claude client -> claude upstream, json, cached |  |
| `claude.claude.stream.text` | PASS | claude client -> claude upstream, stream, text |  |
| `claude.claude.stream.tool` | PASS | claude client -> claude upstream, stream, tool |  |
| `claude.claude.stream.thinking` | PASS | claude client -> claude upstream, stream, thinking |  |
| `claude.claude.stream.parallel` | PASS | claude client -> claude upstream, stream, parallel |  |
| `claude.claude.stream.mixed` | PASS | claude client -> claude upstream, stream, mixed |  |
| `claude.claude.stream.length` | PASS | claude client -> claude upstream, stream, length |  |
| `claude.claude.stream.cached` | PASS | claude client -> claude upstream, stream, cached |  |
| `claude.codex.json.text` | PASS | claude client -> codex upstream, json, text |  |
| `claude.codex.json.tool` | PASS | claude client -> codex upstream, json, tool |  |
| `claude.codex.json.thinking` | PASS | claude client -> codex upstream, json, thinking |  |
| `claude.codex.json.parallel` | PASS | claude client -> codex upstream, json, parallel |  |
| `claude.codex.json.mixed` | PASS | claude client -> codex upstream, json, mixed |  |
| `claude.codex.json.length` | PASS | claude client -> codex upstream, json, length |  |
| `claude.codex.json.cached` | PASS | claude client -> codex upstream, json, cached |  |
| `claude.codex.stream.text` | PASS | claude client -> codex upstream, stream, text |  |
| `claude.codex.stream.tool` | PASS | claude client -> codex upstream, stream, tool |  |
| `claude.codex.stream.thinking` | PASS | claude client -> codex upstream, stream, thinking |  |
| `claude.codex.stream.parallel` | PASS | claude client -> codex upstream, stream, parallel |  |
| `claude.codex.stream.mixed` | PASS | claude client -> codex upstream, stream, mixed |  |
| `claude.codex.stream.length` | PASS | claude client -> codex upstream, stream, length |  |
| `claude.codex.stream.cached` | PASS | claude client -> codex upstream, stream, cached |  |
| `claude.gemini.json.text` | PASS | claude client -> gemini upstream, json, text |  |
| `claude.gemini.json.tool` | PASS | claude client -> gemini upstream, json, tool |  |
| `claude.gemini.json.thinking` | PASS | claude client -> gemini upstream, json, thinking |  |
| `claude.gemini.json.parallel` | PASS | claude client -> gemini upstream, json, parallel |  |
| `claude.gemini.json.mixed` | PASS | claude client -> gemini upstream, json, mixed |  |
| `claude.gemini.json.length` | PASS | claude client -> gemini upstream, json, length |  |
| `claude.gemini.json.cached` | PASS | claude client -> gemini upstream, json, cached |  |
| `claude.gemini.stream.text` | PASS | claude client -> gemini upstream, stream, text |  |
| `claude.gemini.stream.tool` | PASS | claude client -> gemini upstream, stream, tool |  |
| `claude.gemini.stream.thinking` | PASS | claude client -> gemini upstream, stream, thinking |  |
| `claude.gemini.stream.parallel` | PASS | claude client -> gemini upstream, stream, parallel |  |
| `claude.gemini.stream.mixed` | PASS | claude client -> gemini upstream, stream, mixed |  |
| `claude.gemini.stream.length` | PASS | claude client -> gemini upstream, stream, length |  |
| `claude.gemini.stream.cached` | PASS | claude client -> gemini upstream, stream, cached |  |
| `claude.compat.json.text` | PASS | claude client -> compat upstream, json, text |  |
| `claude.compat.json.tool` | PASS | claude client -> compat upstream, json, tool |  |
| `claude.compat.json.thinking` | PASS | claude client -> compat upstream, json, thinking |  |
| `claude.compat.json.parallel` | PASS | claude client -> compat upstream, json, parallel |  |
| `claude.compat.json.mixed` | PASS | claude client -> compat upstream, json, mixed |  |
| `claude.compat.json.length` | PASS | claude client -> compat upstream, json, length |  |
| `claude.compat.json.cached` | PASS | claude client -> compat upstream, json, cached |  |
| `claude.compat.stream.text` | PASS | claude client -> compat upstream, stream, text |  |
| `claude.compat.stream.tool` | PASS | claude client -> compat upstream, stream, tool |  |
| `claude.compat.stream.thinking` | PASS | claude client -> compat upstream, stream, thinking |  |
| `claude.compat.stream.parallel` | PASS | claude client -> compat upstream, stream, parallel |  |
| `claude.compat.stream.mixed` | PASS | claude client -> compat upstream, stream, mixed |  |
| `claude.compat.stream.length` | PASS | claude client -> compat upstream, stream, length |  |
| `claude.compat.stream.cached` | PASS | claude client -> compat upstream, stream, cached |  |
| `gemini.claude.json.text` | PASS | gemini client -> claude upstream, json, text |  |
| `gemini.claude.json.tool` | PASS | gemini client -> claude upstream, json, tool |  |
| `gemini.claude.json.thinking` | PASS | gemini client -> claude upstream, json, thinking |  |
| `gemini.claude.json.parallel` | PASS | gemini client -> claude upstream, json, parallel |  |
| `gemini.claude.json.mixed` | PASS | gemini client -> claude upstream, json, mixed |  |
| `gemini.claude.json.length` | PASS | gemini client -> claude upstream, json, length |  |
| `gemini.claude.json.cached` | PASS | gemini client -> claude upstream, json, cached |  |
| `gemini.claude.stream.text` | PASS | gemini client -> claude upstream, stream, text |  |
| `gemini.claude.stream.tool` | PASS | gemini client -> claude upstream, stream, tool |  |
| `gemini.claude.stream.thinking` | PASS | gemini client -> claude upstream, stream, thinking |  |
| `gemini.claude.stream.parallel` | PASS | gemini client -> claude upstream, stream, parallel |  |
| `gemini.claude.stream.mixed` | PASS | gemini client -> claude upstream, stream, mixed |  |
| `gemini.claude.stream.length` | PASS | gemini client -> claude upstream, stream, length |  |
| `gemini.claude.stream.cached` | PASS | gemini client -> claude upstream, stream, cached |  |
| `gemini.codex.json.text` | PASS | gemini client -> codex upstream, json, text |  |
| `gemini.codex.json.tool` | PASS | gemini client -> codex upstream, json, tool |  |
| `gemini.codex.json.thinking` | PASS | gemini client -> codex upstream, json, thinking |  |
| `gemini.codex.json.parallel` | PASS | gemini client -> codex upstream, json, parallel |  |
| `gemini.codex.json.mixed` | PASS | gemini client -> codex upstream, json, mixed |  |
| `gemini.codex.json.length` | PASS | gemini client -> codex upstream, json, length |  |
| `gemini.codex.json.cached` | PASS | gemini client -> codex upstream, json, cached |  |
| `gemini.codex.stream.text` | PASS | gemini client -> codex upstream, stream, text |  |
| `gemini.codex.stream.tool` | PASS | gemini client -> codex upstream, stream, tool |  |
| `gemini.codex.stream.thinking` | PASS | gemini client -> codex upstream, stream, thinking |  |
| `gemini.codex.stream.parallel` | PASS | gemini client -> codex upstream, stream, parallel |  |
| `gemini.codex.stream.mixed` | PASS | gemini client -> codex upstream, stream, mixed |  |
| `gemini.codex.stream.length` | PASS | gemini client -> codex upstream, stream, length |  |
| `gemini.codex.stream.cached` | PASS | gemini client -> codex upstream, stream, cached |  |
| `gemini.gemini.json.text` | PASS | gemini client -> gemini upstream, json, text |  |
| `gemini.gemini.json.tool` | PASS | gemini client -> gemini upstream, json, tool |  |
| `gemini.gemini.json.thinking` | PASS | gemini client -> gemini upstream, json, thinking |  |
| `gemini.gemini.json.parallel` | PASS | gemini client -> gemini upstream, json, parallel |  |
| `gemini.gemini.json.mixed` | PASS | gemini client -> gemini upstream, json, mixed |  |
| `gemini.gemini.json.length` | PASS | gemini client -> gemini upstream, json, length |  |
| `gemini.gemini.json.cached` | PASS | gemini client -> gemini upstream, json, cached |  |
| `gemini.gemini.stream.text` | PASS | gemini client -> gemini upstream, stream, text |  |
| `gemini.gemini.stream.tool` | PASS | gemini client -> gemini upstream, stream, tool |  |
| `gemini.gemini.stream.thinking` | PASS | gemini client -> gemini upstream, stream, thinking |  |
| `gemini.gemini.stream.parallel` | PASS | gemini client -> gemini upstream, stream, parallel |  |
| `gemini.gemini.stream.mixed` | PASS | gemini client -> gemini upstream, stream, mixed |  |
| `gemini.gemini.stream.length` | PASS | gemini client -> gemini upstream, stream, length |  |
| `gemini.gemini.stream.cached` | PASS | gemini client -> gemini upstream, stream, cached |  |
| `gemini.compat.json.text` | PASS | gemini client -> compat upstream, json, text |  |
| `gemini.compat.json.tool` | PASS | gemini client -> compat upstream, json, tool |  |
| `gemini.compat.json.thinking` | PASS | gemini client -> compat upstream, json, thinking |  |
| `gemini.compat.json.parallel` | PASS | gemini client -> compat upstream, json, parallel |  |
| `gemini.compat.json.mixed` | PASS | gemini client -> compat upstream, json, mixed |  |
| `gemini.compat.json.length` | PASS | gemini client -> compat upstream, json, length |  |
| `gemini.compat.json.cached` | PASS | gemini client -> compat upstream, json, cached |  |
| `gemini.compat.stream.text` | PASS | gemini client -> compat upstream, stream, text |  |
| `gemini.compat.stream.tool` | PASS | gemini client -> compat upstream, stream, tool |  |
| `gemini.compat.stream.thinking` | PASS | gemini client -> compat upstream, stream, thinking |  |
| `gemini.compat.stream.parallel` | PASS | gemini client -> compat upstream, stream, parallel |  |
| `gemini.compat.stream.mixed` | PASS | gemini client -> compat upstream, stream, mixed |  |
| `gemini.compat.stream.length` | PASS | gemini client -> compat upstream, stream, length |  |
| `gemini.compat.stream.cached` | PASS | gemini client -> compat upstream, stream, cached |  |
| `geminisse.claude.stream.text` | PASS | geminisse client -> claude upstream, stream, text |  |
| `geminisse.codex.stream.text` | PASS | geminisse client -> codex upstream, stream, text |  |
| `geminisse.gemini.stream.text` | PASS | geminisse client -> gemini upstream, stream, text |  |
| `geminisse.compat.stream.text` | PASS | geminisse client -> compat upstream, stream, text |  |
| `responses.claude.json.pause` | PASS | responses client -> claude upstream, json, pause_turn stop reason |  |
| `responses.claude.stream.pause` | PASS | responses client -> claude upstream, stream, pause_turn stop reason |  |
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
| `err.claude.claude.upstream400` | PASS | claude client -> claude upstream: upstream400 |  |
| `err.claude.claude.429_failover` | PASS | claude client -> claude upstream: 429_failover |  |
| `err.claude.claude.429_all` | PASS | claude client -> claude upstream: 429_all |  |
| `err.claude.claude.500_failover` | PASS | claude client -> claude upstream: 500_failover |  |
| `err.claude.claude.500_all` | PASS | claude client -> claude upstream: 500_all |  |
| `err.claude.claude.stream_500_failover` | PASS | claude client -> claude upstream: stream_500_failover |  |
| `err.claude.claude.stream_mid_error` | PASS | claude client -> claude upstream: stream_mid_error |  |
| `err.claude.claude.stream_cut_abort` | PASS | claude client -> claude upstream: stream_cut_abort |  |
| `err.claude.claude.stream_cut_clean` | PASS | claude client -> claude upstream: stream_cut_clean |  |
| `err.claude.claude.json_cut_abort` | PASS | claude client -> claude upstream: json_cut_abort |  |
| `err.claude.claude.upstream408` | PASS | claude client -> claude upstream: upstream408 |  |
| `err.claude.codex.upstream401` | PASS | claude client -> codex upstream: upstream401 |  |
| `err.claude.codex.upstream400` | PASS | claude client -> codex upstream: upstream400 |  |
| `err.claude.codex.429_failover` | PASS | claude client -> codex upstream: 429_failover |  |
| `err.claude.codex.429_all` | PASS | claude client -> codex upstream: 429_all |  |
| `err.claude.codex.500_failover` | PASS | claude client -> codex upstream: 500_failover |  |
| `err.claude.codex.500_all` | PASS | claude client -> codex upstream: 500_all |  |
| `err.claude.codex.stream_500_failover` | PASS | claude client -> codex upstream: stream_500_failover |  |
| `err.claude.codex.stream_mid_error` | PASS | claude client -> codex upstream: stream_mid_error |  |
| `err.claude.codex.stream_cut_abort` | PASS | claude client -> codex upstream: stream_cut_abort |  |
| `err.claude.codex.stream_cut_clean` | PASS | claude client -> codex upstream: stream_cut_clean |  |
| `err.claude.codex.json_cut_abort` | PASS | claude client -> codex upstream: json_cut_abort |  |
| `err.claude.codex.upstream408` | PASS | claude client -> codex upstream: upstream408 |  |
| `err.claude.gemini.upstream401` | PASS | claude client -> gemini upstream: upstream401 |  |
| `err.claude.gemini.upstream400` | PASS | claude client -> gemini upstream: upstream400 |  |
| `err.claude.gemini.429_failover` | PASS | claude client -> gemini upstream: 429_failover |  |
| `err.claude.gemini.429_all` | PASS | claude client -> gemini upstream: 429_all |  |
| `err.claude.gemini.500_failover` | PASS | claude client -> gemini upstream: 500_failover |  |
| `err.claude.gemini.500_all` | PASS | claude client -> gemini upstream: 500_all |  |
| `err.claude.gemini.stream_500_failover` | PASS | claude client -> gemini upstream: stream_500_failover |  |
| `err.claude.gemini.stream_mid_error` | PASS | claude client -> gemini upstream: stream_mid_error |  |
| `err.claude.gemini.stream_cut_abort` | PASS | claude client -> gemini upstream: stream_cut_abort |  |
| `err.claude.gemini.stream_cut_clean` | PASS | claude client -> gemini upstream: stream_cut_clean |  |
| `err.claude.gemini.json_cut_abort` | PASS | claude client -> gemini upstream: json_cut_abort |  |
| `err.claude.gemini.upstream408` | PASS | claude client -> gemini upstream: upstream408 |  |
| `err.claude.compat.upstream401` | PASS | claude client -> compat upstream: upstream401 |  |
| `err.claude.compat.upstream400` | PASS | claude client -> compat upstream: upstream400 |  |
| `err.claude.compat.429_failover` | PASS | claude client -> compat upstream: 429_failover |  |
| `err.claude.compat.429_all` | PASS | claude client -> compat upstream: 429_all |  |
| `err.claude.compat.500_failover` | PASS | claude client -> compat upstream: 500_failover |  |
| `err.claude.compat.500_all` | PASS | claude client -> compat upstream: 500_all |  |
| `err.claude.compat.stream_500_failover` | PASS | claude client -> compat upstream: stream_500_failover |  |
| `err.claude.compat.stream_mid_error` | PASS | claude client -> compat upstream: stream_mid_error |  |
| `err.claude.compat.stream_cut_abort` | PASS | claude client -> compat upstream: stream_cut_abort |  |
| `err.claude.compat.stream_cut_clean` | PASS | claude client -> compat upstream: stream_cut_clean |  |
| `err.claude.compat.json_cut_abort` | PASS | claude client -> compat upstream: json_cut_abort |  |
| `err.claude.compat.upstream408` | PASS | claude client -> compat upstream: upstream408 |  |
| `err.responses.claude.upstream401` | PASS | responses client -> claude upstream: upstream401 |  |
| `err.responses.claude.upstream400` | PASS | responses client -> claude upstream: upstream400 |  |
| `err.responses.claude.429_failover` | PASS | responses client -> claude upstream: 429_failover |  |
| `err.responses.claude.429_all` | PASS | responses client -> claude upstream: 429_all |  |
| `err.responses.claude.500_failover` | PASS | responses client -> claude upstream: 500_failover |  |
| `err.responses.claude.500_all` | PASS | responses client -> claude upstream: 500_all |  |
| `err.responses.claude.stream_500_failover` | PASS | responses client -> claude upstream: stream_500_failover |  |
| `err.responses.claude.stream_mid_error` | PASS | responses client -> claude upstream: stream_mid_error |  |
| `err.responses.claude.stream_cut_abort` | PASS | responses client -> claude upstream: stream_cut_abort |  |
| `err.responses.claude.stream_cut_clean` | PASS | responses client -> claude upstream: stream_cut_clean |  |
| `err.responses.claude.json_cut_abort` | PASS | responses client -> claude upstream: json_cut_abort |  |
| `err.responses.codex.upstream401` | PASS | responses client -> codex upstream: upstream401 |  |
| `err.responses.codex.upstream400` | PASS | responses client -> codex upstream: upstream400 |  |
| `err.responses.codex.429_failover` | PASS | responses client -> codex upstream: 429_failover |  |
| `err.responses.codex.429_all` | PASS | responses client -> codex upstream: 429_all |  |
| `err.responses.codex.500_failover` | PASS | responses client -> codex upstream: 500_failover |  |
| `err.responses.codex.500_all` | PASS | responses client -> codex upstream: 500_all |  |
| `err.responses.codex.stream_500_failover` | PASS | responses client -> codex upstream: stream_500_failover |  |
| `err.responses.codex.stream_mid_error` | PASS | responses client -> codex upstream: stream_mid_error |  |
| `err.responses.codex.stream_cut_abort` | PASS | responses client -> codex upstream: stream_cut_abort |  |
| `err.responses.codex.stream_cut_clean` | PASS | responses client -> codex upstream: stream_cut_clean |  |
| `err.responses.codex.json_cut_abort` | PASS | responses client -> codex upstream: json_cut_abort |  |
| `err.responses.gemini.upstream401` | PASS | responses client -> gemini upstream: upstream401 |  |
| `err.responses.gemini.upstream400` | PASS | responses client -> gemini upstream: upstream400 |  |
| `err.responses.gemini.429_failover` | PASS | responses client -> gemini upstream: 429_failover |  |
| `err.responses.gemini.429_all` | PASS | responses client -> gemini upstream: 429_all |  |
| `err.responses.gemini.500_failover` | PASS | responses client -> gemini upstream: 500_failover |  |
| `err.responses.gemini.500_all` | PASS | responses client -> gemini upstream: 500_all |  |
| `err.responses.gemini.stream_500_failover` | PASS | responses client -> gemini upstream: stream_500_failover |  |
| `err.responses.gemini.stream_mid_error` | PASS | responses client -> gemini upstream: stream_mid_error |  |
| `err.responses.gemini.stream_cut_abort` | PASS | responses client -> gemini upstream: stream_cut_abort |  |
| `err.responses.gemini.stream_cut_clean` | PASS | responses client -> gemini upstream: stream_cut_clean |  |
| `err.responses.gemini.json_cut_abort` | PASS | responses client -> gemini upstream: json_cut_abort |  |
| `err.responses.compat.upstream401` | PASS | responses client -> compat upstream: upstream401 |  |
| `err.responses.compat.upstream400` | PASS | responses client -> compat upstream: upstream400 |  |
| `err.responses.compat.429_failover` | PASS | responses client -> compat upstream: 429_failover |  |
| `err.responses.compat.429_all` | PASS | responses client -> compat upstream: 429_all |  |
| `err.responses.compat.500_failover` | PASS | responses client -> compat upstream: 500_failover |  |
| `err.responses.compat.500_all` | PASS | responses client -> compat upstream: 500_all |  |
| `err.responses.compat.stream_500_failover` | PASS | responses client -> compat upstream: stream_500_failover |  |
| `err.responses.compat.stream_mid_error` | PASS | responses client -> compat upstream: stream_mid_error |  |
| `err.responses.compat.stream_cut_abort` | PASS | responses client -> compat upstream: stream_cut_abort |  |
| `err.responses.compat.stream_cut_clean` | PASS | responses client -> compat upstream: stream_cut_clean |  |
| `err.responses.compat.json_cut_abort` | PASS | responses client -> compat upstream: json_cut_abort |  |
| `err.gemini.claude.upstream401` | PASS | gemini client -> claude upstream: upstream401 |  |
| `err.gemini.claude.upstream400` | PASS | gemini client -> claude upstream: upstream400 |  |
| `err.gemini.claude.429_failover` | PASS | gemini client -> claude upstream: 429_failover |  |
| `err.gemini.claude.429_all` | PASS | gemini client -> claude upstream: 429_all |  |
| `err.gemini.claude.500_failover` | PASS | gemini client -> claude upstream: 500_failover |  |
| `err.gemini.claude.500_all` | PASS | gemini client -> claude upstream: 500_all |  |
| `err.gemini.claude.stream_500_failover` | PASS | gemini client -> claude upstream: stream_500_failover |  |
| `err.gemini.claude.stream_mid_error` | PASS | gemini client -> claude upstream: stream_mid_error |  |
| `err.gemini.claude.stream_cut_abort` | PASS | gemini client -> claude upstream: stream_cut_abort |  |
| `err.gemini.claude.stream_cut_clean` | PASS | gemini client -> claude upstream: stream_cut_clean |  |
| `err.gemini.claude.json_cut_abort` | PASS | gemini client -> claude upstream: json_cut_abort |  |
| `err.gemini.codex.upstream401` | PASS | gemini client -> codex upstream: upstream401 |  |
| `err.gemini.codex.upstream400` | PASS | gemini client -> codex upstream: upstream400 |  |
| `err.gemini.codex.429_failover` | PASS | gemini client -> codex upstream: 429_failover |  |
| `err.gemini.codex.429_all` | PASS | gemini client -> codex upstream: 429_all |  |
| `err.gemini.codex.500_failover` | PASS | gemini client -> codex upstream: 500_failover |  |
| `err.gemini.codex.500_all` | PASS | gemini client -> codex upstream: 500_all |  |
| `err.gemini.codex.stream_500_failover` | PASS | gemini client -> codex upstream: stream_500_failover |  |
| `err.gemini.codex.stream_mid_error` | PASS | gemini client -> codex upstream: stream_mid_error |  |
| `err.gemini.codex.stream_cut_abort` | PASS | gemini client -> codex upstream: stream_cut_abort |  |
| `err.gemini.codex.stream_cut_clean` | PASS | gemini client -> codex upstream: stream_cut_clean |  |
| `err.gemini.codex.json_cut_abort` | PASS | gemini client -> codex upstream: json_cut_abort |  |
| `err.gemini.gemini.upstream401` | PASS | gemini client -> gemini upstream: upstream401 |  |
| `err.gemini.gemini.upstream400` | PASS | gemini client -> gemini upstream: upstream400 |  |
| `err.gemini.gemini.429_failover` | PASS | gemini client -> gemini upstream: 429_failover |  |
| `err.gemini.gemini.429_all` | PASS | gemini client -> gemini upstream: 429_all |  |
| `err.gemini.gemini.500_failover` | PASS | gemini client -> gemini upstream: 500_failover |  |
| `err.gemini.gemini.500_all` | PASS | gemini client -> gemini upstream: 500_all |  |
| `err.gemini.gemini.stream_500_failover` | PASS | gemini client -> gemini upstream: stream_500_failover |  |
| `err.gemini.gemini.stream_mid_error` | PASS | gemini client -> gemini upstream: stream_mid_error |  |
| `err.gemini.gemini.stream_cut_abort` | PASS | gemini client -> gemini upstream: stream_cut_abort |  |
| `err.gemini.gemini.stream_cut_clean` | PASS | gemini client -> gemini upstream: stream_cut_clean |  |
| `err.gemini.gemini.json_cut_abort` | PASS | gemini client -> gemini upstream: json_cut_abort |  |
| `err.gemini.compat.upstream401` | PASS | gemini client -> compat upstream: upstream401 |  |
| `err.gemini.compat.upstream400` | PASS | gemini client -> compat upstream: upstream400 |  |
| `err.gemini.compat.429_failover` | PASS | gemini client -> compat upstream: 429_failover |  |
| `err.gemini.compat.429_all` | PASS | gemini client -> compat upstream: 429_all |  |
| `err.gemini.compat.500_failover` | PASS | gemini client -> compat upstream: 500_failover |  |
| `err.gemini.compat.500_all` | PASS | gemini client -> compat upstream: 500_all |  |
| `err.gemini.compat.stream_500_failover` | PASS | gemini client -> compat upstream: stream_500_failover |  |
| `err.gemini.compat.stream_mid_error` | PASS | gemini client -> compat upstream: stream_mid_error |  |
| `err.gemini.compat.stream_cut_abort` | PASS | gemini client -> compat upstream: stream_cut_abort |  |
| `err.gemini.compat.stream_cut_clean` | PASS | gemini client -> compat upstream: stream_cut_clean |  |
| `err.gemini.compat.json_cut_abort` | PASS | gemini client -> compat upstream: json_cut_abort |  |
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
| `route.cooling.all_cooling_500` | PASS | every key cooling down: auth_unavailable with the last upstream error |  |
| `route.cooling.all_cooling_401` | PASS | every key cooling down: auth_unavailable with the last upstream error |  |
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
| `history.thinking.chat.claude` | PASS | turn carrying previous reasoning: chat client -> claude upstream |  |
| `history.thinking.chat.codex` | PASS | turn carrying previous reasoning: chat client -> codex upstream |  |
| `history.thinking.chat.gemini` | PASS | turn carrying previous reasoning: chat client -> gemini upstream |  |
| `history.thinking.chat.compat` | PASS | turn carrying previous reasoning: chat client -> compat upstream |  |
| `history.thinking.responses.claude` | PASS | turn carrying previous reasoning: responses client -> claude upstream |  |
| `history.thinking.responses.codex` | PASS | turn carrying previous reasoning: responses client -> codex upstream |  |
| `history.thinking.responses.gemini` | PASS | turn carrying previous reasoning: responses client -> gemini upstream |  |
| `history.thinking.responses.compat` | PASS | turn carrying previous reasoning: responses client -> compat upstream |  |
| `history.thinking.claude.claude` | PASS | turn carrying previous reasoning: claude client -> claude upstream |  |
| `history.thinking.claude.codex` | PASS | turn carrying previous reasoning: claude client -> codex upstream |  |
| `history.thinking.claude.gemini` | PASS | turn carrying previous reasoning: claude client -> gemini upstream |  |
| `history.thinking.claude.compat` | PASS | turn carrying previous reasoning: claude client -> compat upstream |  |
| `history.thinking.gemini.claude` | PASS | turn carrying previous reasoning: gemini client -> claude upstream |  |
| `history.thinking.gemini.codex` | PASS | turn carrying previous reasoning: gemini client -> codex upstream |  |
| `history.thinking.gemini.gemini` | PASS | turn carrying previous reasoning: gemini client -> gemini upstream |  |
| `history.thinking.gemini.compat` | PASS | turn carrying previous reasoning: gemini client -> compat upstream |  |
| `chunking.split.chat.claude` | PASS | upstream stream written split: chat client -> claude upstream |  |
| `chunking.merged.chat.claude` | PASS | upstream stream written merged: chat client -> claude upstream |  |
| `chunking.split.chat.codex` | PASS | upstream stream written split: chat client -> codex upstream |  |
| `chunking.merged.chat.codex` | PASS | upstream stream written merged: chat client -> codex upstream |  |
| `chunking.split.chat.gemini` | PASS | upstream stream written split: chat client -> gemini upstream |  |
| `chunking.merged.chat.gemini` | PASS | upstream stream written merged: chat client -> gemini upstream |  |
| `chunking.split.chat.compat` | PASS | upstream stream written split: chat client -> compat upstream |  |
| `chunking.merged.chat.compat` | PASS | upstream stream written merged: chat client -> compat upstream |  |
| `chunking.split.responses.claude` | PASS | upstream stream written split: responses client -> claude upstream |  |
| `chunking.merged.responses.claude` | PASS | upstream stream written merged: responses client -> claude upstream |  |
| `chunking.split.responses.codex` | PASS | upstream stream written split: responses client -> codex upstream |  |
| `chunking.merged.responses.codex` | PASS | upstream stream written merged: responses client -> codex upstream |  |
| `chunking.split.responses.gemini` | PASS | upstream stream written split: responses client -> gemini upstream |  |
| `chunking.merged.responses.gemini` | PASS | upstream stream written merged: responses client -> gemini upstream |  |
| `chunking.split.responses.compat` | PASS | upstream stream written split: responses client -> compat upstream |  |
| `chunking.merged.responses.compat` | PASS | upstream stream written merged: responses client -> compat upstream |  |
| `chunking.split.claude.claude` | PASS | upstream stream written split: claude client -> claude upstream |  |
| `chunking.merged.claude.claude` | PASS | upstream stream written merged: claude client -> claude upstream |  |
| `chunking.split.claude.codex` | PASS | upstream stream written split: claude client -> codex upstream |  |
| `chunking.merged.claude.codex` | PASS | upstream stream written merged: claude client -> codex upstream |  |
| `chunking.split.claude.gemini` | PASS | upstream stream written split: claude client -> gemini upstream |  |
| `chunking.merged.claude.gemini` | PASS | upstream stream written merged: claude client -> gemini upstream |  |
| `chunking.split.claude.compat` | PASS | upstream stream written split: claude client -> compat upstream |  |
| `chunking.merged.claude.compat` | PASS | upstream stream written merged: claude client -> compat upstream |  |
| `chunking.split.gemini.claude` | PASS | upstream stream written split: gemini client -> claude upstream |  |
| `chunking.merged.gemini.claude` | PASS | upstream stream written merged: gemini client -> claude upstream |  |
| `chunking.split.gemini.codex` | PASS | upstream stream written split: gemini client -> codex upstream |  |
| `chunking.merged.gemini.codex` | PASS | upstream stream written merged: gemini client -> codex upstream |  |
| `chunking.split.gemini.gemini` | PASS | upstream stream written split: gemini client -> gemini upstream |  |
| `chunking.merged.gemini.gemini` | PASS | upstream stream written merged: gemini client -> gemini upstream |  |
| `chunking.split.gemini.compat` | PASS | upstream stream written split: gemini client -> compat upstream |  |
| `chunking.merged.gemini.compat` | PASS | upstream stream written merged: gemini client -> compat upstream |  |
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
| `ws.xai.text` | PASS | response.create forwarded over the xAI upstream websocket |  |
| `ws.xai.thinking` | PASS | reasoning events over the xAI websocket |  |
| `ws.xai.two_turns` | PASS | second turn continues on the same upstream socket |  |
| `ws.xai.previous_response_id` | PASS | previous_response_id continues on the live upstream socket |  |
| `ws.xai.error_400` | PASS | upstream error frame |  |
| `ws.xai.error_401` | PASS | upstream error frame |  |
| `ws.xai.error_429` | PASS | upstream error frame |  |
| `ws.xai.error_500` | PASS | upstream error frame |  |
| `ws.xai.mid_error` | PASS | upstream response.failed after some output |  |
| `ws.xai.cut_abort` | PASS | upstream drops its websocket mid-turn |  |
| `ws.steering.text` | PASS | one turn over a duplex stream |  |
| `ws.steering.thinking` | PASS | reasoning events over a duplex stream |  |
| `ws.steering.two_turns` | PASS | a second create travels through the duplex writer |  |
| `ws.steering.steer_frame` | PASS | response.steer reaches the upstream socket untouched |  |
| `ws.steering.bad_frames` | PASS | invalid frames get local error frames and keep the stream alive |  |
| `ws.steering.error_400` | PASS | upstream error frame before the first response |  |
| `ws.steering.error_401` | PASS | upstream error frame before the first response |  |
| `ws.steering.error_429` | PASS | upstream error frame before the first response |  |
| `ws.steering.error_500` | PASS | upstream error frame before the first response |  |
| `ws.steering.mid_error` | PASS | upstream response.failed after some output |  |
| `ws.steering.cut_abort` | PASS | upstream drops its websocket mid-turn |  |
| `ws.handshake.missing_key` | PASS | upgrade without credentials |  |
| `ws.handshake.invalid_key` | PASS | upgrade with a wrong key |  |
| `ws.handshake.query_key` | PASS | key in the query string |  |
| `ws.handshake.codex_backend_path` | PASS | /backend-api/codex/responses alias |  |
| `ws.handshake.turn_state_echo` | PASS | x-codex-turn-state is echoed on the upgrade response |  |
| `ws.handshake.plain_get` | PASS | GET /v1/responses without an upgrade |  |
| `responses.codex.collab_spawn.json` | PASS | orphan delegation output becomes a user message before routing (non-stream) |  |
| `responses.codex.collab_spawn.stream` | PASS | orphan delegation output becomes a user message before routing (SSE) |  |
| `responses.codex.collab_spawn.compact` | PASS | compact rewrites orphan delegations at the handler (tools are prepared by the executor only) |  |
| `responses.codex.collab_spawn.no_subagent_header` | PASS | without X-Openai-Subagent the orphan output is untouched while tools are still prepared |  |
| `ws.fallback.codex.collab_spawn` | PASS | orphan delegation with a call_id is rewritten before the tool-call cache; the next turn replays the rewritten input |  |
| `ws.fallback.codex.collab_spawn_no_header` | PASS | the same websocket request without the sub-agent header keeps the orphan output untouched |  |
| `reqlog.claude.json` | PASS | request-log file of a successful call |  |
| `reqlog.claude.stream` | PASS | request-log file of a successful call |  |
| `reqlog.codex.json` | PASS | request-log file of a successful call |  |
| `reqlog.codex.stream` | PASS | request-log file of a successful call |  |
| `reqlog.gemini.json` | PASS | request-log file of a successful call |  |
| `reqlog.gemini.stream` | PASS | request-log file of a successful call |  |
| `reqlog.compat.json` | PASS | request-log file of a successful call |  |
| `reqlog.compat.stream` | PASS | request-log file of a successful call |  |
| `reqlog.claude.messages` | PASS | request-log file of a Claude-dialect call routed to Claude |  |
| `reqlog.claude.failover` | PASS | two upstream attempts (500 then success) produce API REQUEST 1/2 and API RESPONSE 1/2 |  |
| `reqlog.codex.failover_stream` | PASS | stream bootstrap failover logs both attempts |  |
| `reqlog.gemini.upstream_400` | PASS | upstream client error with request-log on |  |
| `reqlog.compat.mid_error` | PASS | stream failing after some output |  |
| `reqlog.commercial.mid_error` | PASS | commercial mode with request-log on: a 200 stream failing after some output writes no log file (as Go) |  |
| `reqlog.forced.claude.upstream_500` | PASS | error-only log of a failing request (deferred API REQUEST) |  |
| `reqlog.forced.codex.upstream_500` | PASS | error-only log of a failing request (deferred API REQUEST) |  |
| `reqlog.off.success` | PASS | request-log off and a successful call: no file |  |
| `reqlog.ws.upstream_text` | PASS | Responses websocket client over a Codex upstream websocket |  |
| `reqlog.ws.fallback_text` | PASS | Responses websocket client over an HTTP Codex upstream |  |
| `reqlog.ws.upstream_error` | PASS | an upstream error frame on the Codex websocket |  |
| `reqlog.ws.upstream_abort` | PASS | the upstream drops its websocket mid-turn |  |
| `reqlog.ws.steering` | PASS | duplex steering stream on the Codex websocket |  |
| `reqlog.ws.xai` | PASS | Responses websocket client over an xAI upstream websocket |  |
| `reqlog.claude.count_tokens` | PASS | Claude count_tokens request-log file |  |
| `realtime.secrets.create` | PASS | client secret for a realtime session with an explicit lifetime |  |
| `realtime.secrets.create_empty` | PASS | empty request object gets the default session |  |
| `realtime.secrets.create_no_body` | PASS | no body at all |  |
| `realtime.secrets.model_alias` | PASS | realtime-preview models keep the client name in the response |  |
| `realtime.secrets.session_type_defaults` | PASS | a session without type or model gets both defaults |  |
| `realtime.secrets.unsupported_type` | PASS | transcription sessions are not supported |  |
| `realtime.secrets.invalid_session` | PASS | session must be an object |  |
| `realtime.secrets.lifetime_too_short` | PASS | expires_after below the minimum |  |
| `realtime.secrets.lifetime_too_long` | PASS | expires_after above the maximum |  |
| `realtime.secrets.lifetime_anchor` | PASS | unknown anchor |  |
| `realtime.secrets.lifetime_type` | PASS | seconds must be an integer |  |
| `realtime.secrets.array_body` | PASS | request must be an object |  |
| `realtime.secrets.html_and_unicode` | PASS | response escaping of <, >, & and non-ASCII text |  |
| `realtime.secrets.number_formats` | PASS | session numbers and nesting survive the canonical re-encoding |  |
| `realtime.secrets.field_types` | PASS | non-string type and blank model fall back to the defaults |  |
| `realtime.secrets.duplicate_keys` | PASS | the last duplicate key wins |  |
| `realtime.secrets.case_insensitive_keys` | PASS | request members match case-insensitively |  |
| `realtime.secrets.lifetime_float` | PASS | fractional seconds do not decode |  |
| `realtime.secrets.session_null` | PASS | explicit null session |  |
| `realtime.secrets.session_string` | PASS | string session |  |
| `realtime.secrets.null_body` | PASS | JSON null body |  |
| `realtime.secrets.malformed` | PASS | malformed JSON |  |
| `realtime.secrets.missing_key` | PASS | no credentials (realtime-shaped error) |  |
| `realtime.secrets.invalid_key` | PASS | wrong credentials |  |
| `realtime.secrets.second_key` | PASS | the second configured client key |  |
| `realtime.secrets.get_not_routed` | PASS | only POST is registered |  |
| `realtime.sessions.legacy` | PASS | deprecated sessions endpoint embeds the client secret |  |
| `realtime.sessions.legacy_empty` | PASS | legacy endpoint without a body |  |
| `realtime.sessions.legacy_array` | PASS | legacy session must be an object |  |
| `realtime.sessions.legacy_unsupported` | PASS | legacy transcription session |  |
| `realtime.stubs.transcription_sessions` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.translations_post` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.translations_get` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.translations_client_secrets` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.sip_accept` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.sip_reject` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.sip_refer` | PASS | capability is not supported by the Codex OAuth upstream |  |
| `realtime.stubs.no_key` | PASS | stub behind standard auth |  |
| `realtime.stubs.translations_no_key` | PASS | stub behind realtime auth |  |
| `realtime.auth.call_missing_key` | PASS | call bootstrap without credentials |  |
| `realtime.auth.call_invalid_key` | PASS | call bootstrap with a wrong key |  |
| `realtime.auth.unknown_client_secret` | PASS | an ek_ bearer that was never issued |  |
| `realtime.auth.live_missing_key` | PASS | plain API-key error shape on /v1/live |  |
| `realtime.auth.live_invalid_key` | PASS | wrong key on /v1/live |  |
| `realtime.auth.preflight` | PASS | CORS preflight on a realtime route |  |
| `realtime.auth.sideband_missing_key` | PASS | sideband without credentials |  |
| `realtime.call.live.no_credential_sdp` | PASS | raw SDP offer without a Codex OAuth credential |  |
| `realtime.call.live.no_credential_json` | PASS | JSON call request without a Codex OAuth credential |  |
| `realtime.call.realtime.no_credential_sdp` | PASS | raw SDP offer without a Codex OAuth credential |  |
| `realtime.call.realtime.no_credential_json` | PASS | JSON call request without a Codex OAuth credential |  |
| `realtime.call.realtime_calls.no_credential_sdp` | PASS | raw SDP offer without a Codex OAuth credential |  |
| `realtime.call.realtime_calls.no_credential_json` | PASS | JSON call request without a Codex OAuth credential |  |
| `realtime.call.multipart_no_credential` | PASS | multipart call request without a Codex OAuth credential |  |
| `realtime.call.multipart_missing_sdp` | PASS | multipart body without the sdp field |  |
| `realtime.call.multipart_bad_session` | PASS | multipart session field is not JSON |  |
| `realtime.call.multipart_no_boundary` | PASS | multipart without a boundary parameter |  |
| `realtime.call.malformed_json` | PASS | JSON call request that does not parse |  |
| `realtime.call.malformed_json_live` | PASS | same on the live path |  |
| `realtime.call.json_array` | PASS | JSON call request that is an array |  |
| `realtime.call.session_not_object` | PASS | session field that is not an object |  |
| `realtime.call.json_null` | PASS | JSON null call request |  |
| `realtime.call.unknown_content_type` | PASS | an unrelated content type |  |
| `realtime.call.multipart_lf_only` | PASS | multipart with LF-only line endings |  |
| `realtime.call.multipart_truncated` | PASS | multipart body cut inside a part |  |
| `realtime.call.multipart_empty` | PASS | multipart content type with an empty body |  |
| `realtime.call.empty_body` | PASS | no body and no content type |  |
| `realtime.sideband.not_upgrade` | PASS | sideband GET without an upgrade |  |
| `realtime.sideband.not_upgrade_calls` | PASS | same on the realtime calls path |  |
| `realtime.sideband.unknown_call` | PASS | upgrade for a call that does not exist |  |
| `realtime.sideband.unknown_call_realtime` | PASS | realtime-shaped not found |  |
| `realtime.sideband.unknown_call_query` | PASS | call id in the query |  |
| `realtime.sideband.invalid_call_id` | PASS | call id with characters outside the allowed set |  |
| `realtime.sideband.invalid_call_id_query` | PASS | invalid call id in the query |  |
| `realtime.direct.not_upgrade` | PASS | standard realtime GET without an upgrade |  |
| `realtime.direct.not_upgrade_model` | PASS | same with a model query |  |
| `realtime.direct.no_credential` | PASS | upgrade without a Codex OAuth credential |  |
| `realtime.hangup.invalid_call_id` | PASS | hangup with an invalid call id |  |
| `realtime.hangup.unknown_call` | PASS | hangup for a call that does not exist |  |
| `realtime.hangup.missing_key` | PASS | hangup without credentials |  |
| `realtime.routing.trailing_slash_post` | PASS | POST with a trailing slash redirects |  |
| `realtime.routing.trailing_slash_get` | PASS | GET with a trailing slash redirects |  |
| `realtime.routing.wrong_method_calls` | PASS | GET on the POST-only calls route |  |
| `realtime.routing.wrong_method_hangup` | PASS | GET on the hangup route |  |
| `realtime.routing.put_live` | PASS | PUT is not registered on /v1/live |  |
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
| `mgmt.config.v8_historical_paths` | PASS | historical oauth.providers paths read, merge and delete the shared upstream settings |  |
| `mgmt.config.v8_historical_client_path` | PASS | historical spellings of client.codex.optimize-multi-agent-v2 |  |
| `mgmt.config.v8_historical_body` | PASS | historical oauth.providers bodies on PATCH and PUT of the whole config |  |
| `mgmt.config.v8_flow_style` | PASS | flow collections of YAML and JSON bodies survive the write, also when historical paths move |  |
| `mgmt.config.v8_flow_moved` | PASS | flow containers and quoted values moved from historical paths to the canonical ones |  |
| `mgmt.config.v8_unknown_fields` | PASS | nested unknown fields are reported with yaml.v3's line and Go type |  |
| `mgmt.config.v8_upstream_invalid` | PASS | invalid shared upstream values are rejected at both paths |  |
| `mgmt.config.v8_api_keys_auth_index` | PASS | auth_index is injected into v8 api-keys reads and never persisted |  |
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
| `plugins.mgmt.list_simple` | PASS | GET /plugins with the all-capability example loaded |  |
| `plugins.mgmt.list_many` | PASS | GET /plugins with several capability plugins |  |
| `plugins.mgmt.config_edit` | PASS | plugin config GET, PUT, PATCH and enabled toggle |  |
| `plugins.mgmt.delete` | PASS | DELETE removes the plugin file and its config |  |
| `plugins.mgmt.quota_without_provider` | PASS | plugin quota endpoints without a quota provider |  |
| `plugins.mgmt.auth_url_plugin_provider` | PASS | login URL of a plugin auth provider (v0 and v8 routes) and the pending session |  |
| `plugins.resource.management_api` | PASS | plugin resource served without authentication, and unknown resources |  |
| `plugins.resource.host_callback` | PASS | plugin resource that calls the host logger |  |
| `plugins.resource.host_auth_files` | PASS | plugin resource over the host auth file callbacks |  |
| `plugins.resource.host_model_callback` | PASS | plugin resource that runs a model request through the host |  |
| `plugins.models.static_models` | PASS | models registered by a model plugin show up in the listings |  |
| `plugins.models.executor_models` | PASS | an executor plugin registers its provider models |  |
| `plugins.models.auth_models` | PASS | a plugin credential gets the models of its provider; the credential is listed |  |
| `plugins.auth.parse_file` | PASS | a credential file owned by a plugin auth provider is parsed by the plugin |  |
| `plugins.access.frontend_auth` | PASS | a frontend auth provider joins the client key check |  |
| `plugins.access.frontend_auth_exclusive` | PASS | an exclusive frontend auth provider replaces the other providers |  |
| `plugins.access.frontend_auth_exclusive_realtime` | PASS | an exclusive frontend auth provider guards the realtime routes |  |
| `plugins.translate.request_normalizer_chat` | PASS | request_normalizer plugin on a chat request routed to the compat upstream |  |
| `plugins.translate.request_normalizer_claude` | PASS | request_normalizer plugin on a Claude request routed to the Claude upstream |  |
| `plugins.translate.request_translator_chat` | PASS | request_translator plugin on a chat request routed to the compat upstream |  |
| `plugins.translate.request_translator_claude` | PASS | request_translator plugin on a Claude request routed to the Claude upstream |  |
| `plugins.translate.response_normalizer_chat` | PASS | response_normalizer plugin on a chat request routed to the compat upstream |  |
| `plugins.translate.response_normalizer_claude` | PASS | response_normalizer plugin on a Claude request routed to the Claude upstream |  |
| `plugins.translate.response_translator_chat` | PASS | response_translator plugin on a chat request routed to the compat upstream |  |
| `plugins.translate.response_translator_claude` | PASS | response_translator plugin on a Claude request routed to the Claude upstream |  |
| `plugins.translate.thinking_chat` | PASS | thinking plugin on a chat request routed to the compat upstream |  |
| `plugins.translate.thinking_claude` | PASS | thinking plugin on a Claude request routed to the Claude upstream |  |
| `plugins.translate.response_normalizer_stream` | PASS | response normalizer on a streaming chat |  |
| `plugins.translate.codex_service_tier` | PASS | service tier normalizer on a Codex request |  |
| `plugins.translate.two_plugins` | PASS | request and response normalizers together |  |
| `plugins.exec.executor` | PASS | a plugin executor serves its own model (plain and streaming) |  |
| `plugins.exec.executor_with_auth` | PASS | a plugin executor serving a request with a plugin credential |  |
| `plugins.exec.executor_claude_entry` | PASS | a Claude-dialect request routed to the plugin executor |  |
| `plugins.exec.thinking_suffix` | PASS | a thinking suffix on a plugin model reaches the plugin thinking applier |  |
| `plugins.exec.scheduler_delegate` | PASS | a scheduler plugin delegating to the fill-first selector |  |
| `plugins.exec.scheduler_round_robin` | PASS | a scheduler plugin delegating to the round-robin selector |  |
| `plugins.exec.scheduler_deny` | PASS | a scheduler plugin that rejects every pick |  |
| `plugins.exec.usage_plugin` | PASS | a usage plugin observing a request |  |
| `plugins.router.codex_web_search` | PASS | a model router sends Claude web_search requests to the plugin executor, which runs them through the host on Codex |  |
| `plugins.router.default_provider` | PASS | a model router pinning web_search requests to a built-in provider |  |
| `plugins.lifecycle.reject_keyword` | PASS | request interceptor terminating a request with a custom response |  |
| `plugins.lifecycle.slot_release` | PASS | completion events release the interceptor's concurrency slots |  |
| `plugins.lifecycle.reject_keyword_stream` | PASS | request interceptor terminating a streaming request |  |
| `plugins.format.models` | PASS | the plugin's model is listed |  |
| `plugins.format.custom_output` | PASS | a chat request to an executor with a custom output format is translated by the plugin (plain and streaming) |  |
| `plugins.format.claude_entry` | PASS | a Claude-dialect request to the same executor |  |
| `plugins.hostcb.reset_cooldown` | PASS | a plugin resets a credential's cooldown through the host callback |  |
| `redis.auth.commands` | PASS | NOAUTH gate, AUTH argument handling and unknown commands |  |
| `redis.auth.ip_ban` | PASS | five failed attempts ban the client on the Redis path too |  |
| `redis.auth.noauth_counts_failures` | PASS | unauthenticated commands count as failed attempts |  |
| `redis.disabled.no_management` | PASS | without a management secret the RESP connection is closed |  |
| `redis.protocol.errors` | PASS | malformed frames answer ERR and close |  |
| `redis.pop.usage` | PASS | LPOP/RPOP usage after proxied requests |  |
| `redis.pop.arguments` | PASS | LPOP/RPOP argument and channel validation |  |
| `redis.subscribe.usage` | PASS | SUBSCRIBE usage: support refresh, live records, PING, UNSUBSCRIBE |  |
| `redis.subscribe.arguments` | PASS | SUBSCRIBE validation, channel case, QUIT while subscribed |  |
| `redis.subscribe.errors` | PASS | SUBSCRIBE errors: upstream failures arrive as error events |  |
| `redis.mux.http_and_resp` | PASS | HTTP and RESP clients share the port; the queue feeds both consumers |  |
| `redis.usage.cached.claude` | PASS | queued usage of cached-token responses, claude upstream, json and stream |  |
| `redis.usage.cached.codex` | PASS | queued usage of cached-token responses, codex upstream, json and stream |  |
| `redis.usage.cached.gemini` | PASS | queued usage of cached-token responses, gemini upstream, json and stream |  |
| `redis.usage.cached.compat` | PASS | queued usage of cached-token responses, compat upstream, json and stream |  |
| `redis.usage.failed.claude` | PASS | queued usage of failed upstream calls, claude upstream, json and stream |  |
| `redis.usage.failed.codex` | PASS | queued usage of failed upstream calls, codex upstream, json and stream |  |
| `redis.usage.failed.gemini` | PASS | queued usage of failed upstream calls, gemini upstream, json and stream |  |
| `redis.usage.failed.compat` | PASS | queued usage of failed upstream calls, compat upstream, json and stream |  |
| `redis.usage_disabled` | PASS | no usage records are queued with usage-statistics-enabled off |  |
| `media.images.codex.gen_json` | PASS | codex image generation, options pass through |  |
| `media.images.codex.gen_default_model` | PASS | no model defaults to gpt-image-2 |  |
| `media.images.codex.gen_model_variants` | PASS | gpt-image-1.5 and 2.5 models |  |
| `media.images.codex.gen_stream_false` | PASS | stream false drops the flag |  |
| `media.images.codex.gen_stream` | PASS | codex image generation stream relayed |  |
| `media.images.codex.gen_stream_string_flag` | PASS | stream given as the string true |  |
| `media.images.codex.gen_stream_cut_clean` | PASS | stream ends without the completed event |  |
| `media.images.codex.gen_stream_cut_abort` | PASS | upstream drops the stream |  |
| `media.images.codex.gen_stream_error_event` | PASS | in-band error event mid stream |  |
| `media.images.codex.gen_stream_http_error` | PASS | stream request fails before data |  |
| `media.images.codex.gen_401` | PASS | upstream 401 |  |
| `media.images.codex.gen_400` | PASS | upstream 400 |  |
| `media.images.codex.gen_500_failover` | PASS | 500 then success on the second key |  |
| `media.images.codex.gen_429_all` | PASS | every key rate limited |  |
| `media.images.codex.gen_rr` | PASS | round robin across codex keys |  |
| `media.images.codex.gen_non_json_answer` | PASS | upstream answers plain text |  |
| `media.images.codex.gen_passthrough_headers` | PASS | upstream headers are relayed with passthrough |  |
| `media.images.codex.gen_client_headers` | PASS | client Codex headers reach the upstream, user agent does not |  |
| `media.images.codex.edits_json` | PASS | edit with JSON body |  |
| `media.images.codex.edits_json_stream` | PASS | edit stream with JSON body |  |
| `media.images.codex.edits_multipart` | PASS | edit with image, mask and options as multipart |  |
| `media.images.codex.edits_multipart_multi` | PASS | edit with image[] files and no content type |  |
| `media.images.codex.edits_multipart_stream` | PASS | edit stream with multipart body |  |
| `media.images.codex.edits_multipart_stream_yes` | PASS | stream flag spelled yes |  |
| `media.images.codex.edits_error` | PASS | edit upstream 500 on every key |  |
| `media.images.codex.no_retry_500` | PASS | single attempt, upstream 500 |  |
| `media.images.codex_tool.gen_json` | PASS | xAI options become image tool fields |  |
| `media.images.codex_tool.gen_url` | PASS | response_format url builds data urls |  |
| `media.images.codex_tool.gen_stream` | PASS | the stream is synthesized from one call |  |
| `media.images.codex_tool.gen_cut_clean` | PASS | answer ends before completion |  |
| `media.images.codex_tool.gen_cut_abort` | PASS | upstream drops the connection |  |
| `media.images.codex_tool.gen_error_event` | PASS | failed event mid answer |  |
| `media.images.codex_tool.gen_no_image` | PASS | completed without an image call |  |
| `media.images.codex_tool.gen_500` | PASS | upstream 500 on every key |  |
| `media.images.codex_tool.gen_401` | PASS | upstream 401 |  |
| `media.images.codex_tool.gen_400` | PASS | upstream 400 |  |
| `media.images.codex_tool.gen_client_headers` | PASS | client Codex headers and user agent reach the upstream |  |
| `media.images.codex_tool.edits_json` | PASS | edit with the image as a string |  |
| `media.images.codex_tool.edits_json_images` | PASS | edit with several images |  |
| `media.images.codex_tool.edits_json_stream` | PASS | edit stream |  |
| `media.images.codex_tool.edits_multipart` | PASS | edit with uploaded images |  |
| `media.images.codex_tool.base_model_config` | PASS | gpt-image-2-base-model picks the base model |  |
| `media.images.codex_tool.base_model_invalid` | PASS | a base model that is not a gpt model falls back |  |
| `media.images.codex_tool.usage` | PASS | usage records of the base model and of the image tool |  |
| `media.images.codex_tool.usage_failed` | PASS | usage record of a failed image call |  |
| `media.images.codex_tool.payload_rules` | PASS | payload rules apply to the base model request |  |
| `media.images.codex_tool.disable_chat` | PASS | chat mode keeps the image tool on the images endpoint |  |
| `media.responses.image_tool_usage` | PASS | image tool usage is published after the main usage |  |
| `media.images.xai.gen_b64` | PASS | xAI generation returns b64_json |  |
| `media.images.xai.gen_url` | PASS | response_format url |  |
| `media.images.xai.gen_options` | PASS | size, quality and n map to xAI options |  |
| `media.images.xai.gen_aspect` | PASS | aspect ratios and prefixed models |  |
| `media.images.xai.gen_stream` | PASS | stream is synthesized from a normal call |  |
| `media.images.xai.gen_stream_url` | PASS | stream with url format |  |
| `media.images.xai.gen_empty_data` | PASS | upstream returns no images |  |
| `media.images.xai.gen_invalid_answer` | PASS | upstream returns invalid JSON |  |
| `media.images.xai.gen_stream_empty_data` | PASS | stream with an empty upstream answer |  |
| `media.images.xai.gen_data_only_urls` | PASS | answer with only urls and a data url conversion |  |
| `media.images.xai.gen_401` | PASS | upstream 401 |  |
| `media.images.xai.gen_stream_401` | PASS | stream, upstream 401 |  |
| `media.images.xai.gen_500_failover` | PASS | 500 then success on the second xAI key |  |
| `media.images.xai.gen_rr` | PASS | round robin across xAI keys |  |
| `media.images.xai.edits_json_string` | PASS | edit with an image string |  |
| `media.images.xai.edits_json_object` | PASS | edit with image_url object |  |
| `media.images.xai.edits_json_images` | PASS | edit with several images |  |
| `media.images.xai.edits_json_no_image` | PASS | edit without any image |  |
| `media.images.xai.edits_json_stream` | PASS | edit stream |  |
| `media.images.xai.edits_multipart` | PASS | edit with uploaded images |  |
| `media.images.xai.edits_multipart_multi` | PASS | edit with two uploaded images |  |
| `media.images.xai.edits_multipart_stream` | PASS | edit stream from multipart |  |
| `media.images.compat.gen_json` | PASS | compat image generation re-shaped |  |
| `media.images.compat.gen_url` | PASS | compat generation, url format |  |
| `media.images.compat.gen_stream` | PASS | compat stream relayed |  |
| `media.images.compat.gen_stream_error` | PASS | compat stream with an in-band error |  |
| `media.images.compat.gen_stream_cut_clean` | PASS | compat stream ends early |  |
| `media.images.compat.gen_stream_http_error` | PASS | compat stream fails before data |  |
| `media.images.compat.gen_500` | PASS | compat upstream 500 |  |
| `media.images.compat.gen_empty_data` | PASS | compat answer without images |  |
| `media.images.compat.edits_json` | PASS | compat edit with JSON body |  |
| `media.images.compat.edits_multipart` | PASS | compat edit with multipart body |  |
| `media.images.compat.edits_multipart_stream` | PASS | compat edit stream |  |
| `media.images.invalid.gen_not_json` | PASS | generations body is not JSON |  |
| `media.images.invalid.gen_unsupported_model` | PASS | chat model on the images endpoint |  |
| `media.images.invalid.gen_unknown_prefix` | PASS | unknown provider prefix on an image model |  |
| `media.images.invalid.gen_prefixed_model` | PASS | provider prefix on a codex image model |  |
| `media.images.invalid.gen_no_prompt` | PASS | prompt missing |  |
| `media.images.invalid.gen_blank_prompt` | PASS | prompt blank |  |
| `media.images.invalid.chat_model_image_only` | PASS | image model on chat completions |  |
| `media.images.invalid.edits_not_json` | PASS | edit JSON body invalid |  |
| `media.images.invalid.edits_unsupported_model` | PASS | edit with a chat model |  |
| `media.images.invalid.edits_no_prompt_json` | PASS | edit JSON without prompt |  |
| `media.images.invalid.edits_mp_unsupported_model` | PASS | multipart edit with a chat model |  |
| `media.images.invalid.edits_mp_no_prompt` | PASS | multipart edit without prompt |  |
| `media.images.invalid.edits_mp_no_image` | PASS | multipart edit without image |  |
| `media.images.invalid.edits_mp_bad_stream_flag` | PASS | unparseable stream flag counts as false |  |
| `media.images.invalid.edits_text_plain` | PASS | edit with an unsupported content type |  |
| `media.images.invalid.edits_multipart_no_boundary` | PASS | multipart edit without a boundary |  |
| `media.images.invalid.edits_no_content_type` | PASS | edit without a content type |  |
| `media.images.invalid.gen_missing_auth` | PASS | images endpoint needs a client key |  |
| `media.images.invalid.edits_missing_auth` | PASS | edits endpoint needs a client key |  |
| `media.images.mode.all_gen` | PASS | disable-image-generation true: generations is a 404 |  |
| `media.images.mode.all_edits` | PASS | disable-image-generation true: edits is a 404 |  |
| `media.images.mode.all_invalid_first` | PASS | disabled endpoint answers 404 before validation |  |
| `media.images.mode.chat_gen` | PASS | chat mode keeps the images endpoints |  |
| `media.images.mode.chat_edits` | PASS | chat mode keeps image edits |  |
| `media.images.mode.chat_responses_tool` | PASS | chat mode strips the image tool from responses |  |
| `media.images.keepalive.gen` | PASS | non-stream keep-alive newlines before a slow image answer |  |
| `media.images.keepalive.stream` | PASS | stream keep-alive before the first event |  |
| `media.images.keepalive.xai_stream` | PASS | xAI stream keep-alive while the call runs |  |
| `media.images.keepalive.stream_error` | PASS | stream keep-alive then upstream error |  |
| `media.images.no_retry.xai_500` | PASS | xAI 500 without retries |  |
| `media.videos.native.create_generations` | PASS | native video generation |  |
| `media.videos.native.create_root` | PASS | POST /v1/videos is a generation |  |
| `media.videos.native.create_edits` | PASS | native video edit |  |
| `media.videos.native.create_extensions` | PASS | native video extension |  |
| `media.videos.native.create_preview_alias` | PASS | preview alias is normalized in the payload |  |
| `media.videos.native.retrieve` | PASS | native retrieve |  |
| `media.videos.native.retrieve_states` | PASS | native retrieve of failed and pending videos |  |
| `media.videos.native.create_then_retrieve` | PASS | retrieve reuses the credential that created the video |  |
| `media.videos.native.unsupported_model` | PASS | sora is not a native xAI model |  |
| `media.videos.native.foreign_prefix` | PASS | foreign provider prefix rejected |  |
| `media.videos.native.not_json` | PASS | native body is not JSON |  |
| `media.videos.native.upstream_500` | PASS | native create upstream 500 on every key |  |
| `media.videos.native.upstream_401` | PASS | native create upstream 401 |  |
| `media.videos.native.retrieve_404` | PASS | native retrieve upstream 404 |  |
| `media.videos.native.missing_auth` | PASS | videos need a client key |  |
| `media.videos.native.passthrough_headers` | PASS | upstream headers relayed with passthrough |  |
| `media.videos.native.ttl_expired` | PASS | binding expires with a tiny ttl |  |
| `media.videos.native.ttl_invalid` | PASS | invalid ttl falls back to the default |  |
| `media.videos.openai.create_json` | PASS | OpenAI-shaped create |  |
| `media.videos.openai.create_defaults` | PASS | create with defaults |  |
| `media.videos.openai.create_options` | PASS | size, aspect ratio, resolution and clamped seconds |  |
| `media.videos.openai.create_input_reference` | PASS | image reference |  |
| `media.videos.openai.create_references` | PASS | reference images |  |
| `media.videos.openai.create_form_multipart` | PASS | multipart form create |  |
| `media.videos.openai.create_form_urlencoded` | PASS | urlencoded form create |  |
| `media.videos.openai.create_errors` | PASS | request validation errors become failed video resources |  |
| `media.videos.openai.create_unsupported_model` | PASS | model not supported on the OpenAI route |  |
| `media.videos.openai.create_not_json` | PASS | create body is not JSON |  |
| `media.videos.openai.create_upstream_500` | PASS | create upstream 500 |  |
| `media.videos.openai.create_upstream_429` | PASS | create upstream 429 on every key |  |
| `media.videos.openai.create_no_request_id` | PASS | upstream answers without a request id |  |
| `media.videos.openai.create_status_mapping` | PASS | upstream status and progress are mapped |  |
| `media.videos.openai.retrieve_done` | PASS | retrieve of a finished video |  |
| `media.videos.openai.retrieve_states` | PASS | retrieve of failed, pending and error shapes |  |
| `media.videos.openai.create_then_retrieve` | PASS | retrieve is bound to the creating credential |  |
| `media.videos.openai.retrieve_404` | PASS | retrieve upstream 404 |  |
| `media.videos.openai.retrieve_encoded_id` | PASS | blank id is rejected |  |
| `media.videos.openai.content` | PASS | video download |  |
| `media.videos.openai.content_variants` | PASS | variant query |  |
| `media.videos.openai.content_problems` | PASS | content with a missing url, bad url or missing file |  |
| `media.videos.openai.content_upstream_500` | PASS | content, poll fails upstream |  |
| `media.videos.openai.ttl_expired` | PASS | binding expires, retrieve rotates keys |  |
| `media.videos.openai.passthrough_headers` | PASS | retrieve relays upstream headers with passthrough |  |
| `media.videos.openai.keepalive` | PASS | non-stream keep-alive on a slow create |  |
| `media.search.v1_ok` | PASS | alpha search forwarded with cache fields removed |  |
| `media.search.codex_alias_path` | PASS | same endpoint under /backend-api/codex |  |
| `media.search.untouched_body` | PASS | body without cache fields is forwarded as sent |  |
| `media.search.client_headers` | PASS | selected client headers are forwarded |  |
| `media.search.not_json` | PASS | non-JSON body is forwarded verbatim |  |
| `media.search.upstream_errors` | PASS | upstream status and body are relayed |  |
| `media.search.upstream_text` | PASS | upstream non-JSON answer |  |
| `media.search.rr_same_key` | PASS | only the alpha-search key is used |  |
| `media.search.no_eligible_key` | PASS | no key allows alpha search |  |
| `media.search.model_alias` | PASS | alias resolved to the upstream model |  |
| `media.search.unknown_model` | PASS | model without a matching credential |  |
| `media.search.missing_auth` | PASS | alpha search needs a client key |  |
| `media.search.wrong_method` | PASS | GET is not routed |  |
| `media.search.routed_model` | PASS | a router maps the requested model to a Codex model |  |
| `media.search.routed_provider_only` | PASS | a router naming the codex provider without a model keeps the requested one |  |
| `media.search.routed_unsupported_provider` | PASS | a router pointing at another provider is rejected |  |
| `media.search.routed_unsupported_self` | PASS | a router pointing at its own executor is rejected |  |
| `media.search.routed_unhandled` | PASS | a router that declines leaves the request alone |  |
| `rust.smart_quota.claude` | PASS | smart-quota: a cold session lands on key 1 (tie), then new sessions avoid it (90% used) while the bound session stays |  |
