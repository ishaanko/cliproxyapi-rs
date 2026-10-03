#!/usr/bin/env bash
# Run a build/test command under the machine-wide shared lock. Many shared holders can run at
# once; tools/exclusive.sh (benchmarks) waits for all of them and blocks new ones while pending.
# usage: tools/shared.sh cargo test -q --workspace --all-targets
set -euo pipefail
L=/home/ishaan/box/cliproxyapirust/tmp
mkdir -p "$L"
# Wait while a benchmark is pending or running (the gate is held exclusively for its duration).
flock "$L/bench-gate.lock" true
exec flock -s "$L/machine.lock" "$@"
