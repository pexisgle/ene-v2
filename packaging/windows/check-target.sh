#!/usr/bin/env bash
# Build the Windows-only code from a Linux host.
#
# `cargo check` on a Linux host never sees a `#[cfg(windows)]` module, so a
# change to `crates/ene-platform-os` Windows paths would pass every other gate
# and still not compile on Windows. This drops in a `x86_64-pc-windows-gnu`
# standard library plus a mingw linker so those modules are type-checked.
#
# This host has no rustup and its flake pins one target (`wasm32-unknown-unknown`),
# so the Windows std is built with the repo's own flake inputs. A plain
# `rustup target add` is not available here and is forbidden by the project rules.
#
# Usage:
#   packaging/windows/check-target.sh                  # check ene-platform-os
#   packaging/windows/check-target.sh -p ene-desktop   # check another package
#   packaging/windows/check-target.sh --workspace --all-targets
#
# Exit 0 means the Windows code type-checks. It does not link a binary, and it
# does not run anything: this is a compile check, not a Windows test.

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if [ "$#" -eq 0 ]; then
	set -- -p ene-platform-os
fi

# Fail early with the reason rather than inside a nix evaluation error.
if ! command -v nix >/dev/null 2>&1; then
	printf 'FAIL nix is required to provide the Windows standard library\n' >&2
	exit 1
fi

exec nix shell --impure --expr "
let
  flake = builtins.getFlake \"path:${root}\";
  p = import flake.inputs.nixpkgs {
    system = \"x86_64-linux\";
    overlays = [ flake.inputs.rust-overlay.overlays.default ];
  };
  rust = p.rust-bin.stable.\"1.100.0\".default.override {
    targets = [ \"x86_64-pc-windows-gnu\" ];
  };
in p.symlinkJoin {
  name = \"ene-windows-check\";
  paths = [ rust p.clang p.lld p.pkg-config p.pkgsCross.mingwW64.stdenv.cc ];
}
" -c env -C "$root" \
	CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
	CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
	cargo check --target x86_64-pc-windows-gnu "$@"
