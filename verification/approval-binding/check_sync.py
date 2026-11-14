#!/usr/bin/env python3
"""Check that ApprovalBinding.tla still describes the guards in the Rust code.

The model checker checks the design, not the Rust source. This script closes the
gap in the other direction: every guard the model relies on must still exist in
the covered file, and where ordering matters it must still come before the
write it protects. A missing guard or a moved guard fails; a commit newer than
the recorded sync commit only warns, because the next agent must re-read the
covered functions and move the sync commit forward deliberately.
"""
import pathlib
import re
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parents[2]
COVERAGE = pathlib.Path(__file__).resolve().parent / "coverage.md"

# (file, [required snippets], [(earlier snippet, later snippet)])
RULES = [
    (
        "apps/ene-host/src/host/client/device.rs",
        [
            "if current.as_ref() != Some(&(binding.client_id.clone(), binding.activation_revision)) {",
            '"client_moved: the active client changed before the operation was dispatched"',
            "if result.activation_revision != operation.activation_revision",
            "|| result.client_id != operation.client_id",
        ],
        [
            (
                "if current.as_ref() != Some(&(binding.client_id.clone(), binding.activation_revision)) {",
                "store.insert_operation(&operation)?;",
            ),
            (
                "if result.activation_revision != operation.activation_revision",
                "if operation.status.is_terminal() {",
            ),
        ],
    ),
    (
        "apps/ene-host/src/host/permission/mod.rs",
        [
            "if current.as_ref() != Some(&(client.to_owned(), revision)) {",
            '"client_moved: the approval is bound to an old client generation"',
            "activation_revision: Some(activation_revision),",
            "client_id: Some(client_id.clone()),",
        ],
        [
            (
                "if current.as_ref() != Some(&(client.to_owned(), revision)) {",
                "world.entity_mut(hold.call).insert(Approved);",
            ),
            (
                "let hold = world.spawn(HoldOf(call)).id();",
                "store.insert_approval(&approval)",
            ),
        ],
    ),
    (
        "apps/ene-host/src/host/client/mod.rs",
        [
            "store.activate(&request.companion_id, client, request.expected_revision)?;",
            "store.fail_operations_of(&old_client, previous.revision)?;",
            "store.expire_approvals_for(&request.companion_id)?;",
        ],
        [
            (
                "store.activate(&request.companion_id, client, request.expected_revision)?;",
                "store.fail_operations_of(&old_client, previous.revision)?;",
            ),
            (
                "store.fail_operations_of(&old_client, previous.revision)?;",
                "store.expire_approvals_for(&request.companion_id)?;",
            ),
        ],
    ),
    (
        "apps/ene-host/src/host/db/approvals.rs",
        ["WHERE status = 'pending' AND session_id = ?1"],
        [],
    ),
    (
        "apps/ene-host/src/host/db/clients.rs",
        ["AND status IN ('pending','dispatched')"],
        [],
    ),
]


def main():
    problems = []
    for relative, required, ordered in RULES:
        path = REPO / relative
        if not path.exists():
            problems.append(f"{relative}: missing file")
            continue
        text = path.read_text()
        lines = text.splitlines()
        for snippet in required:
            if snippet not in text:
                problems.append(f"{relative}: guard missing: {snippet[:90]}")
        for first, second in ordered:
            if first in text and second in text:
                a = next((i for i, line in enumerate(lines) if first in line), None)
                b = next((i for i, line in enumerate(lines) if second in line), None)
                if a is None or b is None:
                    continue
                if a >= b:
                    problems.append(
                        f"{relative}: order changed: {first[:60]} now comes after {second[:60]}"
                    )

    sync = re.search(r"sync-commit:\s*`?([0-9a-f]{7,40})`?", COVERAGE.read_text())
    if not sync:
        problems.append("coverage.md: no sync-commit recorded")
    else:
        # Only a change to a covered file can invalidate the model. Commits that
        # touch other files (docs, skills, unrelated handlers) are irrelevant.
        paths = [relative for relative, _, _ in RULES]
        changed = subprocess.run(
            ["git", "-C", str(REPO), "log", "--oneline", f"{sync.group(1)}..HEAD", "--", *paths],
            capture_output=True,
            text=True,
            check=False,
        )
        if changed.returncode != 0:
            print(f"WARN  sync: cannot inspect history from {sync.group(1)[:7]}; "
                  "re-read the covered functions before trusting this run")
        elif changed.stdout.strip():
            commits = " | ".join(changed.stdout.strip().splitlines()[:3])
            print(f"WARN  sync: covered code changed after {sync.group(1)[:7]} ({commits}); "
                  "re-read the functions in coverage.md, then move sync-commit forward")

    if problems:
        for problem in problems:
            print(f"FAIL  sync: {problem}")
        return 1
    print(f"PASS  sync: {len(RULES)} covered files still contain every guard the model relies on")
    return 0


if __name__ == "__main__":
    sys.exit(main())
