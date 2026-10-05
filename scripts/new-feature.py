#!/usr/bin/env python3
"""Generate a compiling configuration slice and register all four Rust modules."""
import argparse
from pathlib import Path
import re

KEYWORDS = set("as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while abstract become box do final macro override priv typeof unsized virtual yield try gen".split())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name", help="Rust snake_case module name")
    parser.add_argument("--field", choices=("name", "mind"), default="name", help="working example's companion field; adapt it to your domain")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-z][a-z0-9]*(?:_[a-z0-9]+)*", args.name) or args.name in KEYWORDS:
        parser.error("name must be a non-keyword Rust snake_case identifier")
    root = args.root.resolve()
    replacements = {
        "__NAME__": args.name,
        "__TYPE__": "".join(part.capitalize() for part in args.name.split("_")),
        "__PATH__": args.name.replace("_", "-"),
        "__FIELD__": args.field,
        "__MAX_BYTES__": "200" if args.field == "name" else "16_000",
    }
    generated = {
        f"crates/ene-api-types/src/{args.name}.rs": "input.rs.in",
        f"crates/ene-core/src/application/operations/{args.name}.rs": "command.rs.in",
        f"apps/ene-host/src/api/v1/companions/{args.name}.rs": "route.rs.in",
        f"crates/ene-api-client/src/{args.name}.rs": "client.rs.in",
    }
    writes = {}
    for path, template in generated.items():
        destination = root / path
        if destination.exists():
            parser.error(f"refusing to overwrite {destination}")
        content = (root / "templates/configuration" / template).read_text()
        for key, value in replacements.items():
            content = content.replace(key, value)
        writes[destination] = content
    registrations = {
        "crates/ene-api-types/src/lib.rs": ("pub mod sync;", f"pub mod {args.name};\npub use {args.name}::*;\npub mod sync;"),
        "crates/ene-core/src/application/operations/mod.rs": ("mod checks;", f"pub mod {args.name};\nmod checks;"),
        "crates/ene-api-client/src/lib.rs": ("mod resources;", f"mod {args.name};\nmod resources;"),
        "apps/ene-host/src/api/v1/companions/mod.rs": ("mod messages;", f"mod {args.name};\nmod messages;"),
    }
    for path, (anchor, replacement) in registrations.items():
        destination = root / path
        content = destination.read_text()
        if content.count(anchor) != 1 or re.search(rf"\bmod\s+{re.escape(args.name)}\s*;", content):
            parser.error(f"registration is ambiguous or already exists: {destination}")
        content = content.replace(anchor, replacement, 1)
        if path.endswith("companions/mod.rs"):
            anchor = '.nest("/{companion_id}/messages", messages::router())'
            if content.count(anchor) != 1:
                parser.error("companion router anchor has changed; update the template generator")
            content = content.replace(anchor, f'.merge({args.name}::router())\n        {anchor}')
        writes[destination] = content
    # Preflight every destination and registration before touching the repository.
    for path, content in writes.items():
        if not args.dry_run:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        print(path.relative_to(root))
    if not args.dry_run:
        print("Run cargo fmt --all, then cargo test --locked -p ene-host -p ene-api-client.")


if __name__ == "__main__":
    main()
