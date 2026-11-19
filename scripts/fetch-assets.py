#!/usr/bin/env python3
"""Fetch the pinned Seed-san VRM development sample."""
import argparse
import hashlib
import os
from pathlib import Path
import tempfile
import urllib.request

MODEL = {
    "url": "https://raw.githubusercontent.com/vrm-c/vrm-specification/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san/vrm/Seed-san.vrm",
    "sha256": "624d0d554bc205bbdc33e22a68a2c3c20edebb3e573011ead8878a65e5329b23",
    "path": "models/Seed-san.vrm",
    "source": "https://github.com/vrm-c/vrm-specification/tree/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san",
    "license_url": "https://vrm.dev/licenses/1.0/",
    "credit": "Seed-san model by VirtualCast, Inc. / VRM Public License 1.0; credit required",
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
    cache = (Path(os.environ["LOCALAPPDATA"]) / "ene" / "cache" if os.name == "nt" else Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene")
    path = cache / asset["path"]
    cached = path.exists()
    if cached:
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(asset["url"], timeout=60) as response:
            data = response.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES or hashlib.sha256(data).hexdigest() != asset["sha256"]:
        raise ValueError(f"{path.name}: checksum mismatch; existing file was not overwritten")
    path.parent.mkdir(parents=True, exist_ok=True)
    if not cached:
        write_atomic(path, data)
    print(path)
    print(asset["credit"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("asset", nargs="?", choices=("vrm", "all"), default="vrm")
    parser.parse_args()
    try:
        fetch(MODEL)
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
