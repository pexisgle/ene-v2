#!/usr/bin/env python3
"""Run the real Extism PDK integration test with an explicitly built Wasm guest.

Build only ene-openrouter-wasm for wasm32-unknown-unknown, keeping native
client/server features out of the guest dependency graph.
"""
import argparse
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--wasm", type=Path, default=Path("target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm"))
args = parser.parse_args()
env = os.environ.copy()
env["ENE_TEST_PDK_WASM"] = str(args.wasm.resolve(strict=True))
subprocess.run(["cargo", "test", "--locked", "-p", "ene-plugin-runner", "--test", "pdk", "--", "--ignored"], env=env, check=True)
