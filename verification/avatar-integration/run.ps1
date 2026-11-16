# Drive the avatar renderer over its JSON Lines protocol and check the
# integration the desktop client depends on: an expression reaches the morphs,
# lip_sync moves the smoothed mouth, a motion change fades instead of reloading,
# and no motion means the rest pose.
#
# Exit 0 means every check passed. 1 means a check failed. It needs a display
# (the renderer opens a window) and a VRM/VRMA pair on this machine.
#
# Usage:
#   pwsh -File verification/avatar-integration/run.ps1
#   $env:ENE_VERIFY_MODEL = 'C:\path\to\model.vrm'   # optional override
#
# See coverage.md for what this covers and what it does not.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$failures = New-Object System.Collections.ArrayList

function Check([string]$name, [bool]$ok, [string]$detail) {
    if ($ok) {
        Write-Host "PASS $name"
    } else {
        Write-Host "FAIL $name ($detail)"
        [void]$failures.Add($name)
    }
}

function Resolve-Asset([string]$envName, [string]$fallback) {
    $override = [Environment]::GetEnvironmentVariable($envName)
    if ($override) { return $override }
    return $fallback
}

$model = Resolve-Asset 'ENE_VERIFY_MODEL' "$env:LOCALAPPDATA\ene\cache\models\Seed-san.vrm"
$motion = Resolve-Asset 'ENE_VERIFY_MOTION' "$env:LOCALAPPDATA\ene\cache\motions\three-vrm-sample.vrma"
$motion2 = Resolve-Asset 'ENE_VERIFY_MOTION2' "$env:LOCALAPPDATA\ene\cache\motions\quaternius-dance.vrma"
foreach ($path in @($model, $motion, $motion2)) {
    if (-not (Test-Path -LiteralPath $path)) {
        Write-Host "SKIP the renderer asset is missing: $path"
        exit 1
    }
}

Write-Host '== build =='
Push-Location $root
try {
    $build = & cargo build --locked -p ene-avatar --bin ene-avatar 2>&1
    if ($LASTEXITCODE -ne 0) {
        $build | Select-Object -Last 20 | ForEach-Object { Write-Host $_ }
        Write-Host 'FAIL the renderer did not build'
        exit 1
    }
} finally {
    Pop-Location
}

$exe = Join-Path $root 'target\debug\ene-avatar.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "FAIL the renderer binary is missing: $exe"
    exit 1
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.WorkingDirectory = $root
$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $psi
$proc.Start() | Out-Null
$stdout = $proc.StandardOutput
$stderr = $proc.StandardError
$script:outTask = $stdout.ReadLineAsync()
$script:errTask = $stderr.ReadLineAsync()
$lines = New-Object System.Collections.ArrayList

# Only one read may be outstanding per stream, so each completed read is
# replaced by exactly one new one, and the state lives in script scope.
function Pump([int]$milliseconds) {
    $deadline = (Get-Date).AddMilliseconds($milliseconds)
    while ((Get-Date) -lt $deadline) {
        if ($script:outTask.IsCompleted) {
            $line = $script:outTask.Result
            if ($null -ne $line) { [void]$lines.Add($line) }
            $script:outTask = $stdout.ReadLineAsync()
        }
        if ($script:errTask.IsCompleted) {
            $line = $script:errTask.Result
            if ($null -ne $line -and $line -notmatch 'bevy_') { Write-Host "log: $line" }
            $script:errTask = $stderr.ReadLineAsync()
        }
        Start-Sleep -Milliseconds 40
    }
}

function Send($message) {
    $line = $message | ConvertTo-Json -Compress -Depth 6
    $proc.StandardInput.WriteLine($line)
    $proc.StandardInput.Flush()
}

function Call($message, [int]$timeoutMs = 3000) {
    Send $message
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    $id = $message.id
    while ((Get-Date) -lt $deadline) {
        Pump 60
        for ($i = 0; $i -lt $lines.Count; $i++) {
            try { $reply = $lines[$i] | ConvertFrom-Json } catch { continue }
            if ($reply.id -eq $id) {
                $lines.RemoveAt($i)
                return $reply
            }
        }
    }
    return $null
}

function Actor([int]$timeoutMs = 3000) {
    $reply = Call @{ id = $script:nextId; op = 'get' } $timeoutMs
    if (-not $reply) { return $null }
    return $reply.ok.characters[0]
}

