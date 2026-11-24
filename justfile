[windows]
set shell := ["powershell.exe", "-NoLogo", "-Command"]

# Cargo 1.100's per-crate build-dir layout expands large crates' rustc arguments
# past Windows' 32 KiB CreateProcess limit; sccache flattens Cargo's response
# file on `dep-info` probes and fails with `os error 206`.
cargo_wrapper := if os_family() == "windows" { "--config \"build.rustc-wrapper=''\"" } else { "" }

default:
    @just --list

[windows]
observe:
    if (Get-Command uv -ErrorAction SilentlyContinue) { uv run python scripts/observe.py } elseif (Get-Command py -ErrorAction SilentlyContinue) { py -3 scripts/observe.py } else { python scripts/observe.py }

[unix]
observe:
    python3 scripts/observe.py

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

[windows]
check-arch:
    if (Get-Command uv -ErrorAction SilentlyContinue) { uv run python scripts/check-architecture.py } elseif (Get-Command py -ErrorAction SilentlyContinue) { py -3 scripts/check-architecture.py } else { python scripts/check-architecture.py }

[unix]
check-arch:
    python3 scripts/check-architecture.py

check:
    cargo check {{cargo_wrapper}} --workspace --all-targets --all-features --locked

clippy:
    cargo clippy {{cargo_wrapper}} --workspace --all-targets --all-features --locked -- -D warnings

test: build-test-plugins
    cargo test {{cargo_wrapper}} --workspace --all-features --locked

# Proves the last commit builds from a fresh checkout.
committed-builds:
    bash verification/committed-tree-builds/run.sh

build:
    cargo build {{cargo_wrapper}} --workspace --all-targets --all-features --locked

[windows]
installer-windows:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build-windows-installer.ps1

[windows]
fetch-assets asset="all":
    if (Get-Command uv -ErrorAction SilentlyContinue) { uv run python scripts/fetch-assets.py "{{asset}}" } elseif (Get-Command py -ErrorAction SilentlyContinue) { py -3 scripts/fetch-assets.py "{{asset}}" } else { python scripts/fetch-assets.py "{{asset}}" }

[unix]
fetch-assets asset="all":
    python3 scripts/fetch-assets.py "{{asset}}"

# Re-run tests only when the behavior under test changes.
verify: fmt-check check-arch clippy

build-test-plugins:
    cargo build {{cargo_wrapper}} --release --locked -p ene-reference-wasm -p ene-openrouter-wasm --target wasm32-unknown-unknown

# The build scratch lives in target/tmp: sccache puts its per-compile directory
# under the system temp, and when Windows cleanup removes one mid-build rustc
# fails to write its dep-info. A workspace path is not cleaned by anything.

# Start the host and desktop client together for local interactive testing.
[unix]
run: avatar
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/tmp
    export TMPDIR="$PWD/target/tmp"
    state_dir="${ENE_HOST_STATE_DIR:-$PWD/target/run/host/state}"
    runtime_dir="${ENE_HOST_RUNTIME_DIR:-$PWD/target/run/host/runtime}"
    connection="${ENE_HOST_CONNECTION:-$runtime_dir/connection.json}"
    renderer="${ENE_DESKTOP_RENDERER:-$PWD/target/release/ene-avatar}"
    allowed_caps="${ENE_HOST_ALLOWED_CAPABILITIES:-*}"

    cargo build --locked -p ene-host -p ene-desktop

    host_pid=""
    desktop_pid=""
    marker=$(mktemp "$TMPDIR/ene-run.XXXXXX")
    cleanup() {
      local status=$?
      trap - EXIT
      trap '' INT TERM
      rm -f "$marker"
      if [ -n "$desktop_pid" ] && kill -0 "$desktop_pid" 2>/dev/null; then
        kill -TERM "$desktop_pid" 2>/dev/null || true
        wait "$desktop_pid" 2>/dev/null || true
      fi
      if [ -n "$host_pid" ] && kill -0 "$host_pid" 2>/dev/null; then
        kill -TERM "$host_pid" 2>/dev/null || true
        wait "$host_pid" 2>/dev/null || true
      fi
      exit "$status"
    }
    trap cleanup EXIT
    trap 'exit 0' INT TERM

    ENE_HOST_STATE_DIR="$state_dir" \
    ENE_HOST_RUNTIME_DIR="$runtime_dir" \
    ENE_HOST_ALLOWED_CAPABILITIES="$allowed_caps" \
      "$PWD/target/debug/ene-host" &
    host_pid=$!

    deadline=$((SECONDS + 30))
    while [ "$SECONDS" -lt "$deadline" ]; do
      if ! kill -0 "$host_pid" 2>/dev/null; then
        wait "$host_pid" || exit $?
        echo "just run: ene-host exited before publishing $connection" >&2
        exit 1
      fi
      if [ -s "$connection" ] && ! [ "$marker" -nt "$connection" ]; then
        break
      fi
      sleep 0.05
    done
    if [ ! -s "$connection" ] || [ "$marker" -nt "$connection" ]; then
      echo "just run: ene-host did not publish $connection within 30s" >&2
      exit 1
    fi

    ENE_HOST_CONNECTION="$connection" \
    ENE_DESKTOP_RENDERER="$renderer" \
      "$PWD/target/debug/ene-desktop" &
    desktop_pid=$!
    wait "$desktop_pid"

