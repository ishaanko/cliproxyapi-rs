# cliproxyapi-rs

A Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI): one local endpoint that speaks the OpenAI, Claude, Gemini and Codex APIs and routes requests to your CLI subscriptions (Claude Code, Codex, Antigravity, Kimi, xAI, ...) and API keys, with failover, cooldowns and a management UI.

![Overview](docs/screenshots/overview-demo.png)

## Run

```sh
cd ui && bun install && bun run build && cd ..
cargo build --release
./target/release/cliproxy --config config.yaml
```

Building needs `cmake`, a C++ compiler and libclang: the Claude Code and Chrome TLS fingerprints (`crates/tlsfp`) compile BoringSSL from source. If bindgen cannot find libclang, point `LIBCLANG_PATH` at the directory holding `libclang.so` (for example `uv pip install --target ~/.local/share/libclang libclang`, then `LIBCLANG_PATH=~/.local/share/libclang/clang/native`) and, if it also misses compiler headers, set `BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/<version>/include`. Without them, `cargo build --release -p cpa-server --no-default-features` drops the fingerprints (plain rustls, no BoringSSL).

`config.example.yaml` documents every option; existing CLIProxyAPI configs and auth directories work unchanged. Log in to a provider with `--claude-login`, `--codex-login`, `--antigravity-login` and friends, or from the UI at `http://localhost:8317/management.html`.

Development builds: the release profile is fat LTO with one codegen unit (slow to link, fastest binary). `cargo build --profile release-fast` (thin LTO, 16 codegen units) rebuilds much faster and is what `CPA_PROFILE=release-fast tools/e2e.sh` uses while iterating; run the final E2E gate, benchmarks and `tools/pgo.sh` on the plain release profile. `.cargo/config.toml` sets `CFLAGS_<target>` for the mimalloc build (lazy arena commit, see `crates/server/src/main.rs`); an exported `CFLAGS_<target>` of your own takes precedence over it.

## Compatibility

Verified against the Go implementation at upstream commit [`d7914af`](https://github.com/router-for-me/CLIProxyAPI/commit/d7914af) (2026-10-03):

- **Translators:** 5,839 / 5,839 golden cases, captured from the Go test suite and replayed through the Go code (`cargo run --release -p cpa-conformance`).
- **End to end:** 1,068 / 1,068 HTTP, websocket, realtime, media, request-log, Home/Redis and plugin scenarios against a mock upstream, recorded from the Go binary ([report](conformance/e2e/report.md), `tools/e2e.sh`).

## Performance

Go reference vs this port, same config, same zero-latency mock upstream, median of 7 runs with nothing else running, server pinned to 8 logical CPUs (AMD Ryzen 7 7800X3D, WSL2). Every cell, spread, percentile and load average is in [bench/results.md](bench/results.md).

Throughput at 64 concurrent connections:

| Scenario | Go req/s | Rust req/s | Rust/Go | p99 ms Go / Rust | CPU per request Rust/Go |
|---|---|---|---|---|---|
| OpenAI chat, passthrough | 8,463 | 21,534 | 2.54x | 23.1 / 4.5 | 0.42x |
| OpenAI chat SSE to Claude upstream | 4,592 | 10,429 | 2.27x | 29.5 / 9.6 | 0.42x |
| Responses SSE to Codex upstream | 3,970 | 8,590 | 2.16x | 35.3 / 11.5 | 0.47x |
| Claude messages to Gemini upstream | 7,831 | 17,030 | 2.17x | 34.6 / 5.9 | 0.41x |
| `GET /v1/models` | 27,702 | 51,769 | 1.87x | 9.3 / 2.1 | 0.54x |

Per request, on all 18 benchmark scenarios (long streams, native Claude and Gemini streams, Responses over websocket, 250 KB agent requests, 1024 requests in flight against a slow upstream): Rust uses 0.17x to 0.51x of Go's CPU and 0.09x to 0.69x of its instructions, with lower peak RSS than Go at the benchmark concurrency ([per-scenario table](bench/results.md#per-request-metrics-all-scenarios)).

| | Go | Rust |
|---|---|---|
| Idle RSS | 53 MB | 35 MB |
| Startup to healthy | 55 ms | 16 ms |
| 2 MB conversation, chat to Claude, p50 | 266 ms | 39 ms |
| 2 MB conversation, Claude to Gemini, p50 | 116 ms | 33 ms |
| Request errors during the whole run | 1,116 | 1 |

Streaming adds under 1 ms to time to first byte on both servers. Reproduce with `bench/scripts/build.sh && tools/exclusive.sh bench/scripts/run.sh` ([methodology](bench/README.md)).

## License

MIT, including the original CLIProxyAPI notice.
