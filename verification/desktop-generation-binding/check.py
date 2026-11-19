"""Sync check for the desktop generation-binding verification.

The exhaustive Rust test proves that `admits` in apps/ene-desktop/src/session.rs
lets an operation run only under the generation the client holds. That proof
describes the code at one moment; this script notices when the code moves on.

It checks three things and prints one line each:

- the guarded functions still exist (admits, refusal, and the exhaustive test);
- each of the three guards is still written the way the property assumes;
- the guard function's body still hashes to the value recorded when the proof
  was last confirmed.

Exit 0 means the proof still describes this source. Any other exit means read
`coverage.md`, then re-read the functions before touching anything.
"""

import hashlib
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "apps/ene-desktop/src/session.rs"
COVERAGE = Path(__file__).with_name("coverage.md")
TEST_NAME = "only_the_claim_naming_the_current_binding_executes"

# What the property assumes about each guard. Both are ownership checks: the
# client that executes is the one the operation was addressed to, on the device
# stream it currently holds, while it is the active device for a companion.
GUARDS = (
    (
        "client identity",
        r"if claim\.client_id != our_id",
    ),
    (
        "device stream",
        r"if !our_subscription\.is_empty\(\)\s*&&\s*claim\.subscription_id != our_subscription",
    ),
    (
        "active device",
        r"None => Err\(\"this client is not the active device for any companion\"\.into\(\)\)",
    ),
)


def guard_body(source: str) -> str | None:
    """The text of `fn admits` from its signature to its closing brace."""
    start = source.find("fn admits(")
    if start < 0:
        return None
    depth = 0
    for offset in range(start, len(source)):
        char = source[offset]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[start : offset + 1]
    return None


def recorded_hash() -> str:
    """The hash the coverage file says was last confirmed."""
    text = COVERAGE.read_text(encoding="utf-8")
    match = re.search(r"guard-sha256:\s*([0-9a-f]{64})", text)
    return match.group(1) if match else ""


def git(*args: str) -> str:
    try:
        return subprocess.run(
            ["git", *args],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()
    except OSError:
        return ""


def main() -> int:
    failures = 0
    if not SOURCE.exists():
        print(f"FAIL source missing: {SOURCE}")
        return 1
    source = SOURCE.read_text(encoding="utf-8")

    body = guard_body(source)
    present = {
        "admits": body is not None,
        "refusal": "fn refusal(" in source,
        "test": f"fn {TEST_NAME}(" in source,
    }
    for name, found in present.items():
        print(f"{'PASS' if found else 'FAIL'} covered function: {name}")
        failures += 0 if found else 1

    for name, pattern in GUARDS:
        found = re.search(pattern, source) is not None
        print(f"{'PASS' if found else 'FAIL'} guard: {name}")
        failures += 0 if found else 1

    if body is None:
        return 1
    digest = hashlib.sha256(re.sub(r"\s+", " ", body).strip().encode()).hexdigest()
    recorded = recorded_hash()
    if not recorded:
        print("FAIL coverage.md has no guard-sha256")
        failures += 1
    elif digest == recorded:
        print(f"PASS guard body unchanged since verification ({digest[:12]})")
    else:
        print(
            "STALE guard body changed since verification: "
            f"recorded {recorded[:12]}, now {digest[:12]}. "
            "Re-read admits and refusal, rerun the test and the break check, "
            "then record the new hash."
        )
        failures += 1

    head = git("rev-parse", "--short", "HEAD")
    dirty = git("status", "--porcelain", "--", str(SOURCE.relative_to(ROOT)))
    state = "uncommitted changes" if dirty else "clean"
    print(f"INFO source {SOURCE.relative_to(ROOT)} at HEAD {head or '?'}, {state}")
    print("PASS" if failures == 0 else f"FAIL {failures} check(s)")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
