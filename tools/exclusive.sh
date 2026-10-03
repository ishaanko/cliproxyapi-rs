#!/usr/bin/env bash
# Run a command (benchmarks, timing measurements, any load generation) as the ONLY workload on the
# machine: closes the gate so no new tools/shared.sh command starts, waits for running ones to
# finish, runs, then keeps the lock until TIME-WAIT sockets drain so the next build/test does not
# hit ephemeral port exhaustion (this host only has ~4000 ephemeral ports).
# usage: tools/exclusive.sh bench/scripts/run.sh --runs 7
set -euo pipefail
L=/home/ishaan/box/cliproxyapirust/tmp
mkdir -p "$L"
exec flock "$L/bench-gate.lock" flock "$L/machine.lock" bash -c '
  rc=0; "$@" || rc=$?
  until [ "$(ss -tan state time-wait | wc -l)" -lt 1000 ]; do sleep 5; done
  exit $rc' exclusive "$@"
