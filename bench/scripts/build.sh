#!/usr/bin/env bash
# Builds both release binaries into target/bench-bin/ (Go reference and Rust port).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/target/bench-bin"
mkdir -p "$OUT"

"$ROOT/bench/scripts/build-go.sh"

cd "$ROOT"
cargo build --release -p cpa-server -p cpa-bench
cp target/release/cliproxy "$OUT/cliproxy"
cp target/release/cpa-bench "$OUT/cpa-bench"
# Go is built with -s -w, so also compare against a stripped Rust binary.
strip -o "$OUT/cliproxy-stripped" "$OUT/cliproxy"
ls -l "$OUT"
