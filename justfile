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

fetch-assets asset="all":
    python3 scripts/fetch-assets.py "{{asset}}"

# Re-run tests only when the behavior under test changes.
verify: fmt-check clippy

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo run --locked -p ene-desktop

# Control the real renderer over IPC without the desktop UI.
avatar-console:
    cargo build --locked -p ene-avatar --bin ene-avatar --example renderer-console
    cargo run --locked -p ene-avatar --example renderer-console
