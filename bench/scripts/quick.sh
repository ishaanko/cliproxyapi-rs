#!/usr/bin/env bash
# Quick deterministic benchmark: one scenario, fixed request count, under a minute.
# Prints CPU/request, instructions/request, allocations/request, context switches/request,
# RSS and (wall-clock, only valid under tools/exclusive.sh) req/s, p50, p99.
#
#   tools/shared.sh bench/scripts/quick.sh chat-claude-stream
#   tools/shared.sh bench/scripts/quick.sh chat-claude-stream --rust-only --conc 64 --requests 6000
#   tools/shared.sh bench/scripts/quick.sh --list
#
# Options handled here (the rest goes to `cpa-bench quick`, see `--help`):
#   --rust-only   skip the Go server
#   --no-build    use the binaries already in target/bench-bin
#   --alloc       also build/run the alloc-stats Rust build for allocations per request
# Env: SERVER_CPUS (0-7), MOCK_CPUS (8-11), LOAD_CPUS (12-15), CPA_GO_BIN (shared Go build).
#
# Run it under tools/shared.sh yourself; this script never takes the machine lock (a nested
# shared.sh would deadlock against a pending tools/exclusive.sh).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
TMP="${CPA_TMP:-/home/ishaan/box/cliproxyapirust/tmp}"
BIN="$ROOT/target/bench-bin"
GO_BIN="${CPA_GO_BIN:-$TMP/bench-go/cli-proxy-api-go}"

build=1 rust_only=0 alloc=0
args=()
for a in "$@"; do
  case "$a" in
    --rust-only) rust_only=1 ;;
    --no-build) build=0 ;;
    --alloc) alloc=1 ;;
    *) args+=("$a") ;;
  esac
done

mkdir -p "$BIN"
if [[ $build == 1 ]]; then
  # Separate invocations so feature unification cannot change the server binary.
  cargo build --release -q -p cpa-server
  cargo build --release -q -p cpa-bench
  cp target/release/cliproxy "$BIN/cliproxy"
  cp target/release/cpa-bench "$BIN/cpa-bench"
fi
if [[ $rust_only == 0 && ! -x "$GO_BIN" ]]; then
  echo "building the Go reference once into $(dirname "$GO_BIN")" >&2
  OUT="$(dirname "$GO_BIN")" bench/scripts/build-go.sh >&2
fi
if [[ $alloc == 1 ]]; then
  bench/scripts/build-alloc.sh >&2
fi

servers=go,rust
[[ $rust_only == 1 ]] && servers=rust
LOAD_CPUS="${LOAD_CPUS-12-15}"
pin=()
[[ -n "$LOAD_CPUS" ]] && pin=(taskset -c "$LOAD_CPUS")
exec "${pin[@]}" "$BIN/cpa-bench" quick "${args[@]}" \
  --servers "$servers" --go-bin "$GO_BIN" --rust-bin "$BIN/cliproxy" \
  --rust-alloc-bin "$BIN/cliproxy-alloc" \
  --server-cpus "${SERVER_CPUS-0-7}" --mock-cpus "${MOCK_CPUS-8-11}"
