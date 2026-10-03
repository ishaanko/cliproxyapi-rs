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

## Compatibility

Verified against the Go implementation:

- **Translators:** 5,684 / 5,684 golden cases, captured from the Go test suite and replayed through the Go code (`cargo run --release -p cpa-conformance`).
- **End to end:** 899 / 899 HTTP, websocket, realtime and media scenarios against a mock upstream, recorded from the Go binary ([report](conformance/e2e/report.md), `tools/e2e.sh`).

## License

MIT, including the original CLIProxyAPI notice.
