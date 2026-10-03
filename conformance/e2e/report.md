# E2E differential report

- server: `/home/ishaan/box/cliproxyapirust/.claude/worktrees/agent-a5d8cf4a792bfdd25/target/release/cliproxy`
- config layout: `legacy`
- goldens digest: `f013e36fd87aaa12`
- scenarios: 765 total, 765 passed, 0 failed

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
| `ws.handshake.missing_key` | PASS | upgrade without credentials |  |
| `ws.handshake.invalid_key` | PASS | upgrade with a wrong key |  |
| `ws.handshake.query_key` | PASS | key in the query string |  |
| `ws.handshake.codex_backend_path` | PASS | /backend-api/codex/responses alias |  |
| `ws.handshake.turn_state_echo` | PASS | x-codex-turn-state is echoed on the upgrade response |  |
| `ws.handshake.plain_get` | PASS | GET /v1/responses without an upgrade |  |
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
| `redis.usage_disabled` | PASS | no usage records are queued with usage-statistics-enabled off |  |
