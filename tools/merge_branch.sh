#!/usr/bin/env bash
# Merge a finished port branch into main, verify build + tests, push.
# usage: tools/merge_branch.sh <branch> [message]
set -euo pipefail
cd "$(dirname "$0")/.."
br="$1"; msg="${2:-merge $1}"
git checkout -q main
git merge --no-ff --no-edit -m "$msg" "$br" || { echo "MERGE CONFLICT"; git status --short | grep -E '^(UU|AA|DU|UD)' ; exit 2; }
python3 tools/scaffold.py >/dev/null
if ! git diff --quiet; then git commit -qam "scaffold: refresh generated mod blocks"; fi
cargo build --all-targets 2>&1 | grep -E '^(error|warning: unused)' | head -20 || true
cargo build --all-targets -q 2>&1 | tail -3
cargo test -q --lib 2>&1 | grep -E '^test result|FAILED|panicked' | tail -15
