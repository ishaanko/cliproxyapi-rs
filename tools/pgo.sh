#!/usr/bin/env bash
# Profile-guided release build of the server: builds an instrumented binary, trains it on the
# bench workload (cpa-bench quick, mock upstream), then rebuilds with the merged profile.
# Result: target/pgo/cliproxy (about 10-15% less CPU per request than the plain release build).
#
#   tools/pgo.sh                 # about 30 minutes; takes the machine locks itself
#
# Needs `rustup component add llvm-tools-preview`. Run it on a quiet machine: the training step
# takes tools/exclusive.sh. The profile is only valid for the commit it was recorded on; functions
# changed since are simply compiled without profile data.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/target/pgo"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
PROFDATA="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/llvm-profdata"
[[ -x "$PROFDATA" ]] || { echo "missing llvm-profdata: rustup component add llvm-tools-preview" >&2; exit 1; }

# Scenarios the profile is recorded on (bench/README.md); the 2 MB ones are left out because the
# instrumented binary is too slow for them.
SCENARIOS=(chat-compat-json chat-claude-stream responses-codex-stream claude-gemini-json models
  claude-native-stream gemini-native-stream chat-gemini-stream chat-claude-stream-long chat-compat-stream-long)

# Same profile as the workspace release profile, spelled out so the script also works when the
# release profile is changed.
export CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

rm -rf "$OUT/raw"
mkdir -p "$OUT/raw"

echo "== instrumented build" >&2
RUSTFLAGS="-Cprofile-generate=$OUT/raw" CARGO_TARGET_DIR="$OUT/gen-target" \
  tools/shared.sh cargo build --release -p cpa-server --features pgo-dump
echo "== bench driver" >&2
tools/shared.sh cargo build --release -p cpa-bench

echo "== training" >&2
# One fresh server per scenario; the server flushes its counters every few seconds
# (feature `pgo-dump`) because the harness ends it with SIGKILL.
for s in "${SCENARIOS[@]}"; do
  LLVM_PROFILE_FILE="$OUT/raw/p-%p.profraw" tools/exclusive.sh taskset -c "${LOAD_CPUS:-12-15}" \
    target/release/cpa-bench quick "$s" --servers rust --rust-bin "$OUT/gen-target/release/cliproxy" \
    --rust-alloc-bin /nonexistent --reps 1 --server-cpus "${SERVER_CPUS:-0-7}" --mock-cpus "${MOCK_CPUS:-8-11}" >/dev/null
done

echo "== merge" >&2
# A server killed mid-write leaves an empty file; skip those.
mapfile -t RAW < <(find "$OUT/raw" -name '*.profraw' -size +0)
"$PROFDATA" merge -o "$OUT/merged.profdata" "${RAW[@]}"

echo "== optimized build" >&2
RUSTFLAGS="-Cprofile-use=$OUT/merged.profdata" CARGO_TARGET_DIR="$OUT/use-target" \
  tools/shared.sh cargo build --release -p cpa-server
cp "$OUT/use-target/release/cliproxy" "$OUT/cliproxy"
ls -l "$OUT/cliproxy"
