#!/usr/bin/env python3
"""Fetch the pinned three-vrm VRMA sample into the development cache."""
import hashlib
import os
from pathlib import Path
import tempfile
import urllib.request

COMMIT = "1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7"
SHA256 = "38d0fd12d61e896f1a970b5e358ebb41a96c8d5ee8e284496ea18f0ba1f04e7b"
URL = f"https://raw.githubusercontent.com/pixiv/three-vrm/{COMMIT}/packages/three-vrm-animation/examples/models/test.vrma"


def main():
    folder = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene/motions"
    path = folder / "three-vrm-sample.vrma"
    license_text = (Path(__file__).resolve().parents[1] / "licenses/three-vrm-animation.txt").read_bytes()
    if path.exists():
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(URL, timeout=60) as response:
            data = response.read(1024 * 1024 + 1)
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise SystemExit("VRMA checksum mismatch; existing file was not overwritten")
    folder.mkdir(parents=True, exist_ok=True)
    # Keep the upstream notice alongside the cached motion.
    for destination, content in [(folder / "three-vrm-sample.LICENSE.txt", license_text), (path, data)]:
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=folder, prefix=".vrma-", delete=False) as file:
                temporary = Path(file.name)
                file.write(content)
            temporary.replace(destination)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
    print(path)
    print("three-vrm animation sample / Copyright (c) 2019-2026 pixiv Inc. / MIT License")


if __name__ == "__main__":
    main()
