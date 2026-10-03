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

Verified against the Go implementation:

- **Translators:** 5,684 / 5,684 golden cases, captured from the Go test suite and replayed through the Go code (`cargo run --release -p cpa-conformance`).
- **End to end:** 1,002 / 1,002 HTTP, websocket, realtime, media, request-log, Home/Redis and plugin scenarios against a mock upstream, recorded from the Go binary ([report](conformance/e2e/report.md), `tools/e2e.sh`).

## Performance

Go reference vs this port, same config, same zero-latency mock upstream, 64 concurrent connections, median of 7 runs on an 8 logical CPU pin (AMD Ryzen 7 7800X3D, shared WSL2 host, so spread is large; see [bench/results.md](bench/results.md) for every cell, spread, latency percentiles and load averages).

| Scenario | Go req/s | Rust req/s | Rust/Go | p50 ms Go / Rust | CPU per request Rust/Go |
|---|---|---|---|---|---|
| OpenAI chat, passthrough | 8,322 | 7,890 | 0.95x | 6.9 / 7.8 | 1.11x |
| OpenAI chat SSE to Claude upstream | 3,978 | 2,680 | 0.67x | 15.5 / 21.2 | 1.57x |
| Responses SSE to Codex upstream | 3,503 | 2,082 | 0.59x | 17.1 / 27.3 | 1.20x |
| Claude messages to Gemini upstream | 5,969 | 7,013 | 1.17x | 8.3 / 8.6 | 0.95x |
| `GET /v1/models` | 22,206 | 30,482 | 1.37x | 2.2 / 2.0 | 0.80x |

| | Go | Rust |
|---|---|---|
| Idle RSS | 53 MB | 21 MB |
| Startup to healthy | 59 ms | 31 ms |
| Binary (stripped) | 64.9 MiB | 47.9 MiB |
| 2 MB conversation, chat to Claude, p50 | 262 ms | 364 ms |
| 2 MB conversation, Claude to Gemini, p50 | 118 ms | 281 ms |

Streaming adds about 1 to 2 ms to time to first byte on both servers. Reproduce with `bench/scripts/build.sh && bench/scripts/run.sh` ([methodology](bench/README.md)).

## License

MIT, including the original CLIProxyAPI notice.