$script:nextId = 1
try {
    Write-Host '== renderer contract =='
    Send @{ id = 1; op = 'add'; character = 'verify'; config = @{ model = $model; motion = $motion } }
    $script:nextId = 2
    Pump 8000
    $actor = Actor
    if (-not $actor) { Write-Host 'FAIL the renderer never answered get'; exit 1 }
    Check 'the model loads' ($actor.status -eq 'ready') "status=$($actor.status) error=$($actor.error)"
    $initialStates = @($lines | Where-Object { $_ -match '"event":"state"' }).Count
    [void]$lines.Clear()

    $set = Call @{ id = $script:nextId; op = 'expression'; character = 'verify'; name = 'happy' }
    $script:nextId++
    Pump 600
    $actor = Actor
    Check 'expression is accepted' ($set -and -not $set.error) "reply=$($set | ConvertTo-Json -Compress)"
    Check 'expression is reported' ($actor.expression -eq 'happy') "expression=$($actor.expression)"
    Check 'the expression fades in' ([double]$actor.expression_weight -gt 0.1) "weight=$($actor.expression_weight)"

    Send @{ op = 'lip_sync'; character = 'verify'; rms = 0.9 }
    Pump 900
    $actor = Actor
    Check 'lip_sync reaches the mouth' ([double]$actor.lip -gt 0.89 -and [double]$actor.lip_level -gt 0.5) "lip=$($actor.lip) level=$($actor.lip_level)"

    Send @{ op = 'lip_sync'; character = 'verify'; rms = 0 }
    Pump 900
    $actor = Actor
    Check 'silence closes the mouth' ([double]$actor.lip_level -lt 0.1) "level=$($actor.lip_level)"

    [void]$lines.Clear()
    $set = Call @{ id = $script:nextId; op = 'set'; character = 'verify'; config = @{ model = $model; motion = $motion2 } }
    $script:nextId++
    # The new clip loads asynchronously, so the fade starts whenever the asset
    # arrives. Poll for the overlap instead of assuming a wall-clock moment.
    $during = 0
    $deadline = (Get-Date).AddSeconds(8)
    while ((Get-Date) -lt $deadline) {
        $actor = Actor 500
        $during = @($actor.animation_times).Count
        if ($during -ge 2) { break }
    }
    $states = @($lines | Where-Object { $_ -match '"event":"state"' }).Count
    Check 'a motion change is applied' ($actor.config.motion -eq $motion2) "motion=$($actor.config.motion)"
    Check 'a motion change does not reload the model' ($states -eq 0) "state events=$states"
    Check 'a motion change fades' ($during -ge 2) "two clips were never active at once (max=$during)"

    $after = 2
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline) {
        $actor = Actor 500
        $after = @($actor.animation_times).Count
        if ($after -le 1) { break }
    }
    Check 'the fade settles on one clip' ($after -eq 1) "active=$after"

    $set = Call @{ id = $script:nextId; op = 'set'; character = 'verify'; config = @{ model = $model } }
    $script:nextId++
    Pump 900
    $actor = Actor
    Check 'no motion returns to rest' (@($actor.animation_times).Count -eq 0 -and $actor.status -eq 'ready') "times=$(@($actor.animation_times) -join ',') status=$($actor.status)"

    # The client builds these two lines, and their shape is what the renderer
    # above accepts. Require both named tests, not just a filtered count.
    Push-Location $root
    try {
        $wire = & cargo test --locked -p ene-desktop --lib avatar::tests 2>&1
        $wireText = ($wire -join "`n")
        $both = $wireText -match 'avatar::tests::a_face_request_can_be_set_and_cleared \.\.\. ok' -and
            $wireText -match 'avatar::tests::the_lip_line_is_one_way_and_clamped \.\.\. ok'
        Check 'the client wire format tests pass' ($LASTEXITCODE -eq 0 -and $both) 'cargo test -p ene-desktop --lib avatar::tests'

        # The client validates a body.motion input and decides what to send.
        $plan = & cargo test --locked -p ene-desktop --lib motion_plan 2>&1
        $planText = ($plan -join "`n")
        $planOk = $planText -match 'test result: ok\. 2 passed' -and
            $planText -match 'a_motion_plan_carries_the_face_with_the_motion \.\.\. ok' -and
            $planText -match 'a_motion_plan_rejects_what_this_client_never_declared \.\.\. ok'
        Check 'the client motion plan tests pass' ($LASTEXITCODE -eq 0 -and $planOk) 'cargo test -p ene-desktop --lib motion_plan'
    } finally {
        Pop-Location
    }
} finally {
    try {
        Send @{ id = 999; op = 'quit' }
        $proc.StandardInput.Flush()
        Pump 1200
    } catch { }
    if (-not $proc.HasExited) { $proc.Kill() }
}

if ($failures.Count -gt 0) {
    Write-Host "RESULT fail: $($failures.Count) check(s) failed"
    exit 1
}
Write-Host 'RESULT pass: the renderer contract the desktop client relies on holds'
exit 0
