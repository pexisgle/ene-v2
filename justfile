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

# Re-run tests only when the behavior under test changes.
verify: fmt-check clippy

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-desktop -- --demo

fetch-assets asset="all":
    python3 scripts/fetch-assets.py "{{ asset }}"

run-avatar:
    cargo run --locked -p ene-avatar -- --standalone --model "${XDG_CACHE_HOME:-$HOME/.cache}/ene/models/Seed-san.vrm"
