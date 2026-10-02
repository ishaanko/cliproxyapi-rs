#!/usr/bin/env bash
# Re-runs the Go-vs-Rust differential checks for the cpa-core base layer (util, registry, misc,
# applypatch). See README.md. Scratch output (Go repo copy, binaries) goes to $SCRATCH.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
MAIN_ROOT=$(cd "$(git -C "$REPO" rev-parse --git-common-dir)/.." && pwd)

GO_SRC=${GO_SRC:-$MAIN_ROOT/tmp/CLIProxyAPI}            # reference Go checkout
GO_TOOLCHAIN_BIN=${GO_TOOLCHAIN_BIN:-$MAIN_ROOT/tmp/go-toolchain/bin}
export SCRATCH=${SCRATCH:-$MAIN_ROOT/tmp/scratch/base}
FUZZ_SEEDS=${FUZZ_SEEDS:-"1 2 3"}                       # registry scenario seeds
FUZZ_COUNT=${FUZZ_COUNT:-400}                           # scenarios per seed

mkdir -p "$SCRATCH"
export PATH="$GO_TOOLCHAIN_BIN:$PATH"
export GOPATH=${GOPATH:-$MAIN_ROOT/tmp/gopath}
export GOFLAGS=-mod=mod
export GOCACHE="$SCRATCH/gocache"

echo "== Go oracle"
rm -rf "$SCRATCH/gorepo"
mkdir -p "$SCRATCH/gorepo"
(cd "$GO_SRC" && tar --exclude=.git -cf - .) | tar -xf - -C "$SCRATCH/gorepo"
mkdir -p "$SCRATCH/gorepo/cmd/zz_base_oracle"
cp "$HERE/go/base_oracle.go.txt" "$SCRATCH/gorepo/cmd/zz_base_oracle/main.go"
(cd "$SCRATCH/gorepo" && go build -o "$SCRATCH/base_oracle_go" ./cmd/zz_base_oracle)

echo "== Rust driver"
CARGO_TARGET_DIR="$SCRATCH/target" cargo build --release --manifest-path "$HERE/rust/Cargo.toml"

export GO_ORACLE="$SCRATCH/base_oracle_go"
export RUST_ORACLE="$SCRATCH/target/release/base-oracle"
export GO_REPO="$SCRATCH/gorepo"
export EXTRA_SCHEMAS="$SCRATCH/extra_schemas.json"

cd "$HERE/python"
python3 mk_extra.py
echo "== schemas / name maps / Responses tools (expect only whitespace-in-hint mismatches)"
python3 schemas.py 0
echo "== scalar helpers (expect only 2 OAuth error-text mismatches)"
python3 scalars.py
echo "== registry scenarios (expect 0 mismatches)"
for seed in $FUZZ_SEEDS; do
  python3 registry_fuzz.py "$seed" "$FUZZ_COUNT"
done
