#!/usr/bin/env bash
# One-command check of the approval → dispatch → result generation property.
#
#   ./run.sh              sync check + TLC on the real specification
#   ./run.sh --mutants    prove the specification can fail: run every mutant and
#                         require the named invariant to be violated
#
# Requires nix (tlaplus and a JRE come from the nix shell, cached after the
# first run). TLC output for the default run lands in .out/check/tlc.log.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=$here/.out
mkdir -p "$out/check"

tlc() { # tlc <dir> <spec-basename> <cfg-basename> <log>
  local dir=$1 spec=$2 cfg=$3 log=$4
  mkdir -p "$dir/metadir"
  ( cd "$dir" && nix shell nixpkgs#tlaplus nixpkgs#jre -c \
      tlc -config "$cfg" -metadir "$dir/metadir" -workers auto "$spec" ) >"$log" 2>&1
}

check_spec() {
  local dir=$out/check log=$out/check/tlc.log
  mkdir -p "$dir"
  cp "$here/ApprovalBinding.tla" "$here/ApprovalBinding.cfg" "$dir/"
  tlc "$dir" ApprovalBinding.tla ApprovalBinding.cfg "$log"
  if grep -q "No error has been found" "$log"; then
    echo "PASS  model check: every reachable state satisfies the four invariants"
    grep -E "distinct states found|depth of the complete state graph" "$log" | sed 's/^/      /'
    return 0
  fi
  echo "FAIL  model check: an invariant is violated"
  grep -E "Invariant .* is violated|State [0-9]+:|Error" "$log" | head -20 >&2
  return 1
}

check_mutants() {
  local manifest=$out/mutants/manifest.tsv
  python3 "$here/mutate.py" "$out/mutants" || return 1
  local failed=0
  while IFS=$'\t' read -r name invariant note; do
    local log=$out/mutants/$name/tlc.log
    tlc "$out/mutants/$name" ApprovalBinding.tla ApprovalBinding.cfg "$log"
    if grep -q "Invariant $invariant is violated" "$log"; then
      echo "PASS  mutant $name: $invariant fails as expected"
    else
      echo "FAIL  mutant $name: $invariant did not fail"
      failed=1
    fi
  done <"$manifest"
  return $failed
}

mode=${1:-}
case "$mode" in
  --mutants)
    check_mutants
    ;;
  "")
    python3 "$here/check_sync.py" || exit 1
    check_spec
    ;;
  *)
    echo "usage: $0 [--mutants]" >&2
    exit 2
    ;;
esac
