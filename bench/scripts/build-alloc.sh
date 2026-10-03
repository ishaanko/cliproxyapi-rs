#!/usr/bin/env bash
# Builds the Rust server with the counting allocator (`alloc-stats` feature) into
# target/bench-bin/cliproxy-alloc, for `cpa-bench quick` allocations/request.
#
# crates/server must contain the hook (feature `alloc-stats`, see bench/README.md). While it does
# not, bench/alloc-stats.patch is applied to a synced copy of the tree (target/alloc-src), so the
# working tree and its main release build are never touched. Both variants build into
# target/alloc. Run under tools/shared.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
OUT="$ROOT/target/bench-bin"
mkdir -p "$OUT"

SRC="$ROOT"
if ! grep -q "alloc-stats" crates/server/Cargo.toml; then
  SRC="$ROOT/target/alloc-src"
  mkdir -p "$SRC"
  # rsync keeps mtimes of unchanged files, so cargo's incremental state in target/alloc survives.
  rsync -a --delete --exclude /target --exclude /.git --exclude /tmp --exclude /.claude "$ROOT/" "$SRC/"
  patch -s -p1 -d "$SRC" < "$ROOT/bench/alloc-stats.patch"
fi

(cd "$SRC" && CARGO_TARGET_DIR="$ROOT/target/alloc" cargo build --release -q -p cpa-server --features alloc-stats)
cp "$ROOT/target/alloc/release/cliproxy" "$OUT/cliproxy-alloc"
ls -l "$OUT/cliproxy-alloc"
