#!/bin/bash
# Builds the Go example plugins (c-shared) the plugin E2E scenarios load into target/e2e-plugins.
#
#   GO_REPO     CLIProxyAPI checkout (default: <repo>/tmp/CLIProxyAPI)
#   GO_BIN_DIR  directory holding the `go` binary (default: <repo>/tmp/go-toolchain/bin)
#   OUT         output directory (default: <repo>/target/e2e-plugins)
#
# The examples' go.mod files name module v7 while the checkout is v8, so the sources are copied
# and patched first. Each library is installed as <example>.so (the file stem is the plugin id).
set -eu
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
GO_REPO=${GO_REPO:-/home/ishaan/box/cliproxyapirust/tmp/CLIProxyAPI}
GO_BIN_DIR=${GO_BIN_DIR:-/home/ishaan/box/cliproxyapirust/tmp/go-toolchain/bin}
OUT=${OUT:-$REPO/target/e2e-plugins}
WORK=$REPO/target/e2e-plugins-src
export GOPATH=${GOPATH:-/home/ishaan/box/cliproxyapirust/tmp/gopath}
export GOFLAGS=-mod=mod
export PATH=$GO_BIN_DIR:$PATH

rm -rf "$WORK"
mkdir -p "$WORK" "$OUT"
cp -r "$GO_REPO/examples/plugin/." "$WORK/"
for d in "$WORK"/*/go; do
  name=$(basename "$(dirname "$d")")
  sed -i "s#CLIProxyAPI/v7#CLIProxyAPI/v8#g; s#v7.0.0#v8.0.0#g; s#=> ../../../..#=> $GO_REPO#" "$d/go.mod"
  sed -i "s#CLIProxyAPI/v7#CLIProxyAPI/v8#g" "$d"/*.go
  if (cd "$d" && go build -buildmode=c-shared -o "$OUT/$name.so" . 2>"$OUT/$name.err"); then
    rm -f "$OUT/$name.err" "$OUT/$name.h"
    echo "ok   $name"
  else
    echo "FAIL $name (see $OUT/$name.err)"
  fi
done
