[windows]
set shell := ["powershell.exe", "-NoLogo", "-Command"]

default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

check:
    cargo check --workspace --all-targets --all-features --locked

clippy:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test: build-test-plugins
    cargo test --workspace --all-features --locked

build:
    cargo build --workspace --all-targets --all-features --locked

[windows]
fetch-assets asset="all":
    if (Get-Command uv -ErrorAction SilentlyContinue) { uv run python scripts/fetch-assets.py "{{asset}}" } elseif (Get-Command py -ErrorAction SilentlyContinue) { py -3 scripts/fetch-assets.py "{{asset}}" } else { python scripts/fetch-assets.py "{{asset}}" }

[unix]
fetch-assets asset="all":
    python3 scripts/fetch-assets.py "{{asset}}"

# Re-run tests only when the behavior under test changes.
verify: fmt-check clippy

build-test-plugins:
    cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown

# The build scratch lives in target/tmp: sccache puts its per-compile directory
# under the system temp, and when Windows cleanup removes one mid-build rustc
# fails to write its dep-info. A workspace path is not cleaned by anything.
[unix]
run-host:
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" ENE_HOST_STATE_DIR="$PWD/target/run/host/state" ENE_HOST_RUNTIME_DIR="$PWD/target/run/host/runtime" cargo run --locked -p ene-host

[windows]
run-host:
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; $env:ENE_HOST_STATE_DIR = (Join-Path $PWD 'target\run\host\state'); $env:ENE_HOST_RUNTIME_DIR = (Join-Path $PWD 'target\run\host\runtime'); cargo run --locked -p ene-host

# The renderer the client spawns; release, because a debug Bevy build cannot hold a frame rate.
[unix]
avatar:
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" cargo build --release --locked -p ene-avatar --bin ene-avatar

[windows]
avatar:
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; cargo build --release --locked -p ene-avatar --bin ene-avatar

# Connect to a running host as the desktop client.
[unix]
run-desktop: avatar
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" ENE_HOST_CONNECTION="$PWD/target/run/host/runtime/connection.json" ENE_DESKTOP_RENDERER="$PWD/target/release/ene-avatar" cargo run --locked -p ene-desktop

[windows]
run-desktop: avatar
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; $env:ENE_HOST_CONNECTION = (Join-Path $PWD 'target\run\host\runtime\connection.json'); $env:ENE_DESKTOP_RENDERER = (Join-Path $PWD 'target\release\ene-avatar.exe'); cargo run --locked -p ene-desktop

# Control the transparent renderer using JSON Lines on stdin.
run-avatar:
    cargo run --release --locked -p ene-avatar

# Build the renderer and launch its interactive CLI example.
run-avatar-cli:
    cargo build --release --locked -p ene-avatar --bin ene-avatar
    cargo run --release --locked -p ene-avatar --example control
