#!/usr/bin/env bash
# Build the server and the e2e harness in separate cargo invocations, then run the E2E check
# under the shared lock. Separate builds keep cargo feature unification from leaking between the
# two (the shipped `cargo build --release -p cpa-server` binary is what gets checked).
# usage: tools/e2e.sh [cpa-e2e check args, e.g. --filter chat.claude]
#
#   CPA_PROFILE=release-fast tools/e2e.sh   # thin LTO, 16 codegen units: much faster rebuilds
#                                           # while iterating; final gating uses the default
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
T=$(realpath "${CARGO_TARGET_DIR:-target}")
PROFILE="${CPA_PROFILE:-release}"
case "$PROFILE" in
  release) PROFILE_ARGS=(--release) ;;
  release-fast) PROFILE_ARGS=(--profile release-fast) ;;
  *) echo "CPA_PROFILE must be release or release-fast (got $PROFILE)" >&2; exit 1 ;;
esac
tools/shared.sh cargo build "${PROFILE_ARGS[@]}" -p cpa-server
tools/shared.sh cargo build "${PROFILE_ARGS[@]}" -p cpa-e2e
exec tools/shared.sh flock /home/ishaan/box/cliproxyapirust/tmp/e2e.lock \
  "$T/$PROFILE/cpa-e2e" check --server "$T/$PROFILE/cliproxy" "$@"
