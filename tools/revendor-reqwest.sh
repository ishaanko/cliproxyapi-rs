#!/usr/bin/env bash
# Maintains crates/executors/vendor/reqwest: upstream reqwest plus crates/executors/vendor/reqwest.patch
# (a `ClientBuilder::http1_max_buf_size` option, see PATCH.md in the vendored tree).
#
#   tools/revendor-reqwest.sh            rebuild the vendored tree from upstream + patch
#   tools/revendor-reqwest.sh --check    fail unless the tree equals upstream + patch
#   tools/revendor-reqwest.sh --diff     regenerate the patch from the (edited) vendored tree
#
# To upgrade reqwest: set VERSION, run the default mode, fix rejects by hand if the patch no longer
# applies, run --diff, refresh PATCH.md. Upstream comes from the local cargo registry cache or
# static.crates.io. The tree is the pristine crate (sources of every feature stay, so enabling a
# reqwest feature in any workspace crate keeps working); only tests/CI files are left out.
set -euo pipefail

VERSION=0.12.28
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/crates/executors/vendor/reqwest"
PATCH="$ROOT/crates/executors/vendor/reqwest.patch"
KEEP=(Cargo.toml LICENSE-APACHE LICENSE-MIT README.md src)

work="$(mktemp -d "${TMPDIR:-/tmp}/revendor.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Pristine upstream, reduced to $KEEP, in $work/a.
fetch() {
  local src
  src="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/reqwest-"$VERSION" 2>/dev/null | head -n 1 || true)"
  if [[ -z "$src" ]]; then
    curl -sSfL "https://static.crates.io/crates/reqwest/reqwest-$VERSION.crate" | tar xz -C "$work"
    src="$work/reqwest-$VERSION"
  fi
  mkdir -p "$work/a"
  for f in "${KEEP[@]}"; do cp -r "$src/$f" "$work/a/$f"; done
}

fetch
case "${1:-}" in
  --diff)
    mkdir -p "$work/cmp" && cp -r "$work/a" "$work/cmp/a" && mkdir "$work/cmp/b"
    for f in "${KEEP[@]}"; do cp -r "$DEST/$f" "$work/cmp/b/$f"; done
    (cd "$work/cmp" && { diff -ruN a b || true; } | sed -E 's/^((---|\+\+\+) [^\t]+)\t.*/\1/' > "$PATCH")
    echo "wrote $PATCH ($(wc -l < "$PATCH") lines)"
    ;;
  --check)
    cp -r "$work/a" "$work/b"
    patch -s -p1 -d "$work/b" < "$PATCH"
    for f in "${KEEP[@]}"; do diff -ru "$work/b/$f" "$DEST/$f"; done && echo "vendored reqwest matches upstream $VERSION + patch"
    ;;
  "")
    cp -r "$work/a" "$work/b"
    patch -s -p1 -d "$work/b" < "$PATCH"
    cp "$DEST/PATCH.md" "$work/b/PATCH.md" 2>/dev/null || true
    rm -rf "$DEST"
    mv "$work/b" "$DEST"
    echo "re-vendored reqwest $VERSION into $DEST"
    ;;
  *)
    echo "usage: $0 [--check|--diff]" >&2
    exit 2
    ;;
esac
