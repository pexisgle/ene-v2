#!/usr/bin/env bash
# Prove the avatar renderer terminates cleanly from the protocol's own shutdown
# paths. The renderer must exit 0, never panic, and never stall on teardown.
#
# Both cases below are shutdowns the client really sends: a `quit` command, and
# stdin closing when the desktop client goes away. Either may arrive before the
# Wayland compositor handshake has finished, which is what used to panic inside
# exwlshellev (`Not provide server here`) and turn every such exit into 101.
#
# Exit 0 means every check passed. 1 means a check failed. It needs a display,
# because the renderer opens a window.
#
# Usage:
#   verification/avatar-shutdown/run.sh [path/to/ene-avatar]
#
# See coverage.md for what this covers and what it does not.

set -u

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="${1:-$root/target/debug/ene-avatar}"
failures=0

check() {
	local name="$1" ok="$2" detail="${3:-}"
	if [ "$ok" = "1" ]; then
		printf 'PASS %s\n' "$name"
	else
		printf 'FAIL %s (%s)\n' "$name" "$detail"
		failures=$((failures + 1))
	fi
}

if [ ! -x "$bin" ]; then
	printf 'FAIL the renderer binary is missing or not executable: %s\n' "$bin"
	exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# One case: feed a shutdown at `delay` seconds and require a clean, panic-free,
# prompt exit.
probe() {
	local name="$1" delay="$2" input="$3"
	local start end elapsed_ms rc
	start=$(date +%s%N)
	if [ -n "$input" ]; then
		( sleep "$delay"; printf '%s\n' "$input" ) | timeout 30 "$bin" \
			> "$work/$name.out" 2> "$work/$name.err"
	else
		( sleep "$delay" ) | timeout 30 "$bin" > "$work/$name.out" 2> "$work/$name.err"
	fi
	rc=$?
	end=$(date +%s%N)
	elapsed_ms=$(( (end - start) / 1000000 ))

	check "$name exits 0" "$([ "$rc" -eq 0 ] && echo 1 || echo 0)" "rc=$rc"
	check "$name does not panic" \
		"$(grep -q 'panicked at' "$work/$name.err" && echo 0 || echo 1)" \
		"$(grep -m1 'panicked at' "$work/$name.err" || true)"
	# A clean exit is immediate; the two-second teardown budget was the symptom
	# of a channel that never closed.
	check "$name exits promptly" "$([ "$elapsed_ms" -lt 2000 ] && echo 1 || echo 0)" \
		"${elapsed_ms}ms"
}

# Shutdown during the compositor handshake: the case that used to panic.
probe quit-before-first-frame 0.0 '{"id":1,"op":"quit"}'
probe eof-before-first-frame 0.0 ''

# Shutdown once the render loop is running, so both sides of init are covered.
probe quit-after-first-frame 1.2 '{"id":1,"op":"quit"}'
probe eof-after-first-frame 1.2 ''

if [ "$failures" -ne 0 ]; then
	printf '\n%d check(s) failed\n' "$failures"
	exit 1
fi
printf '\nall checks passed\n'
