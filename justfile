set windows-shell := ["cmd.exe", "/c"]

default:
    @just --list

fmt:
    cargo fmt --all

verify:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

build:
    cargo build --workspace --locked

[windows]
installer-windows:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File packaging/windows/build-windows-installer.ps1

run:
    cargo build --locked -p ene-host -p ene-avatar
    cargo run --locked -p ene-desktop

run-host:
    cargo run --locked -p ene-host

run-desktop:
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-desktop

run-avatar:
    cargo run --release --locked -p ene-avatar

[windows]
run-avatar:
    set "WGPU_DX12_PRESENTATION_SYSTEM=Visual" && cargo run --release --locked -p ene-avatar
