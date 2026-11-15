#!/usr/bin/env python3
"""Check coverage and completion evidence for the library delegation audit."""

import argparse
import copy
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
COUNTS = {"H": 21, "T": 13, "A": 6, "V": 8, "P": 13, "R": 2}
EXPECTED = {f"{prefix}{index:02d}" for prefix, count in COUNTS.items() for index in range(1, count + 1)}
STATUSES = {"pending", "investigated", "implemented", "verified", "rejected"}
CHANGE_TYPES = {"production_change", "test_infrastructure_change"}
REQUESTED_SURFACES = {"T06": "test_infrastructure"}


def validate(document, require_complete=True, check_commits=True):
    errors = []
    commit_files = {}
    items = document.get("items", [])
    ids = [item.get("id") for item in items]
    if len(ids) != len(set(ids)):
        errors.append("Audit IDs must be unique.")
    if set(ids) != EXPECTED:
        errors.append(f"Audit coverage differs from the original list. Missing {sorted(EXPECTED - set(ids))}; extra {sorted(set(ids) - EXPECTED)}.")
    for item in items:
        identifier = item.get("id", "unknown")
        status = item.get("status")
        if status not in STATUSES:
            errors.append(f"{identifier} has an unknown status.")
        if require_complete and status not in {"verified", "rejected"}:
            errors.append(f"{identifier} is not complete ({status}).")
        if status == "verified":
            implementation = item.get("implementation", {})
            change_type = implementation.get("type")
            if change_type not in CHANGE_TYPES:
                errors.append(f"{identifier} needs a requested code replacement, not only added tests or a retention decision.")
            if change_type == "test_infrastructure_change":
                if REQUESTED_SURFACES.get(identifier) != "test_infrastructure":
                    errors.append(f"{identifier} does not request a test-infrastructure replacement.")
                replacement = implementation.get("replacement", {})
                if not replacement.get("before") or not replacement.get("after"):
                    errors.append(f"{identifier} needs the actual test-helper replacement, not only new tests.")
            if not implementation.get("files") or not implementation.get("commits"):
                errors.append(f"{identifier} needs changed files and integrated commits.")
            changed_files = set()
            for commit in implementation.get("commits", []):
                if not check_commits:
                    continue
                if commit not in commit_files:
                    integrated = subprocess.run(
                        ["git", "merge-base", "--is-ancestor", commit, "HEAD"], cwd=ROOT,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    ).returncode == 0
                    diff = subprocess.run(
                        ["git", "diff-tree", "--root", "--no-commit-id", "--name-only", "-r", commit],
                        cwd=ROOT, capture_output=True, text=True,
                    ) if integrated else None
                    commit_files[commit] = set(diff.stdout.splitlines()) if diff and diff.returncode == 0 else None
                files = commit_files[commit]
                if files is None:
                    errors.append(f"{identifier} commit {commit} is not an inspectable integrated commit.")
                else:
                    changed_files.update(files)
            if check_commits:
                missing_changes = set(implementation.get("files", [])) - changed_files
                if missing_changes:
                    errors.append(f"{identifier} implementation files were not changed by its commits. {sorted(missing_changes)}")
            checks = item.get("checks", [])
            if not checks or any(check.get("result") != "pass" or not check.get("evidence") for check in checks):
                errors.append(f"{identifier} needs passing checks with evidence.")
        if status == "rejected":
            rejection = item.get("rejection", {})
            for field in ["reason", "api_evidence", "contract_evidence", "alternative"]:
                if not rejection.get(field):
                    errors.append(f"{identifier} rejection needs {field}.")
        if status in {"verified", "rejected"} and not item.get("review"):
            errors.append(f"{identifier} needs an independent review reference.")
    return errors


def self_test():
    fixture = {"items": [
        {"id": identifier, "status": "rejected", "review": "fixture review", "rejection": {
            "reason": "A different contract is required.", "api_evidence": ["fixed API fixture"],
            "contract_evidence": ["contract fixture"], "alternative": "Keep the required contract.",
        }} for identifier in sorted(EXPECTED)
    ]}
    assert not validate(fixture, check_commits=False)
    duplicate = copy.deepcopy(fixture)
    duplicate["items"][1]["id"] = duplicate["items"][0]["id"]
    assert validate(duplicate, check_commits=False)
    missing = copy.deepcopy(fixture)
    missing["items"].pop()
    assert validate(missing, check_commits=False)
    pending = copy.deepcopy(fixture)
    pending["items"][0]["status"] = "pending"
    assert validate(pending, check_commits=False)
    assert not validate(pending, require_complete=False, check_commits=False)
    tests_only = copy.deepcopy(fixture)
    tests_only["items"][0]["status"] = "verified"
    tests_only["items"][0]["implementation"] = {"type": "tests_only", "files": ["test.rs"], "commits": ["fixture"]}
    tests_only["items"][0]["checks"] = [{"result": "pass", "evidence": "fixture"}]
    assert validate(tests_only, check_commits=False)
    unproven = copy.deepcopy(fixture)
    del unproven["items"][0]["rejection"]["api_evidence"]
    assert validate(unproven, check_commits=False)
    infrastructure = copy.deepcopy(fixture)
    requested = next(item for item in infrastructure["items"] if item["id"] == "T06")
    requested.update(status="verified", implementation={
        "type": "test_infrastructure_change", "files": ["helper.rs"], "commits": ["fixture"],
        "replacement": {"before": "external kill command", "after": "native process-group API"},
    }, checks=[{"result": "pass", "evidence": "fixture"}])
    assert not validate(infrastructure, check_commits=False)
    added_tests = copy.deepcopy(infrastructure)
    next(item for item in added_tests["items"] if item["id"] == "T06")["implementation"].pop("replacement")
    assert validate(added_tests, check_commits=False)
    wrong_surface = copy.deepcopy(infrastructure)
    entry = next(item for item in wrong_surface["items"] if item["id"] == "H01")
    entry.update(status="verified", implementation=copy.deepcopy(requested["implementation"]), checks=requested["checks"])
    assert validate(wrong_surface, check_commits=False)
    unchanged = copy.deepcopy(fixture)
    unchanged["items"][0].update(status="verified", implementation={
        "type": "production_change", "files": ["not-the-committed-implementation.rs"], "commits": ["HEAD"],
    }, checks=[{"result": "pass", "evidence": "fixture"}])
    assert any("were not changed" in error for error in validate(unchanged))
    print("PASS coverage, completion, tests-only, requested infrastructure, changed-file, and rejection gates")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", nargs="?", type=Path, default=ROOT / "docs/refactoring/library-delegation-audit.json")
    parser.add_argument("--allow-pending", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    try:
        document = json.loads(args.ledger.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"Cannot read audit ledger. {error}", file=sys.stderr)
        return 1
    errors = validate(document, require_complete=not args.allow_pending)
    for error in errors:
        print(error, file=sys.stderr)
    counts = {status: sum(item.get("status") == status for item in document["items"]) for status in sorted(STATUSES)}
    print(json.dumps({"items": len(document["items"]), "statuses": counts}, sort_keys=True))
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
