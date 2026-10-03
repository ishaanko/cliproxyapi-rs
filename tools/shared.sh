#!/usr/bin/env bash
# Run a build/test command under the machine-wide shared lock. Many shared holders can run at
# once; tools/exclusive.sh (benchmarks) waits for all of them and blocks new ones while pending.
# Re-entrant: nested calls (e.g. tools/e2e.sh run under shared.sh) skip the lock, because waiting
# on the gate while holding the shared lock would deadlock against a pending exclusive run.
# usage: tools/shared.sh cargo test -q --workspace --all-targets
set -euo pipefail
if [ -n "${CPA_SHARED_LOCK_HELD:-}" ]; then exec "$@"; fi
L=/home/ishaan/box/cliproxyapirust/tmp
mkdir -p "$L"
export CPA_SHARED_LOCK_HELD=1
# Wait while a benchmark is pending or running (the gate is held exclusively for its duration).
flock "$L/bench-gate.lock" true
exec flock -s "$L/machine.lock" "$@"
