# cliproxyapi-rs

A Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI): one local endpoint that speaks OpenAI, Claude, Gemini and Codex APIs and routes to your CLI subscriptions and API keys.

Work in progress.

## Build

```sh
cargo build --release
```

## Conformance

Translators are tested against outputs recorded from the Go implementation:

```sh
cargo run -p cpa-conformance
```
