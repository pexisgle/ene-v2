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

verify: fmt-check check clippy test build

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo run --locked -p ene-desktop
