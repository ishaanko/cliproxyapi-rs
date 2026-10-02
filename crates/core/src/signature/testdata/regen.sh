#!/bin/bash
# Regenerates golden.jsonl.gz from the Go reference implementation.
# The generator is a Go test file injected into internal/signature with `go test -overlay`
# (the reference checkout is never modified). Usage: regen.sh [reference-repo] [go-toolchain-bin]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
ref="${1:-/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI}"
gobin="${2:-/home/ishaan/box/cliproxyapirust/tmp/go-toolchain/bin}"
work="$(mktemp -d)"
cp "$here/zz_golden_test.go.txt" "$work/zz_golden_test.go"
echo "{\"Replace\": {\"$ref/internal/signature/zz_golden_test.go\": \"$work/zz_golden_test.go\"}}" > "$work/overlay.json"
export PATH="$gobin:$PATH" GOPATH="${GOPATH:-$ref/../gopath}" GOFLAGS=-mod=mod GOPROXY=off
(cd "$ref" && GOLDEN_OUT="$work/golden.jsonl" go test -overlay "$work/overlay.json" -run TestZZGolden ./internal/signature/)
gzip -9 -c "$work/golden.jsonl" > "$here/golden.jsonl.gz"
rm -rf "$work"
