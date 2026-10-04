#!/usr/bin/env bash
# Live Go-vs-Rust servers for tools/live/diff.py. Usage: ctl.sh start|stop|restart
# Rust listens on 18317, Go on 18318. State lives in tmp/live (git-ignored): each server has
# its own auth dir (tmp/live/{rust,go}/auths, filled by --claude-login etc.) so the two never
# race on refresh-token rotation. An optional Meta API key is read from tmp/live/meta.key.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
STATE="$ROOT/tmp/live"
RUST_BIN="${RUST_BIN:-$ROOT/target/release/cliproxy}"
GO_BIN="${GO_BIN:-$ROOT/tmp/cli-proxy-api-go}"
mkdir -p "$STATE/rust/auths" "$STATE/go/auths"
chmod 700 "$STATE"

write_config() { # $1=name $2=port
  umask 077
  cat > "$STATE/$1/config.yaml" <<EOF
config-version: 8
server:
  host: "127.0.0.1"
  port: $2
management:
  secret-key: "live-mgmt"
access:
  api-keys:
    - "live-test-key"
routing:
  retry:
    request-retry: 0
oauth:
  auth-dir: "$STATE/$1/auths"
observability:
  usage:
    usage-statistics-enabled: true
EOF
  if [ -s "$STATE/meta.key" ]; then
    cat >> "$STATE/$1/config.yaml" <<EOF
api-keys:
  meta:
    - name: meta-1
      keys:
        - api-key: "$(cat "$STATE/meta.key")"
EOF
  fi
}

stop() {
  for s in rust go; do
    if [ -f "$STATE/$s/pid" ]; then kill "$(cat "$STATE/$s/pid")" 2>/dev/null || true; rm -f "$STATE/$s/pid"; fi
  done
  sleep 1
}

start_one() { # $1=name $2=port $3=binary
  write_config "$1" "$2"
  setsid nohup "$3" --config "$STATE/$1/config.yaml" > "$STATE/$1/server.log" 2>&1 &
  echo $! > "$STATE/$1/pid"
  for _ in $(seq 100); do curl -sf -o /dev/null "localhost:$2/" && return; sleep 0.1; done
  echo "$1 did not come up on :$2, see $STATE/$1/server.log" >&2; exit 1
}

case "${1:-}" in
  start) start_one rust 18317 "$RUST_BIN"; start_one go 18318 "$GO_BIN" ;;
  stop) stop ;;
  restart) stop; start_one rust 18317 "$RUST_BIN"; start_one go 18318 "$GO_BIN" ;;
  *) echo "usage: $0 start|stop|restart" >&2; exit 2 ;;
esac
