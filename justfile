set default-list
set windows-shell := ["cmd.exe", "/c"]

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

check:
    cargo check --workspace --all-targets --all-features --locked

clippy:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

verify: fmt-check clippy

test: build-test-plugins
    cargo nextest run --workspace --all-features --locked

build-test-plugins:
    cargo build --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown

build:
    cargo build --workspace --all-targets --all-features --locked

[windows]
installer-windows:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File packaging/windows/build-windows-installer.ps1

run:
    cargo build --locked -p ene-host
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-desktop -- --local-host

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-desktop -- --connect-only

# Debug Bevy builds cannot hold a frame rate.
avatar:
    cargo build --release --locked -p ene-avatar

run-avatar:
    cargo run --release --locked -p ene-avatar

run-avatar-cli: avatar
    cargo run --release --locked -p ene-avatar --example control
