# Benchmark: Go vs Rust

Compares the Go reference (CLIProxyAPI) with the Rust port under identical configs and the same mock upstream. Results are in [results.md](results.md); raw samples in `results/raw.json`.

## Reproduce

```sh
bench/scripts/build.sh          # Go release build (-trimpath -ldflags "-s -w") + cargo build --release
bench/scripts/run.sh            # full run (about an hour), writes results/raw.json and results.md
bench/scripts/run.sh --runs 2 --conc 1,64 --only chat-compat   # quick subset
```

`build.sh` expects the Go checkout and toolchain under `$CPA_TMP` (default `/home/ishaan/box/cliproxyapirust/tmp`, override with `CPA_TMP`, `GO_SRC`, `GO_TOOLCHAIN`, `GOPATH`). Everything is driven by the `cpa-bench` binary (`crates/bench`): `run` (orchestrator), `report` (raw JSON to markdown), `mock` (the upstream).

## Setup

- Three processes, pinned with `taskset` to disjoint CPUs: the server under test (`0-7`, 4 physical cores), the mock upstream (`8-11`) and the load generator (`12-15`). Go reads its CPU count from the affinity mask (GOMAXPROCS) and tokio sizes its pool from it, so both servers get the same 8 logical CPUs.
- Both servers get the same `config.yaml` (legacy flat layout, both accept it): two keys for each of Claude, Codex, Gemini and one OpenAI-compatible provider, `request-log: false`, `usage-statistics-enabled: false`, `logging-to-file: false`, `debug: false`, `--local-model`, no management panel. stdout and stderr go to `/dev/null`; both still format their per-request info log line, which cannot be turned off in the Go server.
- The mock (`crates/bench/src/mock.rs`) is a minimal hyper server: no logging, no body parsing beyond a scan for `"stream":true`, canned replies in each family's wire format. It runs on its own cores and its ceiling is measured every run (the `direct` rows send the same request straight to it, typically 5 to 20 times the proxies' throughput).
- Upstream latency is zero for the throughput, latency, resource and large-request tests, which isolates proxy overhead (the worst case for the proxy; real LLM upstreams take seconds).
- The load generator is closed loop: N keep-alive connections, each sending the next request when the previous response is fully read. Warmup and measured windows are separate; only requests that start and finish inside the window count.
- Before measuring, every scenario is sent once to each server and must return 200 with the expected content, so error paths are never benchmarked. Errors during measured windows are counted and reported.

## What is measured

| Test | How |
|---|---|
| Throughput and latency | 5 scenarios x concurrency 1, 16, 64, 256. p50/p90/p99 are exact percentiles of per-request latency. |
| Streaming overhead | Mock waits 20 ms, then sends 40 text deltas 5 ms apart. Every delta contains its send time (`@@<unix micros>`), which survives translation, so the client computes the age of each chunk on arrival. Reported: time to first byte, time to first token, chunk age p50/p99, as added time over the same request sent directly to the mock. Concurrency 1 and 16. |
| Resources | Startup: spawn until `/v1/models` returns the model list. Idle RSS: 3 s after healthy. Steady RSS: read at the end of each measured window. Peak RSS: kernel high-water mark (`VmHWM`), reset via `/proc/<pid>/clear_refs` at the start of each window. CPU: user+system time from `/proc/<pid>/stat` over the window divided by completed requests. Binary size: file size (the Go build is stripped by `-s -w`, so the Rust binary is also reported stripped). |
| Large requests | ~2 MB agentic conversation (24 tool definitions, about 100 tool-call round trips with 8 to 32 KB tool results), non-stream. Chat to Claude and Claude to Gemini. Concurrency 1 and 8. |

Scenarios: OpenAI chat non-stream to the compat upstream (passthrough); OpenAI chat SSE to a Claude upstream (translation); OpenAI Responses SSE to a Codex upstream; Claude messages non-stream to a Gemini upstream (translation); `GET /v1/models`.

## Runs and statistics

Each run starts a fresh server, runs every cell, and stops it. There are 5 runs; the order of Go and Rust alternates between runs so drift hits both. The tables show the median over runs and ±(half of the min-max range) as a percentage of the median. Ratios are computed from medians, always Rust / Go.

## Caveats

- Runs on a shared WSL2 machine; other processes add noise even with pinning. `results.md` lists the load average at the start of each run, and the spread column shows the effect.
- Zero-latency loopback upstream exaggerates proxy overhead compared with real use.
- Concurrency 256 can hit the Go server's upstream connection handling (default `net/http` transport idle pool), which is part of what is being measured.
