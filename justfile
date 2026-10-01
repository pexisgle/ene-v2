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

# Run separately in a Wayland session; does not substitute for native IME checks.
test-desktop-wayland:
    cargo test --locked -p ene-desktop wayland_rendering_and_input -- --ignored --nocapture --test-threads=1

# Keep the integration watcher separate from the desktop session's real tray.
test-desktop-tray:
    dbus-run-session -- env ENE_TRAY_TEST_PRIVATE_BUS=1 cargo test --locked -p ene-desktop wayland_tray_lifecycle -- --ignored --nocapture --test-threads=1

fetch-avatar:
    python3 scripts/fetch-seed.py

run-avatar:
    cargo run --locked -p ene-avatar -- --standalone --model "${XDG_CACHE_HOME:-$HOME/.cache}/ene/models/Seed-san.vrm"

# Pointer test refuses physical outputs and requires an explicitly isolated Sway.
test-avatar-wayland:
    cargo build --locked -p ene-avatar --example test_pointer
    cargo build --locked -p ene-avatar
    cargo run --locked -p ene-avatar -- --standalone --model "${XDG_CACHE_HOME:-$HOME/.cache}/ene/models/Seed-san.vrm" --frames 150 --snapshot target/avatar-seed.png
    python3 scripts/test-avatar-wayland.py

# Uses real client processes and a private watcher; never the desktop's real tray.
test-avatar-client:
    cargo build --locked -p ene-avatar --example test_pointer
    cargo build --locked -p ene-desktop --example test_tray
    cargo build --locked -p ene-desktop -p ene-avatar
    dbus-run-session -- env ENE_TRAY_TEST_PRIVATE_BUS=1 python3 scripts/test-avatar-client.py

measure-avatar:
    cargo build --release --locked -p ene-avatar
    python3 scripts/measure-avatar.py

# Real retarget/placement and silhouette tests; requires the private Sway setup.
test-vrma:
    python3 scripts/make-vrma-test-fixture.py
    cargo test --locked -p ene-avatar vrma_retarget_stays_stationary -- --ignored --nocapture --test-threads=1
    cargo build --locked -p ene-avatar --example test_pointer
    cargo build --locked -p ene-avatar
    python3 scripts/test-vrma-wayland.py
