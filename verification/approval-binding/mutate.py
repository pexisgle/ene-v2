#!/usr/bin/env python3
"""Generate broken variants of ApprovalBinding.tla, one per guarded property.

Each mutant removes or weakens exactly the guard that mirrors a real check in
apps/ene-host. The runner in run.sh expects the named invariant to fail on
each mutant; a property that survives its own mutant is not being checked.
"""
import pathlib
import sys

BASE = pathlib.Path(__file__).resolve().parent
SPEC = BASE / "ApprovalBinding.tla"
CFG = BASE / "ApprovalBinding.cfg"

MUTANTS = [
    (
        "move-no-expiry",
        "Move no longer expires the pending approval of the moved generation.\n"
        "Mirrors removing db::expire_approvals_for from client::activate.",
        "NoStalePendingApproval",
        (
            '    /\\ IF approvalState = "pending" THEN approvalState\' = "expired" ELSE approvalState\' = approvalState',
            "    /\\ approvalState' = approvalState",
        ),
    ),
    (
        "dispatch-no-revalidation",
        "Dispatch keeps the call's generation instead of revalidating the active one.\n"
        "Mirrors removing the target comparison from client::device::accept.",
        "DispatchedToCurrentGeneration",
        (
            '    /\\ callState = "ready"\n'
            '    /\\ active = callClient\n'
            '    /\\ rev = callRev\n'
            '    /\\ opClient\' = callClient',
            '    /\\ callState = "ready"\n'
            "    /\\ opClient' = callClient",
        ),
    ),
    (
        "dispatch-without-approval",
        "Dispatch is allowed while the approval is still pending.\n"
        "Mirrors losing the Stage::Gate / Approved ordering.",
        "NoUnapprovedDispatch",
        (
            'DispatchChecked ==\n    /\\ callState = "ready"',
            "DispatchChecked ==",
        ),
    ),
    (
        "result-any-generation",
        "A result of any client generation completes the operation.\n"
        "Mirrors removing the client/revision comparison from client::device::complete.",
        "AppliedBySameGeneration",
        (
            "    /\\ appliedAt' = <<opClient, opRev>>",
            "    /\\ appliedAt' \\in (Clients \\X Revs) \\ {<<opClient, opRev>>}",
        ),
    ),
]


def main():
    out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else BASE / ".out" / "mutants")
    spec = SPEC.read_text()
    cfg = CFG.read_text()
    out.mkdir(parents=True, exist_ok=True)
    manifest = []
    for name, note, invariant, (old, new) in MUTANTS:
        if old not in spec:
            sys.exit(f"mutant {name}: anchor not found in {SPEC}; update mutate.py")
        directory = out / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "ApprovalBinding.tla").write_text(spec.replace(old, new, 1))
        (directory / "ApprovalBinding.cfg").write_text(cfg)
        manifest.append(f"{name}\t{invariant}\t{note.replace(chr(10), ' ')}")
    (out / "manifest.tsv").write_text("\n".join(manifest) + "\n")
    print(f"wrote {len(MUTANTS)} mutants to {out}")


if __name__ == "__main__":
    main()
