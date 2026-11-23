#!/usr/bin/env bash
# Build the COMMITTED tree, not the working tree.
#
# The working tree can pass while a commit does not: a change committed without
# the files it depends on still compiles locally, because the missing pieces are
# sitting unstaged. That is exactly how a re-export was left out of a commit and
# HEAD stopped building while every local gate stayed green.
#
# Exit 0 means the commit builds on its own.
#
# Usage:
#   verification/committed-tree-builds/run.sh [rev]
#
# See coverage.md.

set -u

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
rev="${1:-HEAD}"
checkout="$(mktemp -d)/tree"

git -C "$root" worktree add -q "$checkout" "$rev" || {
	printf 'FAIL could not check out %s\n' "$rev"
	exit 1
}
cleanup() { git -C "$root" worktree remove --force "$checkout" >/dev/null 2>&1; }
trap cleanup EXIT

printf 'building %s (%s) in a clean checkout\n' "$rev" "$(git -C "$root" rev-parse --short "$rev")"
if ! ( cd "$checkout" && direnv exec "$root" cargo clippy --locked --workspace --all-targets --all-features -- -D warnings ); then
	printf 'FAIL %s does not build from a clean checkout\n' "$rev"
	exit 1
fi

printf 'PASS %s builds from a clean checkout\n' "$rev"
