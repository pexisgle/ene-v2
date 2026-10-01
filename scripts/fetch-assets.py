#!/usr/bin/env python3
"""Fetch pinned VRM/VRMA development samples; never runs on client startup."""
import argparse
import hashlib
import os
from pathlib import Path
import tempfile
import urllib.request

ASSETS = {
    "vrm": {
        "url": "https://raw.githubusercontent.com/vrm-c/vrm-specification/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san/vrm/Seed-san.vrm",
        "sha256": "624d0d554bc205bbdc33e22a68a2c3c20edebb3e573011ead8878a65e5329b23",
        "path": "models/Seed-san.vrm",
        "license": "Seed-san.md",
        "notice": "Seed-san.LICENSE.md",
        "credit": "Seed-san model by VirtualCast, Inc. / VRM Public License 1.0; credit required",
    },
    "vrma": {
        "url": "https://raw.githubusercontent.com/pixiv/three-vrm/1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7/packages/three-vrm-animation/examples/models/test.vrma",
        "sha256": "38d0fd12d61e896f1a970b5e358ebb41a96c8d5ee8e284496ea18f0ba1f04e7b",
        "path": "motions/three-vrm-sample.vrma",
        "license": "three-vrm-animation.txt",
        "notice": "three-vrm-sample.LICENSE.txt",
        "credit": "three-vrm animation sample / Copyright (c) 2019-2026 pixiv Inc. / MIT License",
    },
}
MAX_BYTES = 32 * 1024 * 1024


def write_atomic(destination, content):
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=destination.parent, prefix=".asset-", delete=False) as file:
            temporary = Path(file.name)
            file.write(content)
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def fetch(asset):
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene"
    path = cache / asset["path"]
    license_text = (Path(__file__).resolve().parents[1] / "licenses" / asset["license"]).read_bytes()
    cached = path.exists()
    if cached:
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(asset["url"], timeout=60) as response:
            data = response.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES or hashlib.sha256(data).hexdigest() != asset["sha256"]:
        raise ValueError(f"{path.name}: checksum mismatch; existing file was not overwritten")
    path.parent.mkdir(parents=True, exist_ok=True)
    write_atomic(path.parent / asset["notice"], license_text)
    if not cached:
        write_atomic(path, data)
    print(path)
    print(asset["credit"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "asset", nargs="?", default="all", choices=["all", *ASSETS],
        help="sample to fetch (default: all)",
    )
    args = parser.parse_args()
    try:
        for name in ASSETS if args.asset == "all" else [args.asset]:
            fetch(ASSETS[name])
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
