#!/usr/bin/env bash
# Type-check the Windows-only code from a Linux host.
#
# CI runs its own matrix leg on `windows-latest`, so this is NOT a CI gate. It
# exists so a contributor on Linux can check `#[cfg(windows)]` code before
# pushing, because a plain `cargo check` on Linux never parses those modules and
# a Windows-only break would otherwise only surface after pushing.
#
# It drops in an `x86_64-pc-windows-gnu` standard library plus a mingw linker so
# those modules are type-checked.
#
# This host has no rustup and its flake pins one target (`wasm32-unknown-unknown`),
# so the Windows std is built with the repo's own flake inputs. A plain
# `rustup target add` is not available here and is forbidden by the project rules.
#
# Usage:
#   packaging/windows/check-target.sh                  # check ene-platform-os
#   packaging/windows/check-target.sh -p ene-work      # check another package
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

# One expression, evaluated twice: the include dir for the C dependencies and
# the tool set to build with. Kept as a file so no shell layer has to escape
# nix interpolation.
expr_file="$(mktemp)"
trap 'rm -f "$expr_file"' EXIT
cat >"$expr_file" <<'NIX'
let
  flake = builtins.getFlake "path:@ROOT@";
  p = import flake.inputs.nixpkgs {
    system = "x86_64-linux";
    overlays = [ flake.inputs.rust-overlay.overlays.default ];
  };
  rust = p.rust-bin.stable."1.100.0".default.override {
    targets = [ "x86_64-pc-windows-gnu" ];
  };
  # `pthreads` supplies the pthread.h and sched.h that aws-lc-sys needs when a
  # crate in the graph links rustls. Without it the check fails inside C, which
  # says nothing about the Rust code under test.
  pthreads = p.pkgsCross.mingwW64.windows.pthreads;
  tools = p.symlinkJoin {
    name = "ene-windows-check";
    paths = [ rust p.clang p.lld p.pkg-config p.pkgsCross.mingwW64.stdenv.cc pthreads ];
  };
in {
  inherit tools;
  include = "${pthreads}/include";
}
NIX

sed -i "s|@ROOT@|$root|" "$expr_file"

include_dir="$(nix eval --impure --raw --expr "((import $expr_file).include)" 2>/dev/null)"
if [ -z "$include_dir" ]; then
	printf 'FAIL cannot resolve the mingw pthreads headers\n' >&2
	exit 1
fi

exec nix shell --impure --file "$expr_file" tools -c env -C "$root" \
	CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
	CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
	CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
	CPATH="$include_dir" \
	CFLAGS_x86_64_pc_windows_gnu="-I$include_dir" \
	CXXFLAGS_x86_64_pc_windows_gnu="-I$include_dir" \
	AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
	cargo check --target x86_64-pc-windows-gnu "$@"
