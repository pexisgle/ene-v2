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
        "source": "https://github.com/vrm-c/vrm-specification/tree/821c11b250d8c70d5804ee13431e42bee56ea9c0/samples/Seed-san",
        "license_url": "https://vrm.dev/licenses/1.0/",
        "credit": "Seed-san model by VirtualCast, Inc. / VRM Public License 1.0; credit required",
    },
    "vrma-sample": {
        "url": "https://raw.githubusercontent.com/pixiv/three-vrm/1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7/packages/three-vrm-animation/examples/models/test.vrma",
        "sha256": "38d0fd12d61e896f1a970b5e358ebb41a96c8d5ee8e284496ea18f0ba1f04e7b",
        "path": "motions/three-vrm-sample.vrma",
        "license_url": "https://github.com/pixiv/three-vrm/blob/1b4fc0cc7ef39a49d62bb7a66dcfeca8f65316f7/LICENSE",
        "credit": "three-vrm animation sample / Copyright (c) 2019-2026 pixiv Inc. / MIT License",
    },
    "vrma-dance": {
        "url": "https://raw.githubusercontent.com/3dkit-online/3d-motion-avatar-viewer-cases/01b3f24cbf2a19103e0808973fac4c4c47233d06/cases/vrma-needs-avatar/motion.vrma",
        "sha256": "878e8a7776a90b2a48ab5d2f5c57d83f11b9e537ab02eaeb66bd6dab27a2ed74",
        "path": "motions/quaternius-dance.vrma",
        "source": "https://quaternius.com/packs/universalanimationlibrary.html",
        "license_url": "https://github.com/3dkit-online/3d-motion-avatar-viewer-cases/blob/01b3f24cbf2a19103e0808973fac4c4c47233d06/LICENSE",
        "credit": "Dance by Quaternius / VRMA conversion by 3D Retarget Online / CC0-1.0",
    },
    "vrma-walk": {
        "url": "https://raw.githubusercontent.com/SanHsien/voxavatar/86f9ef8c67c21c9fa5e332843d76da877e9162f0/public/assets/animations/walk.vrma",
        "sha256": "51c4158488a86a6bb99b978fb4d269e1798f9749bceaf0420473bae413fc3271",
        "path": "motions/josie-walk.vrma",
        "source": "https://booth.pm/ja/items/7861818",
        "license_url": "https://github.com/SanHsien/voxavatar/blob/86f9ef8c67c21c9fa5e332843d76da877e9162f0/public/assets/manifest.json",
        "credit": "Josie Walk by Jen Jell / VRMA retarget by sashii / CC0-1.0",
    },
    "vrma-slow-run": {
        "url": "https://raw.githubusercontent.com/SanHsien/voxavatar/86f9ef8c67c21c9fa5e332843d76da877e9162f0/public/assets/animations/run-slow.vrma",
        "sha256": "54fff21bbbbbe6364dc812f39dd849eadb61de3888b556580143d386592d17e5",
        "path": "motions/josie-slow-run.vrma",
        "source": "https://booth.pm/ja/items/7861818",
        "license_url": "https://github.com/SanHsien/voxavatar/blob/86f9ef8c67c21c9fa5e332843d76da877e9162f0/public/assets/manifest.json",
        "credit": "Josie SlowRun by Jen Jell / VRMA retarget by sashii / CC0-1.0",
    },
}
GROUPS = {
    "all": tuple(ASSETS),
    "vrma": tuple(name for name in ASSETS if name.startswith("vrma-")),
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
    parser.add_argument(
        "asset", nargs="?", default="all", choices=[*GROUPS, *ASSETS],
        help="all: model and all motions; vrma: all motions; otherwise one sample (default: all)",
    )
    args = parser.parse_args()
    try:
        for name in GROUPS.get(args.asset, (args.asset,)):
            fetch(ASSETS[name])
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
