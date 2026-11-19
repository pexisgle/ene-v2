$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

$iscc = (Get-Command ISCC.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
if (-not $iscc) {
    $iscc = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
}
if (-not $iscc) {
    throw 'Install Inno Setup 6.3 or later and make ISCC.exe available on PATH.'
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$crt = & $vswhere -utf8 -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\msvcp140.dll' |
    Select-Object -First 1
if (-not $crt) {
    throw 'Install the x64 MSVC C++ build tools and Visual C++ Redistributable files through Visual Studio Installer.'
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    $metadataJson = cargo.exe metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE) { throw "cargo metadata failed ($LASTEXITCODE)." }
    $metadata = $metadataJson | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'ene-desktop').version
    $winTarget = 'x86_64-pc-windows-msvc'
    $buildDir = Join-Path $metadata.target_directory "$winTarget\release"
    $env:TMP = (New-Item -ItemType Directory -Force 'target/tmp').FullName
    $env:TEMP = $env:TMP

    cargo.exe build --config "build.rustc-wrapper=''" --release --locked --target $winTarget -p ene-host -p ene-desktop -p ene-avatar
    if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)." }

    & $iscc "/DAppVersion=$version" "/DBuildDir=$buildDir" "/DCrtDir=$(Split-Path -Parent $crt)" "$PSScriptRoot\ene-windows.iss"
    if ($LASTEXITCODE) { throw "Inno Setup compilation failed ($LASTEXITCODE)." }
} finally {
    Pop-Location
}
