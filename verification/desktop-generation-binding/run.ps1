# Run the desktop generation-binding check.
#
# Exit 0 means: the exhaustive guard test passes and the source still matches
# what the test was written against (see coverage.md).
#
# Usage: pwsh -File verification/desktop-generation-binding/run.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

Write-Host '== exhaustive guard test =='
Push-Location $root
try {
    $output = & cargo test -p ene-desktop --lib only_the_claim_naming_the_current_binding_executes 2>&1
    $output | Select-String -Pattern 'running|test result|panicked|FAILED' | ForEach-Object { $_.Line }
    $testExit = $LASTEXITCODE
    # A filter that matches nothing also exits 0, so require the test to have run.
    # Join first: -match on an array tests each element and returns the misses.
    if (($output -join "`n") -notmatch 'test result: ok\. 1 passed') {
        Write-Host 'FAIL the guarded test did not run; the filter matched no test or more than one'
        $testExit = 1
    }
} finally {
    Pop-Location
}

Write-Host '== sync check =='
$uv = (Get-Command uv -ErrorAction SilentlyContinue).Source
if (-not $uv) {
    $uv = (Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\astral-sh.uv_*\uv.exe" -ErrorAction SilentlyContinue |
        Select-Object -First 1).FullName
}
if (-not $uv) { throw 'uv is required: it runs check.py without installing anything' }
& $uv run --no-project python (Join-Path $PSScriptRoot 'check.py')
$syncExit = $LASTEXITCODE

if ($testExit -eq 0 -and $syncExit -eq 0) {
    Write-Host 'RESULT pass: the guard is checked in full and the source matches coverage.md'
    exit 0
}
Write-Host 'RESULT fail: read verification/desktop-generation-binding/coverage.md before changing anything'
exit 1
