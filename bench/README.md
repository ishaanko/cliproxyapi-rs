# Benchmark: Go vs Rust

Compares the Go reference (CLIProxyAPI) with the Rust port under identical configs and the same mock upstream. Results are in [results.md](results.md); raw samples in `results/raw.json`.

## Rules of the machine

The host is shared and has only about 4000 ephemeral ports (`ip_local_port_range`), and every closed connection leaves a TIME-WAIT socket for 60 s.

- Anything that generates load (`run.sh`, `quick.sh`, any `cpa-bench` subcommand except `report`) goes through `tools/exclusive.sh`, which also holds the lock until TIME-WAIT drains. Builds and tests go through `tools/shared.sh`.
- The load generator is port-frugal: one keep-alive connection per concurrent worker, never one per request, and the connections are pooled per destination and reused across warmup, windows, cells and scenarios (`load.rs`, `Pool`). A run opens about `max concurrency` client connections per server plus the server's own upstream connections (about 2 x 1024 with the default 1024 ceiling). The only per-use connections are websocket scenarios (about 100 per server run). The pool is cleared when a server stops. Measured TIME-WAIT left behind (host baseline subtracted): `quick.sh --all` (18 scenarios, both servers, 14 minutes) about 3100 at the end, a two-server `run` subset reaching concurrency 1024 about 2100, one slow-upstream `quick` at 1024 with the alloc pass about 3000.
- Measure on the plain `release` profile (fat LTO, one codegen unit) only. The `release-fast` profile (thin LTO, 16 codegen units; `CPA_PROFILE=release-fast tools/e2e.sh`) exists for quick functional iteration and its CPU and RSS numbers are not comparable.
- Both `run` and `quick` pick free ports at startup, so they never collide with other processes.

## Reproduce

```sh
bench/scripts/build.sh          # Go release build (-trimpath -ldflags "-s -w") + cargo build --release
tools/exclusive.sh bench/scripts/run.sh            # full run, writes results/raw.json and results.md
tools/exclusive.sh bench/scripts/run.sh --runs 2 --conc 1,64 --only chat-compat   # quick subset
```

## Quick mode: deterministic metrics while iterating

`bench/scripts/quick.sh` runs one scenario with a fixed request count (not a time window) against Go and Rust (or `--rust-only`) and prints one table in well under a minute:

```sh
tools/exclusive.sh bench/scripts/quick.sh --list                       # scenarios, default concurrency
tools/exclusive.sh bench/scripts/quick.sh chat-claude-stream           # Go vs Rust
tools/exclusive.sh bench/scripts/quick.sh chat-claude-stream --rust-only --conc 64 --requests 6000 --reps 3
tools/exclusive.sh bench/scripts/quick.sh --all --reps 2               # every scenario + a markdown summary table
```

It builds the release binaries first (`--no-build` to skip; the Go reference is built once into `$CPA_TMP/bench-go` and shared by all worktrees), starts a fresh server per measurement, warms it up, and runs `--reps` fixed-count windows (median shown). Columns:

| Column | Meaning | Load sensitivity |
|---|---|---|
| `cpu us/req` | server CPU time per request: task-clock counter (ns resolution; `/proc/<pid>/stat` utime+stime is the fallback and agrees) | low (a little SMT/cache noise when the host is busy) |
| `instr/req` | user-space instructions per request, `perf_event_open` on the server pid with `inherit` | very low (about 0.3% run to run for Rust, 1.5% for Go) |
| `ctxsw/req` | voluntary + involuntary context switches, summed from `/proc/<pid>/task/*/status` | medium |
| `allocs/req`, `alloc KB/r` | allocations and bytes requested per request, Rust only (see below) | none |
| `idle`, `peak`, `end MB` | RSS a second after start, kernel high-water mark during the window, RSS at the end | low |
| `KB/inflt` | (peak - idle) RSS divided by concurrency: memory per in-flight request | low |
| `req/s*`, `p50*`, `p99*` | wall clock | high |

Only the starred wall-clock columns depend on machine load and are valid only under `tools/exclusive.sh`. Everything else is the point of this mode: compare it before and after a change, it moves with the code, not with what other agents are compiling. If you must run it under `tools/shared.sh` instead (no other benchmark running), trust instructions and allocations, treat CPU and context switches as +-10%, ignore the starred columns. `quick.sh` never takes the lock itself (a nested lock would deadlock against a pending exclusive run), so wrap it yourself.

Instructions count user space only (`perf_event_paranoid=2` forbids kernel counting); syscall and network-stack cost is in `cpu us/req` but not in `instr/req`. Hardware counters work on this WSL2 kernel; where they are unavailable (no PMU) the column shows `-` and a note is printed. `CPA_BENCH_DEBUG=1` prints the raw counter samples.

Pinning follows `run.sh`: server `0-7`, mock `8-11`, load generator `12-15` (`SERVER_CPUS`, `MOCK_CPUS`, `LOAD_CPUS`; set one empty to leave it unpinned).

### How the counters are attached

The server is started through a tiny gate (`sh -c 'read _; exec taskset ...'`): the harness attaches the perf counters to the shell's pid with `inherit` while it is still blocked, then releases it. Because the counters exist before the program starts, every thread it ever creates is counted. Attaching after spawn races with thread creation and silently undercounts (observed: 10x too few instructions for Go under load).

### Allocations per request (Rust)

The `cpa-allocstats` crate (`crates/allocstats`) is a counting `GlobalAlloc` wrapper over any allocator, with per-thread-sharded counters, and a thread that serves the totals on an abstract unix socket named by `CPA_ALLOC_STATS_SOCK`. It is only linked when `cpa-server` is built with the `alloc-stats` feature, so shipped builds are unchanged. The hook in `crates/server` is two lines (`bench/alloc-stats.patch`):

