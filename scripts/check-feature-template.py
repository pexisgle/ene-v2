#!/usr/bin/env python3
"""Compile and exercise generated API slices without changing the current checkout."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    root = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="ene-template-") as directory:
        copy = Path(directory) / "repo"
        shutil.copytree(root, copy, ignore=shutil.ignore_patterns(".git", "target", "__pycache__"))
        env = os.environ.copy()
        env.setdefault("CARGO_TARGET_DIR", str(root / "target"))
        for name, field in [("template_name", "name"), ("template_mind", "mind")]:
            subprocess.run(["python3", str(copy / "scripts/new-feature.py"), name, "--field", field], check=True)
        subprocess.run(["cargo", "fmt", "--all"], cwd=copy, env=env, check=True)
        subprocess.run(["cargo", "test", "--locked", "-p", "ene-host", "api::v1::companions::template_"], cwd=copy, env=env, check=True)
        subprocess.run(["cargo", "clippy", "--locked", "-p", "ene-host", "-p", "ene-api-client", "--all-targets", "--all-features", "--", "-D", "warnings"], cwd=copy, env=env, check=True)


if __name__ == "__main__":
    main()
