#!/usr/bin/env pwsh
# Run the full per-PR verification gate from docs/refactoring/ene_v2_refactoring_plan.md §9.
# Usage: scripts/verify-all.ps1 [-SkipTests] [-SkipClippy] [-SkipDeny]
param(
    [switch]$SkipTests,
    [switch]$SkipClippy,
    [switch]$SkipDeny,
    [switch]$SkipWasm
)

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# `python3` is a Microsoft Store stub on this host; use the bundled interpreter.
$py = "C:\Users\pexisgle\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe"
$env:PATH = "C:\tools\buf;" + $env:PATH

# sccache puts its per-compile scratch under the system temp; Windows cleanup can
# remove one mid-build and rustc then fails with LNK1104. Keep it inside the
# workspace, where nothing cleans it (matches the justfile's build-test-plugins note).
$buildTmp = Join-Path $repo ".cargo-tmp"
New-Item -ItemType Directory -Force -Path $buildTmp | Out-Null

# Test binaries must be staged outside the workspace: the sandbox denies the
# certificate-store open to processes rooted under it (see the test gate below).
# Capture the real system temp before redirecting TMP/TEMP for sccache scratch,
# because GetTempPath() itself reads those variables.
$stagingRoot = if ($env:TEMP) { $env:TEMP } else { [System.IO.Path]::GetTempPath() }
$env:TMP = $buildTmp
$env:TEMP = $buildTmp

$failures = [System.Collections.Generic.List[string]]::new()
$results  = [System.Collections.Generic.List[object]]::new()

function Invoke-Gate {
    param([string]$Name, [scriptblock]$Body)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Write-Host ""
    Write-Host "=== $Name ===" -ForegroundColor Cyan
    & $Body 2>&1 | Tee-Object -Variable out | Select-Object -Last 40 | ForEach-Object { Write-Host $_ }
    $code = $LASTEXITCODE
    $sw.Stop()
    $ok = ($code -eq 0)
    if (-not $ok) { $failures.Add("$Name (exit $code)") }
    $results.Add([pscustomobject]@{
        Gate = $Name; Exit = $code; Ok = $ok
        Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    })
    Write-Host ("--- {0}: {1} ({2}s)" -f $Name, $(if ($ok) { "PASS" } else { "FAIL exit=$code" }), [math]::Round($sw.Elapsed.TotalSeconds,1)) `
        -ForegroundColor $(if ($ok) { "Green" } else { "Red" })
}

Invoke-Gate "cargo fmt --check" { cargo fmt --all -- --check }
Invoke-Gate "architecture self-test" { & $py scripts/check-architecture.py --self-test }
Invoke-Gate "architecture check" { & $py scripts/check-architecture.py }

if (-not $SkipDeny) {
    Invoke-Gate "cargo deny" { cargo deny check }
    Invoke-Gate "cargo machete" { cargo machete --with-metadata }
}

# These three lints are declared `warn` in `[workspace.lints.clippy]` so every
# normal build already reports them. They are exempted from `-D warnings` only
# because their clean-up is scheduled later by the refactoring plan:
#   undocumented_unsafe_blocks / multiple_unsafe_ops_per_block -> PR-4.3
#   let_underscore_must_use                                   -> PR-5.1
# Each PR drops its own `-A` here (and flips the lint to `deny`) when the last
# site is fixed. Never add a new exemption to this list.
$stagedLints = @(
    "-A", "clippy::undocumented_unsafe_blocks",
    "-A", "clippy::multiple_unsafe_ops_per_block",
    "-A", "clippy::let_underscore_must_use"
)

if (-not $SkipWasm) {
    Invoke-Gate "clippy wasm targets" {
        cargo clippy --profile ci --locked -p ene-plugin -p ene-openrouter-wasm -p ene-reference-wasm `
            --target wasm32-unknown-unknown -- -D warnings @stagedLints
    }
}

if (-not $SkipClippy) {
    Invoke-Gate "clippy workspace" {
        cargo clippy --profile ci --workspace --all-targets --all-features --locked -- -D warnings @stagedLints
    }
}

if (-not $SkipTests) {
    # Two library tests construct a rustls client config, which calls
    # `rustls_native_certs::load_native_certs()`. On this host the DSH file sandbox
    # denies the Windows CurrentUser\ROOT store open to processes rooted under the
    # workspace, so those two fail with "os error 5"; the identical binaries pass
    # when run from %TEMP%. They are excluded here and re-run below from %TEMP% so
    # the coverage is real rather than skipped. Everything else runs normally,
    # which keeps `CARGO_BIN_EXE_*` and fixture paths valid for the integration
    # tests that spawn `ene-host`.
    $sandboxedLibTests = @(
        "f10_remote_host_target_enforces_https_and_roundtrips_config",
        "t20_remote_tls_endpoint_validation_enforces_https_and_rejects_plaintext"
    )
    $skipArgs = $sandboxedLibTests | ForEach-Object { "--skip"; $_ }

    Invoke-Gate "cargo test workspace" {
        cargo test --profile ci --workspace --all-features --locked -- @skipArgs
    }
    Invoke-Gate "cargo test (cert-store tests, run from %TEMP%)" {
        $stage = Join-Path $stagingRoot ("ene-tls-tests-" + [guid]::NewGuid())
        New-Item -ItemType Directory -Force -Path $stage | Out-Null
        $targets = @(
            @{ Crate = "ene-rpc"; Filter = "t20_remote_tls_endpoint_validation" },
            @{ Crate = "ene-desktop"; Filter = "host::tests::f10" }
        )
        $failed = @()
        Push-Location $stage
        try {
            foreach ($t in $targets) {
                $pattern = if ($t.Crate -eq "ene-rpc") { "ene_rpc-*.exe" } else { "ene_desktop-*.exe" }
                # Several builds share a crate name (lib, lib-test, bin). The test
                # binaries are the large ones (unit tests linked in); picking the
                # newest instead grabs whatever `cargo test --no-run` built last.
                $exe = Get-ChildItem (Join-Path $repo "target\ci") -Recurse -Filter $pattern -ErrorAction SilentlyContinue |
                    Where-Object { $_.DirectoryName -match '\\out$' -and $_.Length -gt 1MB } |
                    Sort-Object LastWriteTime -Descending | Select-Object -First 1
                if (-not $exe) {
                    Write-Host "missing test binary for $($t.Crate)" -ForegroundColor Red
                    $failed += $t.Crate
                    continue
                }
                $dest = Join-Path $stage $exe.Name
                Copy-Item $exe.FullName $dest -Force
                & $dest $t.Filter --test-threads=1
                if ($LASTEXITCODE -ne 0) { $failed += $t.Crate }
            }
        } finally {
            Pop-Location
            Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
        }
        if ($failed.Count -gt 0) {
            Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
            cmd /c exit 1
        } else {
            Write-Host "cert-store tests passed from %TEMP%" -ForegroundColor Green
            cmd /c exit 0
        }
    }
    Invoke-Gate "host plugin_tests (serial)" {
        cargo test --profile ci --locked -p ene-host --lib host::model::plugin_tests -- --test-threads=1
    }
}

Write-Host ""
Write-Host "================ GATE SUMMARY ================" -ForegroundColor Cyan
$results | Format-Table -AutoSize | Out-String | Write-Host
if ($failures.Count -gt 0) {
    Write-Host "FAILED: $($failures -join '; ')" -ForegroundColor Red
    exit 1
}
Write-Host "ALL GATES PASS" -ForegroundColor Green
exit 0
