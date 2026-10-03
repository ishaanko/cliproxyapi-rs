#!/usr/bin/env bash
# Builds the Go reference as a release binary (-trimpath -ldflags "-s -w") into target/bench-bin/.
# Env: OUT (output dir), GO_SRC (CLIProxyAPI checkout), GO_TOOLCHAIN (go install dir), GOPATH, CPA_TMP.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="${CPA_TMP:-/home/ishaan/box/cliproxyapirust/tmp}"
GO_SRC="${GO_SRC:-$TMP/CLIProxyAPI}"
GO_TOOLCHAIN="${GO_TOOLCHAIN:-$TMP/go-toolchain}"
OUT="${OUT:-$ROOT/target/bench-bin}"
mkdir -p "$OUT"

export PATH="$GO_TOOLCHAIN/bin:$PATH"
export GOPATH="${GOPATH:-$TMP/gopath}" GOFLAGS=-mod=mod
cd "$GO_SRC"
go build -trimpath -ldflags "-s -w" -o "$OUT/cli-proxy-api-go" ./cmd/server
ls -l "$OUT/cli-proxy-api-go"
