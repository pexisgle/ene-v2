#!/usr/bin/env python3
"""Fetch the pinned official development model; never runs on client startup."""
import hashlib
import os
from pathlib import Path
import urllib.request

COMMIT = "821c11b250d8c70d5804ee13431e42bee56ea9c0"
SHA256 = "624d0d554bc205bbdc33e22a68a2c3c20edebb3e573011ead8878a65e5329b23"
URL = f"https://raw.githubusercontent.com/vrm-c/vrm-specification/{COMMIT}/samples/Seed-san/vrm/Seed-san.vrm"
path = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "ene/models/Seed-san.vrm"
data = path.read_bytes() if path.exists() else urllib.request.urlopen(URL, timeout=60).read()
if hashlib.sha256(data).hexdigest() != SHA256:
    raise SystemExit("Seed-san checksum mismatch; existing file was not overwritten")
path.parent.mkdir(parents=True, exist_ok=True)
temporary = path.with_suffix(".download")
temporary.write_bytes(data)
temporary.replace(path)
print(path)
print("Seed-san model by VirtualCast, Inc. / VRM Public License 1.0; credit required")
