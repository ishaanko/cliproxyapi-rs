#!/usr/bin/env bash
# Build the server and the e2e harness in separate cargo invocations, then run the E2E check
# under the shared lock. Separate builds keep cargo feature unification from leaking between the
# two (the shipped `cargo build --release -p cpa-server` binary is what gets checked).
# usage: tools/e2e.sh [cpa-e2e check args, e.g. --filter chat.claude]
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
T=$(realpath "${CARGO_TARGET_DIR:-target}")
cargo build --release -p cpa-server
cargo build --release -p cpa-e2e
exec flock /home/ishaan/box/cliproxyapirust/tmp/e2e.lock \
  "$T/release/cpa-e2e" check --server "$T/release/cliproxy" "$@"
