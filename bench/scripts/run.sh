#!/usr/bin/env bash
# Runs the benchmark with the load generator pinned, then writes bench/results.md.
# Extra arguments go to `cpa-bench run` (e.g. --runs 3 --conc 1,64 --only chat).
# Env: SERVER_CPUS (default 0-7), MOCK_CPUS (8-11), LOAD_CPUS (12-15).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
BIN="$ROOT/target/bench-bin"
LOAD_CPUS="${LOAD_CPUS:-12-15}"

export CPA_BENCH_LOAD_CPUS="$LOAD_CPUS"
mkdir -p bench/results
taskset -c "$LOAD_CPUS" "$BIN/cpa-bench" run \
  --server-cpus "${SERVER_CPUS:-0-7}" --mock-cpus "${MOCK_CPUS:-8-11}" \
  --go-bin "$BIN/cli-proxy-api-go" --rust-bin "$BIN/cliproxy" \
  --out bench/results/raw.json "$@"
"$BIN/cpa-bench" report --input bench/results/raw.json --out bench/results.md
