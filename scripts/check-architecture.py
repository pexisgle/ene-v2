#!/usr/bin/env python3
"""Check architectural boundaries and ratchet baselines across the workspace."""

from __future__ import annotations

import argparse
import collections
from dataclasses import dataclass
import json
from pathlib import Path
import re
import sys
import tempfile


ROOT = Path(__file__).resolve().parent.parent

# Ratchet baselines (lowered as each refactoring phase lands).
MAX_UI_RPC_REFS = 464
MAX_DB_UPWARD_REFS = 7
DEFAULT_MAX_IMPL_FILES = 3
IMPL_SPREAD_ALLOWLIST: dict[tuple[str, str], int] = {
    ("apps/ene-desktop", "SettingsStudio"): 18,
    ("apps/ene-host", "Store"): 11,
    ("apps/ene-desktop", "Hub"): 6,
    ("apps/ene-desktop", "CompanionPod"): 4,
    ("apps/ene-desktop", "Session"): 4,
    ("crates/ene-plugin-runtime", "Events"): 4,
}

ALLOW_STDERR_MARKER = "architecture:allow-stderr:"
CFG_TEST_RE = re.compile(r"^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
ATTR_LINE_RE = re.compile(r"^\s*#")
UI_RPC_W_RE = re.compile(r"\bw::")
UI_RPC_USE_RE = re.compile(r"\buse\s+ene_rpc\b")
DB_UPWARD_RE = re.compile(r"crate::host::(client|model|permission|providers|work)\b")
INHERENT_IMPL_RE = re.compile(
    r"^\s*impl\b(?:<[^>{]*>)?\s+([A-Z][A-Za-z0-9_]*)\b([^\n{]*)",
    re.MULTILINE,
)
PRINTLN_RE = re.compile(r"\b(?:e?println!)\s*\(")


@dataclass(frozen=True)
class Baselines:
    max_ui_rpc_refs: int = MAX_UI_RPC_REFS
    max_db_upward_refs: int = MAX_DB_UPWARD_REFS
    default_max_impl_files: int = DEFAULT_MAX_IMPL_FILES
    impl_spread_allowlist: dict[tuple[str, str], int] | None = None

    def allowlist(self) -> dict[tuple[str, str], int]:
        return IMPL_SPREAD_ALLOWLIST if self.impl_spread_allowlist is None else self.impl_spread_allowlist


def is_test_or_example(rel_path: Path) -> bool:
    if "tests" in rel_path.parts or "examples" in rel_path.parts:
        return True
    return rel_path.name == "tests.rs" or rel_path.name.endswith(("_tests.rs", "_test.rs"))


def strip_line_comment(line: str) -> str:
    """Strip a trailing `//` comment while preserving `//` inside string literals."""
    in_string = False
    escaped = False
    i = 0
    while i < len(line):
        ch = line[i]
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        else:
            if ch == '"':
                in_string = True
            elif ch == "/" and i + 1 < len(line) and line[i + 1] == "/":
                return line[:i]
        i += 1
    return line


def _scan_rust_delimiters(line: str, state: dict[str, object]) -> tuple[int, bool]:
    """Return net brace delta and whether ';' was seen outside strings/comments."""
    delta = 0
    saw_semi = False
    in_string = bool(state["in_string"])
    in_block_comment = int(state["in_block_comment"])  # type: ignore[arg-type]
    escaped = False
    i = 0
    n = len(line)
    while i < n:
        ch = line[i]
        nxt = line[i + 1] if i + 1 < n else ""
        if in_block_comment > 0:
            if ch == "/" and nxt == "*":
                in_block_comment += 1
                i += 2
                continue
            if ch == "*" and nxt == "/":
                in_block_comment -= 1
                i += 2
                continue
            i += 1
            continue
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            i += 1
            continue
        if ch == "/" and nxt == "/":
            break
        if ch == "/" and nxt == "*":
            in_block_comment += 1
            i += 2
            continue
        if ch == '"':
            in_string = True
            i += 1
            continue
        if ch == "{":
            delta += 1
        elif ch == "}":
            delta -= 1
        elif ch == ";":
            saw_semi = True
        i += 1
    state["in_string"] = in_string
    state["in_block_comment"] = in_block_comment
    return delta, saw_semi


def strip_cfg_test(text: str) -> str:
    """Remove `#[cfg(test)]` items and inline modules from Rust source."""
    lines = text.splitlines(keepends=True)
    kept: list[str] = []
    i = 0
    while i < len(lines):
        if CFG_TEST_RE.match(lines[i]):
            j = i + 1
            while j < len(lines) and ATTR_LINE_RE.match(lines[j]):
                j += 1
            depth = 0
            seen_brace = False
            state: dict[str, object] = {"in_string": False, "in_block_comment": 0}
            k = j
            while k < len(lines):
                delta, saw_semi = _scan_rust_delimiters(lines[k], state)
                if delta > 0:
                    seen_brace = True
                depth += delta
                if seen_brace and depth <= 0:
                    k += 1
                    break
                if not seen_brace and depth == 0 and saw_semi:
                    k += 1
                    break
                k += 1
            i = k
            continue
        kept.append(lines[i])
        i += 1
    return "".join(kept)


def iter_workspace_rs(root: Path) -> list[Path]:
    files: list[Path] = []
    for base in ("apps", "crates"):
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for path in sorted(base_dir.rglob("*.rs")):
            if "target" not in path.parts:
                files.append(path)
    return files


def check_ui_rpc_leak(root: Path, max_refs: int) -> tuple[int, list[str]]:
    ui_dir = root / "apps/ene-desktop/src/ui"
    if not ui_dir.is_dir():
        return 0, []
    total = 0
    for path in sorted(ui_dir.rglob("*.rs")):
        text = path.read_text(encoding="utf-8")
        cleaned = "\n".join(strip_line_comment(line) for line in text.splitlines())
        total += len(UI_RPC_W_RE.findall(cleaned))
        total += len(UI_RPC_USE_RE.findall(cleaned))
    errors: list[str] = []
    if total > max_refs:
        errors.append(
            f"Rule 1 (ui_rpc_leak): found {total} `w::`/`use ene_rpc` refs under "
            f"apps/ene-desktop/src/ui/ (baseline max {max_refs})."
        )
    return total, errors


def check_db_upward_imports(root: Path, max_refs: int) -> tuple[int, list[str]]:
    db_dir = root / "apps/ene-host/src/host/db"
    if not db_dir.is_dir():
        return 0, []
    hits: list[str] = []
    for path in sorted(db_dir.rglob("*.rs")):
        rel = path.relative_to(root)
        if is_test_or_example(rel):
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        for line_no, line in enumerate(prod.splitlines(), 1):
            cleaned = strip_line_comment(line)
            if DB_UPWARD_RE.search(cleaned):
                hits.append(f"{rel.as_posix()}:{line_no}: {cleaned.strip()}")
    errors: list[str] = []
    if len(hits) > max_refs:
        errors.append(
            f"Rule 2 (db_upward_imports): found {len(hits)} upward refs from "
            f"apps/ene-host/src/host/db/ (baseline max {max_refs}): {hits}"
        )
    return len(hits), errors


def check_impl_file_spread(
    root: Path,
    workspace_rs: list[Path],
    default_max: int,
    allowlist: dict[tuple[str, str], int],
) -> tuple[dict[str, int], list[str]]:
    impl_files: dict[tuple[str, str], set[str]] = collections.defaultdict(set)
    for path in workspace_rs:
        rel = path.relative_to(root)
        if is_test_or_example(rel) or rel.name == "build.rs" or len(rel.parts) < 2:
            continue
        crate_dir = "/".join(rel.parts[:2])
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        cleaned = "\n".join(strip_line_comment(line) for line in prod.splitlines())
        for match in INHERENT_IMPL_RE.finditer(cleaned):
            struct_name, rest = match.group(1), match.group(2)
            if re.search(r"\bfor\b", rest):
                continue
            impl_files[(crate_dir, struct_name)].add(rel.as_posix())

    errors: list[str] = []
    over_default: dict[str, int] = {}
    for (crate_dir, struct_name), files in sorted(impl_files.items()):
        limit = allowlist.get((crate_dir, struct_name), default_max)
        count = len(files)
        if count > default_max:
            over_default[f"{crate_dir}::{struct_name}"] = count
        if count > limit:
            errors.append(
                f"Rule 3 (impl_file_spread): {crate_dir}::{struct_name} has inherent `impl` "
                f"blocks across {count} files (max {limit}): {sorted(files)}"
            )
    return over_default, errors


def _has_allow_stderr_comment(raw_lines: list[str], idx: int) -> bool:
    for probe_idx in (idx, idx - 1):
        if probe_idx < 0:
            continue
        line = raw_lines[probe_idx]
        if ALLOW_STDERR_MARKER in line:
            reason = line.split(ALLOW_STDERR_MARKER, 1)[1].strip()
            if reason:
                return True
    return False


def check_no_prod_println(root: Path, workspace_rs: list[Path]) -> tuple[list[str], list[str]]:
    violations: list[str] = []
    for path in workspace_rs:
        rel = path.relative_to(root)
        if is_test_or_example(rel) or rel.name in ("build.rs", "cli.rs"):
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        raw_lines = prod.splitlines()
        is_main_rs = rel.as_posix().endswith("/src/main.rs")
        for idx, raw_line in enumerate(raw_lines):
            cleaned = strip_line_comment(raw_line)
            if not PRINTLN_RE.search(cleaned):
                continue
            if is_main_rs and _has_allow_stderr_comment(raw_lines, idx):
                continue
            violations.append(f"{rel.as_posix()}:{idx + 1}: {raw_line.strip()}")

    errors: list[str] = []
    if violations:
        errors.append(
            f"Rule 4 (no_prod_println): found {len(violations)} forbidden println!/eprintln! "
            f"call(s) in production code: {violations}"
        )
    return violations, errors


def evaluate(root: Path, baselines: Baselines | None = None) -> tuple[dict[str, object], list[str]]:
    cfg = baselines or Baselines()
    workspace_rs = iter_workspace_rs(root)
    ui_count, ui_errors = check_ui_rpc_leak(root, cfg.max_ui_rpc_refs)
    db_count, db_errors = check_db_upward_imports(root, cfg.max_db_upward_refs)
    impl_over_default, impl_errors = check_impl_file_spread(
        root, workspace_rs, cfg.default_max_impl_files, cfg.allowlist()
    )
    println_violations, println_errors = check_no_prod_println(root, workspace_rs)
    errors = [*ui_errors, *db_errors, *impl_errors, *println_errors]
    report: dict[str, object] = {
        "status": "fail" if errors else "pass",
        "rules": {
            "ui_rpc_leak": {"count": ui_count, "max": cfg.max_ui_rpc_refs},
            "db_upward_imports": {"count": db_count, "max": cfg.max_db_upward_refs},
            "impl_file_spread_over_default": impl_over_default,
            "no_prod_println": {"violations": len(println_violations)},
        },
        "errors": errors,
    }
    return report, errors


def _write_fixture(base: Path, files: dict[str, str]) -> None:
    for rel, content in files.items():
        target = base / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def self_test() -> None:
    clean_files = {
        "apps/ene-desktop/src/ui/mod.rs": (
            "// comment mentioning w::Ok and use ene_rpc\n"
            "pub fn ok() {}\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    fn t() { println!(\"ok in test\"); }\n"
            "}\n"
        ),
        "apps/ene-desktop/src/main.rs": (
            "fn main() {\n"
            "    // architecture:allow-stderr: panic fallback\n"
            "    eprintln!(\"fatal\");\n"
            "}\n"
        ),
        "apps/ene-host/src/host/db/mod.rs": (
            "pub struct Store;\n"
            "#[cfg(test)]\n"
            "mod tests { use crate::host::permission::Approval; }\n"
        ),
        "apps/ene-host/src/cli.rs": "pub fn run() { println!(\"cli\"); }\n",
        "apps/ene-host/build.rs": "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        "crates/demo/src/a.rs": "struct S;\nimpl S { fn a() {} }\ntrait T {}\nimpl T for S {}\n",
        "crates/demo/src/b.rs": "impl S { fn b() {} }\nimpl T for S {}\n",
        "crates/demo/src/c.rs": "impl S { fn c() {} }\nimpl T for S {}\n",
        "crates/demo/src/d.rs": "impl T for S {}\n",
    }
    baselines = Baselines(
        max_ui_rpc_refs=0,
        max_db_upward_refs=0,
        default_max_impl_files=3,
        impl_spread_allowlist={("apps/ene-desktop", "SettingsStudio"): 4},
    )

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        _write_fixture(root, clean_files)
        _, errors = evaluate(root, baselines)
        assert not errors, f"expected clean fixture to pass, got {errors}"

        # Rule 1 violation
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/ui/mod.rs": "use ene_rpc as w;\nfn f(_: w::Empty) {}\n",
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 1" in e for e in errors), f"expected Rule 1 error, got {errors}"

        # Rule 2 violation
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/host/db/mod.rs": "use crate::host::permission::Approval;\n",
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 2" in e for e in errors), f"expected Rule 2 error, got {errors}"

        # Rule 3 violation (4th file with inherent impl S, and allowlist enforcement)
        _write_fixture(
            root,
            {
                **clean_files,
                "crates/demo/src/d.rs": "impl S { fn d() {} }\n",
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 3" in e for e in errors), f"expected Rule 3 error, got {errors}"

        # Rule 4 violation in lib.rs even with marker comment
        _write_fixture(
            root,
            {
                **clean_files,
                "crates/demo/src/lib.rs": (
                    "// architecture:allow-stderr: should only be allowed in main.rs\n"
                    "pub fn bad() { eprintln!(\"nope\"); }\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 4" in e for e in errors), f"expected Rule 4 error, got {errors}"

        # Rule 4 violation in main.rs with empty reason
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/main.rs": (
                    "fn main() {\n"
                    "    // architecture:allow-stderr:   \n"
                    "    eprintln!(\"fatal\");\n"
                    "}\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 4" in e for e in errors), f"expected Rule 4 empty-reason error, got {errors}"

    # Verify strip_cfg_test keeps code after an inline test module and ignores braces in strings/comments.
    sample = (
        "fn before() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    // { stray brace in comment\n"
        "    fn t() { let _ = \"}\"; }\n"
        "}\n"
        "fn after() {}\n"
    )
    stripped = strip_cfg_test(sample)
    assert "fn before()" in stripped and "fn after()" in stripped and "mod tests" not in stripped

    print("PASS all 4 architecture rules and self-test fixtures")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON output.")
    parser.add_argument("--self-test", action="store_true", help="Run built-in unit/fixture checks.")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    report, errors = evaluate(ROOT)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        rules = report["rules"]
        assert isinstance(rules, dict)
        ui = rules["ui_rpc_leak"]
        db = rules["db_upward_imports"]
        println = rules["no_prod_println"]
        spread = rules["impl_file_spread_over_default"]
        print(
            f"architecture check: ui_rpc_leak={ui['count']}/{ui['max']}, "
            f"db_upward_imports={db['count']}/{db['max']}, "
            f"impl_over_default={spread}, "
            f"no_prod_println={println['violations']}"
        )
        for error in errors:
            print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
