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

verify: fmt-check clippy test build

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-desktop -- --demo

fetch-avatar:
    python3 scripts/fetch-seed.py

run-avatar:
    cargo run --locked -p ene-avatar -- --standalone --model "${XDG_CACHE_HOME:-$HOME/.cache}/ene/models/Seed-san.vrm"