```rust
// Cargo.toml: [features] alloc-stats = ["dep:cpa-allocstats"]; cpa-allocstats = { path = "../allocstats", optional = true }
#[cfg(feature = "alloc-stats")]
#[global_allocator]
static ALLOC: cpa_allocstats::Counting<std::alloc::System> = cpa_allocstats::Counting::new(std::alloc::System);
// first line of main():
#[cfg(feature = "alloc-stats")]
cpa_allocstats::serve_from_env();
```

If another allocator is chosen (mimalloc, jemalloc), wrap it: `Counting::new(mimalloc::MiMalloc)`. Until the hook lives in `crates/server`, `bench/scripts/build-alloc.sh` applies the patch to a synced copy of the tree (`target/alloc-src`, built into `target/alloc`) and installs `target/bench-bin/cliproxy-alloc`; `quick.sh --alloc` runs it, and whenever that binary exists `quick` adds one extra Rust pass with it (the counting allocator perturbs timing, so CPU and instructions always come from the normal binary). Go allocations are not exposed.

## Scenarios

The five standard ones (throughput/latency, concurrency 1, 16, 64, 256, 1024), the two 2 MB requests, and the additional set below, selected with `--only` (substring of the id) in `run` and by id in `quick`. Each carries its own mock behavior (think time, SSE event count and gap).

| Id | What |
|---|---|
| `chat-claude-stream-long` | chat SSE translated from a Claude upstream stream of 1000 small deltas |
| `chat-compat-stream-long` | chat SSE passthrough to an OpenAI-compatible upstream, 2000 deltas |
| `claude-native-stream` | Claude messages SSE to a native Claude upstream (passthrough-ish), 100 deltas |
| `gemini-native-stream` | `:streamGenerateContent?alt=sse` to a native Gemini upstream, 200 deltas |
| `chat-gemini-stream` | chat SSE translated from Gemini SSE, 200 deltas |
| `responses-ws-codex` | Responses over websocket, one `response.create` per request, Codex upstream |
| `agent-chat-claude-stream`, `agent-claude-native-stream`, `agent-responses-codex-stream` | Claude Code sized requests: 40 tools, tool-call history, streamed |
| `slow-chat-compat-json`, `slow-chat-claude-stream` | upstream think time uniform 200 ms to 2 s (plus 40 deltas 20 ms apart for the stream), 256 to 1024 requests in flight: memory per in-flight request and task overhead |

In `run` these are reported in the "Additional scenarios" table (req/s, p50/p99, CPU, instructions, peak RSS, KB per in-flight request; the last only from concurrency 256 up, where in-flight memory dominates the baseline). `--skip-extra` omits them. The mock listens on `/__ctl?first_ms=&first_max_ms=&gap_us=&chunks=` to change its behavior between scenarios.

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
| Throughput and latency | 5 scenarios x concurrency 1, 16, 64, 256, 1024. p50/p90/p99 are exact percentiles of per-request latency. |
| Streaming overhead | Mock waits 20 ms, then sends 40 text deltas 5 ms apart. Every delta contains its send time (`@@<unix micros>`), which survives translation, so the client computes the age of each chunk on arrival. Reported: time to first byte, time to first token, chunk age p50/p99, as added time over the same request sent directly to the mock. Concurrency 1 and 16. |
| Resources | Startup: spawn until `/v1/models` returns the model list (probed every 2 ms with a 10 ms timeout per probe). Idle RSS: 3 s after healthy. Steady RSS: read at the end of each measured window. Peak RSS: kernel high-water mark (`VmHWM`), reset via `/proc/<pid>/clear_refs` at the start of each window. CPU: user+system time from `/proc/<pid>/stat` over the window divided by completed requests; instructions per request (perf_event_open, user space) and context switches per request alongside it. Binary size: file size (the Go build is stripped by `-s -w`, so the Rust binary is also reported stripped). |
| Large requests | ~2 MB agentic conversation (24 tool definitions, about 100 tool-call round trips with 8 to 32 KB tool results), non-stream. Chat to Claude and Claude to Gemini. Concurrency 1 and 8. |

Scenarios: OpenAI chat non-stream to the compat upstream (passthrough); OpenAI chat SSE to a Claude upstream (translation); OpenAI Responses SSE to a Codex upstream; Claude messages non-stream to a Gemini upstream (translation); `GET /v1/models`.

## Runs and statistics

Each run starts a fresh server, runs every cell, and stops it. There are 7 runs (`--runs`); the order of Go and Rust alternates between runs so drift hits both. The tables show the median over runs and ±(half of the min-max range) as a percentage of the median. Ratios are computed from medians, always Rust / Go. The throughput table also lists the best (highest) run per server and the ratio of bests: interference from other processes on a shared host only ever slows a run down, so the best run approximates an undisturbed one.

## Caveats

- The checked-in results come from one session on a busy shared machine (1-minute load average 3 to 22 sampled at run starts, 16 logical CPUs, other agents compiling). Medians moved by tens of percent between sessions; treat ratios inside roughly 0.8x to 1.25x as a tie. `CPU ms per 1k requests` is far less sensitive to load than req/s.
- The Go server answered about 0.1% of requests at concurrency 16 on `chat-compat-json` with HTTP 500 in several runs (the Rust server did not). Those requests are counted as errors and excluded from the latency and throughput figures. Set `CPA_BENCH_DEBUG=1` to print non-200 statuses. The cause was not investigated.
- Runs on a shared WSL2 machine; other processes add noise even with pinning. `results.md` lists the load average at the start of each run, and the spread column shows the effect.
- Zero-latency loopback upstream exaggerates proxy overhead compared with real use.
- Concurrency 256 can hit the Go server's upstream connection handling (default `net/http` transport idle pool), which is part of what is being measured.
