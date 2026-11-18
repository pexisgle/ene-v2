# Prove the guarded check can fail.
#
# Applies one mutation to the real guard, runs the exhaustive test, and requires
# it to fail. The source is restored afterwards, byte for byte, and the test is
# rerun green. Exit 0 means the mutation was caught and the source is back.
#
# Usage: pwsh -File verification/desktop-generation-binding/break-check.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$source = Join-Path $root 'apps/ene-desktop/src/session.rs'
$backup = Join-Path ([System.IO.Path]::GetTempPath()) 'session.rs.guard-backup'

$original = [System.IO.File]::ReadAllText($source)
Copy-Item $source $backup -Force
Write-Host "backed up to $backup"

$guard = '    if claim.client_id != our_id {'
$weakened = '    if false && claim.client_id != our_id {'
if (-not $original.Contains($guard)) {
    throw 'the guard this check mutates is not written the way the script expects; read admits first'
}

$failed = $false
try {
    [System.IO.File]::WriteAllText($source, $original.Replace($guard, $weakened), (New-Object System.Text.UTF8Encoding($false)))
    Write-Host '== mutation applied: the identity arm accepts any client id =='
    Push-Location $root
    try {
        $output = & cargo test -p ene-desktop --lib only_the_claim_naming_the_current_binding_executes 2>&1
        $joined = $output -join "`n"
        $output | Select-String -Pattern 'panicked|assertion|left:|right:|test result' | ForEach-Object { $_.Line }
        if ($joined -match 'test result: FAILED') {
            Write-Host 'PASS the mutation is caught by the exhaustive check'
            $failed = $true
        } else {
            Write-Host 'FAIL the mutation was not caught: the check does not guard this arm'
        }
    } finally {
        Pop-Location
    }
} finally {
    [System.IO.File]::WriteAllText($source, $original, (New-Object System.Text.UTF8Encoding($false)))
    $restored = [System.IO.File]::ReadAllText($source)
    if ($restored -ne $original) { throw 'the source was not restored; recover it from the backup before doing anything else' }
    Write-Host 'source restored'
}

Push-Location $root
try {
    $green = & cargo test -p ene-desktop --lib only_the_claim_naming_the_current_binding_executes 2>&1
    $passed = ($green -join "`n") -match 'test result: ok\. 1 passed'
    Write-Host $(if ($passed) { 'PASS the check is green again' } else { 'FAIL the check is not green after restore' })
} finally {
    Pop-Location
}

if ($failed -and $passed) { exit 0 }
exit 1
