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

test:
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

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo run --locked -p ene-desktop

# Control the transparent renderer using JSON Lines on stdin.
run-avatar:
    cargo run --release --locked -p ene-avatar

# Build the renderer and launch its interactive CLI example.
run-avatar-cli:
    cargo build --release --locked -p ene-avatar --bin ene-avatar
    cargo run --release --locked -p ene-avatar --example control
