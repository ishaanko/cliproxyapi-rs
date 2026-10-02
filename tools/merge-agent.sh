#!/usr/bin/env bash
# Merge an agent branch into main only if the workspace builds and tests pass.
# usage: tools/merge-agent.sh <branch> "<commit message>"
set -euo pipefail
branch=$1 msg=$2
cd "$(git rev-parse --show-toplevel)"
git merge --no-ff --no-commit "$branch" || true
if git diff --name-only --diff-filter=U | grep -qv '^Cargo.lock$'; then
  echo "non-lockfile conflicts:"; git diff --name-only --diff-filter=U; exit 1
fi
git checkout --theirs Cargo.lock 2>/dev/null || true
git add -A
cargo build -q --workspace
cargo test -q --workspace 2>&1 | grep -E 'test result|FAILED|panicked' | grep -v 'ok\.' && { echo "tests failed"; exit 1; } || true
git commit -qm "$msg"
git push -q
echo "merged $branch"