[windows]
run: avatar
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; $state = if ($env:ENE_HOST_STATE_DIR) { $env:ENE_HOST_STATE_DIR } else { Join-Path $PWD 'target\run\host\state' }; $runtime = if ($env:ENE_HOST_RUNTIME_DIR) { $env:ENE_HOST_RUNTIME_DIR } else { Join-Path $PWD 'target\run\host\runtime' }; $conn = if ($env:ENE_HOST_CONNECTION) { $env:ENE_HOST_CONNECTION } else { Join-Path $runtime 'connection.json' }; $renderer = if ($env:ENE_DESKTOP_RENDERER) { $env:ENE_DESKTOP_RENDERER } else { Join-Path $PWD 'target\release\ene-avatar.exe' }; $caps = if ($env:ENE_HOST_ALLOWED_CAPABILITIES) { $env:ENE_HOST_ALLOWED_CAPABILITIES } else { '*' }; cargo build {{cargo_wrapper}} --locked -p ene-host -p ene-desktop; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:ENE_HOST_STATE_DIR = $state; $env:ENE_HOST_RUNTIME_DIR = $runtime; $env:ENE_HOST_ALLOWED_CAPABILITIES = $caps; $marker = Join-Path $env:TMP ("ene-run-{0}.tmp" -f [guid]::NewGuid().ToString('N')); New-Item -ItemType File -Force $marker | Out-Null; $markerTime = (Get-Item $marker).LastWriteTimeUtc; $hostProc = $null; $desktopProc = $null; try { $hostProc = Start-Process -FilePath (Join-Path $PWD 'target\debug\ene-host.exe') -NoNewWindow -PassThru; $null = $hostProc.Handle; $deadline = [DateTime]::UtcNow.AddSeconds(30); while ([DateTime]::UtcNow -lt $deadline) { if ($hostProc.HasExited) { $hostProc.WaitForExit(); $code = if ($null -ne $hostProc.ExitCode) { $hostProc.ExitCode } else { 1 }; Write-Error ("just run: ene-host exited early with code {0} (0x{0:x8})" -f $code); exit $code }; if ((Test-Path $conn) -and ((Get-Item $conn).Length -gt 0) -and ((Get-Item $conn).LastWriteTimeUtc -ge $markerTime)) { break }; Start-Sleep -Milliseconds 50 }; if (-not ((Test-Path $conn) -and ((Get-Item $conn).Length -gt 0) -and ((Get-Item $conn).LastWriteTimeUtc -ge $markerTime))) { Write-Error "just run: ene-host did not publish $conn within 30s"; exit 1 }; $env:ENE_HOST_CONNECTION = $conn; $env:ENE_DESKTOP_RENDERER = $renderer; $desktopProc = Start-Process -FilePath (Join-Path $PWD 'target\debug\ene-desktop.exe') -PassThru; $null = $desktopProc.Handle; while (-not $desktopProc.HasExited) { if ($hostProc.HasExited) { $hostProc.WaitForExit(); $code = if ($null -ne $hostProc.ExitCode) { $hostProc.ExitCode } else { 1 }; Write-Error ("just run: ene-host exited while desktop was running (code {0}, 0x{0:x8})" -f $code); exit $code }; Start-Sleep -Milliseconds 100 }; $desktopProc.WaitForExit(); $code = if ($null -ne $desktopProc.ExitCode) { $desktopProc.ExitCode } else { 0 }; exit $code } finally { if ($marker) { Remove-Item -Force -ErrorAction SilentlyContinue $marker }; if ($desktopProc -and -not $desktopProc.HasExited) { Stop-Process -Id $desktopProc.Id -Force -ErrorAction SilentlyContinue; $desktopProc.WaitForExit() }; if ($hostProc -and -not $hostProc.HasExited) { Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue; $hostProc.WaitForExit() } }

[unix]
run-host:
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" ENE_HOST_STATE_DIR="$PWD/target/run/host/state" ENE_HOST_RUNTIME_DIR="$PWD/target/run/host/runtime" cargo run --locked -p ene-host

[windows]
run-host:
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; $env:ENE_HOST_STATE_DIR = (Join-Path $PWD 'target\run\host\state'); $env:ENE_HOST_RUNTIME_DIR = (Join-Path $PWD 'target\run\host\runtime'); cargo run {{cargo_wrapper}} --locked -p ene-host

# The renderer the client spawns; release, because a debug Bevy build cannot hold a frame rate.
[unix]
avatar:
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" cargo build --release --locked -p ene-avatar --bin ene-avatar

[windows]
avatar:
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; cargo build {{cargo_wrapper}} --release --locked -p ene-avatar --bin ene-avatar

# Connect to a running host as the desktop client.
[unix]
run-desktop: avatar
    mkdir -p target/tmp && TMPDIR="$PWD/target/tmp" ENE_HOST_CONNECTION="$PWD/target/run/host/runtime/connection.json" ENE_DESKTOP_RENDERER="$PWD/target/release/ene-avatar" cargo run --locked -p ene-desktop

[windows]
run-desktop: avatar
    $env:TMP = (Join-Path $PWD 'target\tmp'); $env:TEMP = $env:TMP; New-Item -ItemType Directory -Force $env:TMP | Out-Null; $env:ENE_HOST_CONNECTION = (Join-Path $PWD 'target\run\host\runtime\connection.json'); $env:ENE_DESKTOP_RENDERER = (Join-Path $PWD 'target\release\ene-avatar.exe'); cargo run {{cargo_wrapper}} --locked -p ene-desktop

# Control the transparent renderer using JSON Lines on stdin.
run-avatar:
    cargo run {{cargo_wrapper}} --release --locked -p ene-avatar

# Build the renderer and launch its interactive CLI example.
run-avatar-cli:
    cargo build {{cargo_wrapper}} --release --locked -p ene-avatar --bin ene-avatar
    cargo run {{cargo_wrapper}} --release --locked -p ene-avatar --example control
