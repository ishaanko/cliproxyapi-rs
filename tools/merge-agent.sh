#!/usr/bin/env bash
# Merge an agent branch into main only if the workspace builds and tests pass.
# usage: tools/merge-agent.sh <branch> "<commit message>"
set -euo pipefail
branch=$1 msg=$2
cd "$(git rev-parse --show-toplevel)"
git checkout -q Cargo.lock 2>/dev/null || true
if [ -n "$(git status --porcelain)" ]; then echo "working tree dirty"; git status --short; exit 1; fi
git merge --no-ff --no-commit "$branch" || true
python3 tools/dedupe-toml.py
git add crates/*/Cargo.toml 2>/dev/null || true
if git diff --name-only --diff-filter=U | grep -qvE '^(Cargo.lock|crates/[^/]+/Cargo.toml)$'; then
  echo "non-lockfile conflicts:"; git diff --name-only --diff-filter=U; exit 1
fi
git checkout --theirs Cargo.lock 2>/dev/null || true
python3 tools/dedupe-toml.py
git add -A
cargo build -q --workspace
if ! cargo test -q --workspace --all-targets >/tmp/merge-agent-test.log 2>&1; then
  grep -E '^error|FAILED|panicked' /tmp/merge-agent-test.log | head -20; echo "tests failed"; exit 1
fi
git commit -qm "$msg"
git push -q
echo "merged $branch"
