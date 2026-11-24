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
MAX_UI_RPC_REFS = 0
MAX_DB_UPWARD_REFS = 0
MAX_PROVIDERS_MODEL_REFS = 0
MAX_HOST_WORLD_REFS = 159
# Rule 7: domain crates that must not depend on the wire protocol crate.
MAX_DOMAIN_RPC_DEPS = 0
# Rule 8: production `unsafe` occurrences under apps/ene-desktop.
MAX_DESKTOP_UNSAFE = 0
# Rule 9: production files longer than PROD_FILE_LINE_LIMIT.
MAX_LONG_PROD_FILES = 35
# Rule 10: production `#[derive(Resource)]` items under the RPC transport layer.
MAX_RPC_ECS_RESOURCES = 0
# Rule 11: production `block_on`/`block_in_place` sites. A ratchet rather than
# a hard zero, because three callers legitimately own the runtime they block:
#   apps/ene-desktop/src/main.rs            the process entry point, once
#   apps/ene-desktop/src/plugins.rs         startup loader, before any actor runs
#   crates/ene-plugin-runtime/.../invocation.rs  the guest call's own runtime
# The host's kernel thread is NOT such a caller: it runs every queued command,
# so `permission/gate.rs` blocking there stalls unrelated work. That site is
# what this rule exists to drive down.
MAX_PROD_BLOCK_ON = 7
DEFAULT_MAX_IMPL_FILES = 3
IMPL_SPREAD_ALLOWLIST: dict[tuple[str, str], int] = {
    ("crates/ene-plugin-runtime", "Events"): 4,
}

# `ene-domain` is the shared kernel; the rest are L0 domain crates.
DOMAIN_CRATES = ("ene-domain", "ene-policy", "ene-catalog", "ene-plugin", "ene-voice")
PROD_FILE_LINE_LIMIT = 500

