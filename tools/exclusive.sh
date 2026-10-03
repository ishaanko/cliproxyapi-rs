#!/usr/bin/env bash
# Run a command (benchmarks, timing measurements) as the ONLY workload on the machine: closes the
# gate so no new tools/shared.sh command starts, waits for running ones to finish, then runs.
# usage: tools/exclusive.sh bench/scripts/run.sh --runs 7
set -euo pipefail
L=/home/ishaan/box/cliproxyapirust/tmp
mkdir -p "$L"
exec flock "$L/bench-gate.lock" flock "$L/machine.lock" "$@"
