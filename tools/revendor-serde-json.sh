#!/usr/bin/env bash
# Maintains crates/json/vendor/serde_json: upstream serde_json plus crates/json/vendor/serde_json.patch.
#
#   tools/revendor-serde-json.sh            rebuild the vendored tree from upstream + patch
#   tools/revendor-serde-json.sh --check    fail unless the tree equals upstream + patch
#   tools/revendor-serde-json.sh --diff     regenerate the patch from the (edited) vendored tree
#
# To change the fork: edit the vendored tree, run --diff, commit both. To upgrade serde_json:
# set VERSION, run the default mode, fix rejects by hand if the patch no longer applies, run --diff.
# Upstream is fetched from static.crates.io (or the local cargo registry cache when offline).
# Only the files cargo needs are kept (no tests, CI files or lockfile); the patch is applied with
# -p1 from the crate root.
set -euo pipefail

VERSION=1.0.151
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/crates/json/vendor/serde_json"
PATCH="$ROOT/crates/json/vendor/serde_json.patch"
KEEP=(Cargo.toml LICENSE-APACHE LICENSE-MIT README.md build.rs src)

work="$(mktemp -d "${TMPDIR:-/tmp}/revendor.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Pristine upstream, reduced to $KEEP, in $work/a.
fetch() {
  local src
  src="$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/serde_json-"$VERSION" 2>/dev/null | head -n 1 || true)"
  if [[ -z "$src" ]]; then
    curl -sSfL "https://static.crates.io/crates/serde_json/serde_json-$VERSION.crate" | tar xz -C "$work"
    src="$work/serde_json-$VERSION"
  fi
  mkdir -p "$work/a"
  for f in "${KEEP[@]}"; do cp -r "$src/$f" "$work/a/$f"; done
}

fetch
case "${1:-}" in
  --diff)
    # Paths are a/... and b/... so the patch applies with -p1.
    mkdir -p "$work/cmp" && cp -r "$work/a" "$work/cmp/a" && mkdir "$work/cmp/b"
    for f in "${KEEP[@]}"; do cp -r "$DEST/$f" "$work/cmp/b/$f"; done
    # Timestamps are stripped from the file headers so the patch is reproducible.
    (cd "$work/cmp" && { diff -ruN a b || true; } | sed -E 's/^((---|\+\+\+) [^\t]+)\t.*/\1/' > "$PATCH")
    echo "wrote $PATCH ($(wc -l < "$PATCH") lines)"
    ;;
  --check)
    cp -r "$work/a" "$work/b"
    patch -s -p1 -d "$work/b" < "$PATCH"
    diff -ru "$work/b" "$DEST" && echo "vendored serde_json matches upstream $VERSION + patch"
    ;;
  "")
    cp -r "$work/a" "$work/b"
    patch -s -p1 -d "$work/b" < "$PATCH"
    rm -rf "$DEST"
    mv "$work/b" "$DEST"
    echo "re-vendored serde_json $VERSION into $DEST"
    ;;
  *)
    echo "usage: $0 [--check|--diff]" >&2
    exit 2
    ;;
esac