ALLOW_STDERR_MARKER = "architecture:allow-stderr:"
CFG_TEST_RE = re.compile(r"^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
ATTR_LINE_RE = re.compile(r"^\s*#")
UI_RPC_W_RE = re.compile(r"\bw::")
UI_RPC_PATH_RE = re.compile(r"\b(?:ene_rpc|buffa|connectrpc)::")
UI_RPC_USE_RE = re.compile(r"\buse\s+(?:ene_rpc|buffa|connectrpc)\b")
DB_UPWARD_RE = re.compile(r"crate::host::(client|model|permission|providers|work)\b")
PROVIDERS_MODEL_RE = re.compile(r"\b(?:crate::host|super)::model\b")
HOST_WORLD_REF_RE = re.compile(r"&\s*(?:mut\s+)?World\b")
INHERENT_IMPL_RE = re.compile(
    r"^\s*impl\b(?:<[^>{]*>)?\s+([A-Z][A-Za-z0-9_]*)\b([^\n{]*)",
    re.MULTILINE,
)
PRINTLN_RE = re.compile(r"\b(?:e?println!)\s*\(")
UNSAFE_RE = re.compile(r"\bunsafe\b")
DERIVE_RE = re.compile(r"#\s*\[\s*derive\s*\(([^)]*)\)\s*\]")
RPC_DEP_RE = re.compile(r"^\s*ene-rpc\s*[.=]", re.MULTILINE)
# `block_on`/`block_in_place` park an OS thread until the future finishes. In
# the host that thread is the kernel actor, so a call made from there stalls
# every other job queued behind it.
BLOCK_ON_RE = re.compile(r"\b(?:block_on|block_in_place)\s*\(")


@dataclass(frozen=True)
class Baselines:
    max_ui_rpc_refs: int = MAX_UI_RPC_REFS
    max_db_upward_refs: int = MAX_DB_UPWARD_REFS
    max_providers_model_refs: int = MAX_PROVIDERS_MODEL_REFS
    max_host_world_refs: int = MAX_HOST_WORLD_REFS
    default_max_impl_files: int = DEFAULT_MAX_IMPL_FILES
    impl_spread_allowlist: dict[tuple[str, str], int] | None = None
    max_domain_rpc_deps: int = MAX_DOMAIN_RPC_DEPS
    max_desktop_unsafe: int = MAX_DESKTOP_UNSAFE
    max_long_prod_files: int = MAX_LONG_PROD_FILES
    prod_file_line_limit: int = PROD_FILE_LINE_LIMIT
    max_rpc_ecs_resources: int = MAX_RPC_ECS_RESOURCES
    max_prod_block_on: int = MAX_PROD_BLOCK_ON

    def allowlist(self) -> dict[tuple[str, str], int]:
        return IMPL_SPREAD_ALLOWLIST if self.impl_spread_allowlist is None else self.impl_spread_allowlist


def is_test_or_example(rel_path: Path) -> bool:
    if any(
        part in ("tests", "examples") or part.endswith(("_tests", "_test"))
        for part in rel_path.parts[:-1]
    ):
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
        total += len(UI_RPC_PATH_RE.findall(cleaned))
        total += len(UI_RPC_USE_RE.findall(cleaned))
    errors: list[str] = []
    if total > max_refs:
        errors.append(
            f"Rule 1 (ui_rpc_leak): found {total} wire/RPC (`w::`/`ene_rpc`/`buffa`/`connectrpc`) "
            f"refs under apps/ene-desktop/src/ui/ (baseline max {max_refs})."
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


def check_providers_no_model_import(root: Path, max_refs: int) -> tuple[int, list[str]]:
    providers_dir = root / "apps/ene-host/src/host/providers"
    if not providers_dir.is_dir():
        return 0, []
    hits: list[str] = []
    for path in sorted(providers_dir.rglob("*.rs")):
        rel = path.relative_to(root)
        if is_test_or_example(rel):
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        for line_no, line in enumerate(prod.splitlines(), 1):
            cleaned = strip_line_comment(line)
            if PROVIDERS_MODEL_RE.search(cleaned):
                hits.append(f"{rel.as_posix()}:{line_no}: {cleaned.strip()}")
    errors: list[str] = []
    if len(hits) > max_refs:
        errors.append(
            f"Rule 5 (providers_no_model_import): found {len(hits)} model import(s) in "
            f"apps/ene-host/src/host/providers/ (baseline max {max_refs}): {hits}"
        )
    return len(hits), errors


def check_host_world_refs(root: Path, max_refs: int) -> tuple[dict[str, object], list[str]]:
    host_dir = root / "apps/ene-host/src/host"
    if not host_dir.is_dir():
        return {"count": 0, "max": max_refs, "file_count": 0, "files": {}}, []
    per_file: dict[str, int] = {}
    total = 0
    for path in sorted(host_dir.rglob("*.rs")):
        rel = path.relative_to(root)
        if is_test_or_example(rel):
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        count = sum(
            len(HOST_WORLD_REF_RE.findall(strip_line_comment(line)))
            for line in prod.splitlines()
        )
        if count > 0:
            per_file[rel.as_posix()] = count
            total += count
    sorted_files = dict(sorted(per_file.items(), key=lambda item: (-item[1], item[0])))
    errors: list[str] = []
    if total > max_refs:
        top = list(sorted_files.items())[:8]
        errors.append(
            f"Rule 6 (host_world_refs): found {total} production World refs in "
            f"apps/ene-host/src/host/ (baseline max {max_refs}) across {len(per_file)} files; "
            f"top={top}"
        )
    return {
        "count": total,
        "max": max_refs,
        "file_count": len(per_file),
        "files": sorted_files,
    }, errors


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


def production_files(root: Path, base: str, workspace_rs: list[Path] | None = None) -> list[Path]:
    """Return non-test Rust files under `root/base`."""
    base_dir = root / base
    if not base_dir.is_dir():
        return []
    return sorted(
        path
        for path in base_dir.rglob("*.rs")
        if not is_test_or_example(path.relative_to(root))
    )


def check_domain_no_rpc_dep(root: Path, max_deps: int) -> tuple[dict[str, object], list[str]]:
    """Rule 7: L0 domain crates must not depend on the wire crate `ene-rpc`."""
    offenders: list[str] = []
    seen: list[str] = []
    for crate in DOMAIN_CRATES:
        manifest = root / "crates" / crate / "Cargo.toml"
        if not manifest.is_file():
            continue
        seen.append(crate)
        if RPC_DEP_RE.search(manifest.read_text(encoding="utf-8")):
            offenders.append(crate)
    errors: list[str] = []
    if len(offenders) > max_deps:
        errors.append(
            f"Rule 7 (domain_no_rpc_dep): {len(offenders)} domain crate(s) depend on `ene-rpc` "
            f"(baseline max {max_deps}): {offenders}"
        )
    return {
        "count": len(offenders),
        "max": max_deps,
        "crates_checked": seen,
        "offenders": offenders,
    }, errors


def check_desktop_unsafe(root: Path, max_refs: int) -> tuple[dict[str, object], list[str]]:
    """Rule 8: production `unsafe` must stay out of the desktop app crate."""
    per_file: dict[str, int] = {}
    for path in production_files(root, "apps/ene-desktop/src"):
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        count = sum(
            len(UNSAFE_RE.findall(strip_line_comment(line))) for line in prod.splitlines()
        )
        if count:
            per_file[path.relative_to(root).as_posix()] = count
    total = sum(per_file.values())
    sorted_files = dict(sorted(per_file.items(), key=lambda item: (-item[1], item[0])))
    errors: list[str] = []
    if total > max_refs:
        errors.append(
            f"Rule 8 (app_unsafe_count): found {total} production `unsafe` occurrence(s) in "
            f"apps/ene-desktop (baseline max {max_refs}); top={list(sorted_files.items())[:5]}"
        )
    return {"count": total, "max": max_refs, "files": sorted_files}, errors


def check_prod_block_on(
    root: Path, workspace_rs: list[Path], max_sites: int
) -> tuple[dict[str, object], list[str]]:
    """Rule 11: count production `block_on`/`block_in_place` sites.

    Both park the calling thread. On the host's kernel thread that thread runs
    every queued command, so one blocking call stalls unrelated work.
    """
    per_file: dict[str, int] = {}
    for path in workspace_rs:
        rel = path.relative_to(root)
        if is_test_or_example(rel) or rel.name == "build.rs":
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        count = sum(
            len(BLOCK_ON_RE.findall(strip_line_comment(line))) for line in prod.splitlines()
        )
        if count:
            per_file[rel.as_posix()] = count
    total = sum(per_file.values())
    sorted_files = dict(sorted(per_file.items(), key=lambda item: (-item[1], item[0])))
    errors: list[str] = []
    if total > max_sites:
        errors.append(
            f"Rule 11 (prod_block_on): found {total} production blocking call(s) "
            f"(baseline max {max_sites}); top={list(sorted_files.items())[:5]}. "
            "Await the future instead, or move the work off the thread that must stay free."
        )
    return {"count": total, "max": max_sites, "files": sorted_files}, errors


def check_max_prod_file_lines(
    root: Path, workspace_rs: list[Path], max_files: int, limit: int
) -> tuple[dict[str, object], list[str]]:
    """Rule 9: cap how many production files exceed the line limit."""
    over: list[tuple[int, str]] = []
    for path in workspace_rs:
        rel = path.relative_to(root)
        if is_test_or_example(rel) or rel.name == "build.rs":
            continue
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        lines = len(prod.splitlines())
        if lines > limit:
            over.append((lines, rel.as_posix()))
    over.sort(reverse=True)
    errors: list[str] = []
    if len(over) > max_files:
        errors.append(
            f"Rule 9 (max_prod_file_lines): {len(over)} production file(s) exceed {limit} lines "
            f"(baseline max {max_files}); largest={over[:5]}"
        )
    return {
        "count": len(over),
        "max": max_files,
        "limit": limit,
        "files": {name: lines for lines, name in over},
    }, errors


def check_rpc_no_ecs_resource(root: Path, max_resources: int) -> tuple[dict[str, object], list[str]]:
    """Rule 10: the RPC transport layer must not define ECS resources."""
    found: dict[str, list[str]] = {}
    for path in production_files(root, "apps/ene-host/src/server/rpc"):
        prod = strip_cfg_test(path.read_text(encoding="utf-8"))
        names = [
            match.group(0)
            for match in DERIVE_RE.finditer(prod)
            if re.search(r"\bResource\b", match.group(1))
        ]
        if names:
            found[path.relative_to(root).as_posix()] = names
    total = sum(len(v) for v in found.values())
    errors: list[str] = []
    if total > max_resources:
        errors.append(
            f"Rule 10 (rpc_no_ecs_resource): found {total} `#[derive(Resource)]` in "
            f"apps/ene-host/src/server/rpc/ (baseline max {max_resources}): {found}"
        )
    return {"count": total, "max": max_resources, "files": found}, errors


def evaluate(root: Path, baselines: Baselines | None = None) -> tuple[dict[str, object], list[str]]:
    cfg = baselines or Baselines()
    workspace_rs = iter_workspace_rs(root)
    ui_count, ui_errors = check_ui_rpc_leak(root, cfg.max_ui_rpc_refs)
    db_count, db_errors = check_db_upward_imports(root, cfg.max_db_upward_refs)
    providers_model_count, providers_model_errors = check_providers_no_model_import(
        root, cfg.max_providers_model_refs
    )
    host_world_report, host_world_errors = check_host_world_refs(
        root, cfg.max_host_world_refs
    )
    impl_over_default, impl_errors = check_impl_file_spread(
        root, workspace_rs, cfg.default_max_impl_files, cfg.allowlist()
    )
    println_violations, println_errors = check_no_prod_println(root, workspace_rs)
    domain_rpc_report, domain_rpc_errors = check_domain_no_rpc_dep(root, cfg.max_domain_rpc_deps)
    unsafe_report, unsafe_errors = check_desktop_unsafe(root, cfg.max_desktop_unsafe)
    long_files_report, long_files_errors = check_max_prod_file_lines(
        root, workspace_rs, cfg.max_long_prod_files, cfg.prod_file_line_limit
    )
    rpc_ecs_report, rpc_ecs_errors = check_rpc_no_ecs_resource(root, cfg.max_rpc_ecs_resources)
    block_on_report, block_on_errors = check_prod_block_on(
        root, workspace_rs, cfg.max_prod_block_on
    )
    errors = [
        *ui_errors,
        *db_errors,
        *impl_errors,
        *println_errors,
        *providers_model_errors,
        *host_world_errors,
        *domain_rpc_errors,
        *unsafe_errors,
        *long_files_errors,
        *rpc_ecs_errors,
        *block_on_errors,
    ]
    report: dict[str, object] = {
        "status": "fail" if errors else "pass",
        "rules": {
            "ui_rpc_leak": {"count": ui_count, "max": cfg.max_ui_rpc_refs},
            "db_upward_imports": {"count": db_count, "max": cfg.max_db_upward_refs},
            "providers_no_model_import": {
                "count": providers_model_count,
                "max": cfg.max_providers_model_refs,
            },
            "host_world_refs": host_world_report,
            "impl_file_spread_over_default": impl_over_default,
            "no_prod_println": {"violations": len(println_violations)},
            "domain_no_rpc_dep": domain_rpc_report,
            "app_unsafe_count": unsafe_report,
            "max_prod_file_lines": long_files_report,
            "rpc_no_ecs_resource": rpc_ecs_report,
            "prod_block_on": block_on_report,
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
        "apps/ene-host/src/host/providers/grant.rs": (
            "pub struct Grant;\n"
            "#[cfg(test)]\n"
            "mod tests { use crate::host::model::Credentials; }\n"
        ),
        "apps/ene-host/src/host/providers/safety_tests/mod.rs": (
            "use crate::host::model::Credentials;\n"
        ),
        "apps/ene-host/src/host/entry.rs": (
            "// comment mentioning &World should be ignored\n"
            "pub fn run(world: &mut World) { let _ = world; }\n"
            "#[cfg(test)]\n"
            "mod tests { fn helper(_: &World) {} }\n"
        ),
        "apps/ene-host/src/host/entry_tests.rs": "fn test_helper(_: &mut World) {}\n",
        "apps/ene-host/src/cli.rs": "pub fn run() { println!(\"cli\"); }\n",
        "apps/ene-host/build.rs": "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        "crates/demo/src/a.rs": "struct S;\nimpl S { fn a() {} }\ntrait T {}\nimpl T for S {}\n",
        "crates/demo/src/b.rs": "impl S { fn b() {} }\nimpl T for S {}\n",
        "crates/demo/src/c.rs": "impl S { fn c() {} }\nimpl T for S {}\n",
        "crates/demo/src/d.rs": "impl T for S {}\n",
        "crates/ene-policy/Cargo.toml": "[package]\nname = \"ene-policy\"\n",
        "crates/ene-catalog/Cargo.toml": "[dependencies]\nserde = \"1\"\n",
        "apps/ene-desktop/src/tray.rs": "pub fn show() {}\n",
        "apps/ene-host/src/server/rpc/chat.rs": "pub fn chat() {}\n",
    }
    baselines = Baselines(
        max_ui_rpc_refs=0,
        max_db_upward_refs=0,
        max_providers_model_refs=0,
        max_host_world_refs=1,
        default_max_impl_files=3,
        impl_spread_allowlist={("apps/ene-desktop", "SettingsStudio"): 4},
        max_domain_rpc_deps=0,
        max_desktop_unsafe=0,
        max_long_prod_files=0,
        prod_file_line_limit=20,
        max_rpc_ecs_resources=0,
        max_prod_block_on=0,
    )

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        _write_fixture(root, clean_files)
        _, errors = evaluate(root, baselines)
        assert not errors, f"expected clean fixture to pass, got {errors}"

        # Rule 1 violation (`w::` / `use ene_rpc`)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/ui/mod.rs": "use ene_rpc as w;\nfn f(_: w::Empty) {}\n",
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 1" in e for e in errors), f"expected Rule 1 error, got {errors}"

        # Rule 1 violation (`ene_rpc::` path and `use connectrpc`)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/ui/mod.rs": (
                    "use connectrpc::ConnectError;\n"
                    "fn check(u: &str) -> Result<(), ene_rpc::RemoteEndpointError> { let _ = u; Ok(()) }\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 1" in e for e in errors), f"expected Rule 1 error for ene_rpc::/connectrpc, got {errors}"

        # Rule 11 violation: a production `block_on`, and a test-only one that
        # must not count.
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/host/gate.rs": (
                    "pub fn g() { let _ = runtime.block_on(async {}); }\n"
                    "#[cfg(test)]\n"
                    "mod tests {\n"
                    "    fn t() { runtime.block_on(async {}); }\n"
                    "}\n"
                ),
            },
        )
        report, errors = evaluate(root, baselines)
        assert any("Rule 11" in e for e in errors), f"expected Rule 11 error, got {errors}"
        block_on = report["rules"]["prod_block_on"]
        assert block_on["count"] == 1, f"tests must not count toward Rule 11, got {block_on}"

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

        # Rule 5 violation (`providers -> model` in production code)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/host/providers/grant.rs": "use crate::host::model::Credentials;\n",
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 5" in e for e in errors), f"expected Rule 5 error, got {errors}"

        # Rule 6 violation (`&World` / `&mut World` exceeding baseline in apps/ene-host/src/host/)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/host/entry.rs": (
                    "pub fn first(_: &World) {}\n"
                    "pub fn second(_: &mut World) {}\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 6" in e for e in errors), f"expected Rule 6 error, got {errors}"

        # Rule 7 violation (a domain crate depends on `ene-rpc`)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/host/entry.rs": "pub fn ok() {}\n",
                "crates/ene-policy/Cargo.toml": (
                    "[package]\nname = \"ene-policy\"\n\n[dependencies]\nene-rpc.workspace = true\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 7" in e for e in errors), f"expected Rule 7 error, got {errors}"

        # Rule 8 violation (production `unsafe` in the desktop app)
        _write_fixture(
            root,
            {
                **clean_files,
                "crates/ene-policy/Cargo.toml": "[package]\nname = \"ene-policy\"\n",
                "apps/ene-desktop/src/tray.rs": (
                    "pub fn show() {\n"
                    "    // SAFETY: fixture\n"
                    "    unsafe { hide() }\n"
                    "}\n"
                    "#[cfg(test)]\n"
                    "mod tests { fn t() { unsafe { ok() } } }\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 8" in e for e in errors), f"expected Rule 8 error, got {errors}"

        # Rule 8 must ignore `unsafe` that only appears in a comment or test module.
        _write_fixture(
            root,
            {
                **clean_files,
                "crates/ene-policy/Cargo.toml": "[package]\nname = \"ene-policy\"\n",
                "apps/ene-desktop/src/tray.rs": (
                    "// no unsafe here\n"
                    "pub fn show() {}\n"
                    "#[cfg(test)]\n"
                    "mod tests { fn t() { unsafe { ok() } } }\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert not any("Rule 8" in e for e in errors), f"unexpected Rule 8 error, got {errors}"

        # Rule 9 violation (a production file over the line limit)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/big.rs": "pub fn f() {}\n" * 30,
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 9" in e for e in errors), f"expected Rule 9 error, got {errors}"

        # Rule 9 must ignore a long `#[cfg(test)]` module in an otherwise small file.
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-desktop/src/big.rs": (
                    "pub fn f() {}\n#[cfg(test)]\nmod tests {\n" + "    fn t() {}\n" * 40 + "}\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert not any("Rule 9" in e for e in errors), f"unexpected Rule 9 error, got {errors}"

        # Rule 10 violation (an ECS `Resource` derived in the RPC transport layer)
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/server/rpc/chat.rs": (
                    "#[derive(bevy_ecs::prelude::Resource)]\nstruct Observed(u8);\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert any("Rule 10" in e for e in errors), f"expected Rule 10 error, got {errors}"

        # Rule 10 must ignore a `Resource` that only a test module defines.
        _write_fixture(
            root,
            {
                **clean_files,
                "apps/ene-host/src/server/rpc/chat.rs": (
                    "pub fn chat() {}\n"
                    "#[cfg(test)]\n"
                    "mod tests {\n"
                    "    #[derive(bevy_ecs::prelude::Resource)]\n"
                    "    struct Frames(u64);\n"
                    "}\n"
                ),
            },
        )
        _, errors = evaluate(root, baselines)
        assert not any("Rule 10" in e for e in errors), f"unexpected Rule 10 error, got {errors}"

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

    print("PASS all 10 architecture rules and self-test fixtures")


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
        pm = rules["providers_no_model_import"]
        hw = rules["host_world_refs"]
        println = rules["no_prod_println"]
        spread = rules["impl_file_spread_over_default"]
        domain_rpc = rules["domain_no_rpc_dep"]
        unsafe = rules["app_unsafe_count"]
        long_files = rules["max_prod_file_lines"]
        rpc_ecs = rules["rpc_no_ecs_resource"]
        block_on = rules["prod_block_on"]
        print(
            f"architecture check: ui_rpc_leak={ui['count']}/{ui['max']}, "
            f"db_upward_imports={db['count']}/{db['max']}, "
            f"providers_no_model_import={pm['count']}/{pm['max']}, "
            f"host_world_refs={hw['count']}/{hw['max']}, "
            f"impl_over_default={spread}, "
            f"no_prod_println={println['violations']}, "
            f"domain_no_rpc_dep={domain_rpc['count']}/{domain_rpc['max']}, "
            f"app_unsafe_count={unsafe['count']}/{unsafe['max']}, "
            f"max_prod_file_lines={long_files['count']}/{long_files['max']}, "
            f"rpc_no_ecs_resource={rpc_ecs['count']}/{rpc_ecs['max']}, "
            f"prod_block_on={block_on['count']}/{block_on['max']}"
        )
        for error in errors:
            print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
