<#
.SYNOPSIS
    Builds Passthrify and packages a ready-to-run folder in dist\Passthrify.

.EXAMPLE
    .\build.ps1                 # test + release build + dist\Passthrify
    .\build.ps1 -Zip            # also create dist\Passthrify-<version>.zip
    .\build.ps1 -Installer      # also create dist\Passthrify-<version>-setup.exe (needs Inno Setup 6)
    .\build.ps1 -SkipTests -Run # build, package, then launch it
    .\build.ps1 -Dev            # debug build only (fast, with console), no packaging
    .\build.ps1 -Clean          # cargo clean + remove dist first
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$Zip,
    [switch]$Installer,
    [switch]$Run,
    [switch]$Dev,
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
Set-Location $Root

$FfmpegDir = Join-Path $Root 'third_party\ffmpeg'
$FfmpegUrl = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip'
$LlvmBin = 'C:\Program Files\LLVM\bin'

function Write-Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Fail($msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; exit 1 }

function Invoke-Checked {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { Fail "$Exe $($Arguments -join ' ') failed (exit $LASTEXITCODE)" }
}

# --- Prerequisites ------------------------------------------------------------------------
Write-Step 'Checking prerequisites'

# Freshly installed tools may not be on this shell's PATH yet.
foreach ($dir in @("$env:USERPROFILE\.cargo\bin", $LlvmBin)) {
    if ((Test-Path $dir) -and ($env:Path -notlike "*$dir*")) { $env:Path = "$dir;$env:Path" }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail 'cargo not found. Install Rust: winget install Rustlang.Rustup'
}
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = $null
if (Test-Path $vswhere) {
    $vs = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
if (-not $vs) {
    Fail 'MSVC C++ build tools not found. Install: winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
}
if (-not (Test-Path (Join-Path $LlvmBin 'libclang.dll'))) {
    Fail "libclang not found in $LlvmBin. Install: winget install LLVM.LLVM"
}

if (-not (Test-Path (Join-Path $FfmpegDir 'bin\avcodec-*.dll'))) {
    Write-Step 'FFmpeg not found - downloading FFmpeg 8.1 LGPL shared build'
    $tmp = Join-Path $env:TEMP "passthrify-ffmpeg-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Force $tmp | Out-Null
    try {
        $zipPath = Join-Path $tmp 'ffmpeg.zip'
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $FfmpegUrl -OutFile $zipPath -UseBasicParsing
        Expand-Archive $zipPath -DestinationPath (Join-Path $tmp 'x') -Force
        $inner = Get-ChildItem (Join-Path $tmp 'x') -Directory | Select-Object -First 1
        New-Item -ItemType Directory -Force (Split-Path $FfmpegDir) | Out-Null
        if (Test-Path $FfmpegDir) { Remove-Item $FfmpegDir -Recurse -Force }
        Move-Item $inner.FullName $FfmpegDir
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$rustc = (& rustc --version)
Write-Host "  $rustc"
Write-Host "  MSVC:   $vs"
Write-Host "  FFmpeg: $FfmpegDir"

# --- Build --------------------------------------------------------------------------------
if ($Clean) {
    Write-Step 'Cleaning'
    Invoke-Checked cargo @('clean')
    if (Test-Path (Join-Path $Root 'dist')) { Remove-Item (Join-Path $Root 'dist') -Recurse -Force }
}

if (-not $SkipTests) {
    Write-Step 'Running tests'
    Invoke-Checked cargo @('test', '--quiet')
}

if ($Dev) {
    Write-Step 'Building (debug)'
    Invoke-Checked cargo @('build')
    $exe = Join-Path $Root 'target\debug\passthrify.exe'
    Write-Host "`nBuilt $exe" -ForegroundColor Green
    if ($Run) { Start-Process $exe }
    exit 0
}

Write-Step 'Building (release)'
Invoke-Checked cargo @('build', '--release')

# --- Package ------------------------------------------------------------------------------
Write-Step 'Packaging'
$version = (Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$dist = Join-Path $Root 'dist\Passthrify'
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Force $dist | Out-Null

Copy-Item (Join-Path $Root 'target\release\passthrify.exe') $dist
# Only the FFmpeg libraries the app links against (not ffmpeg.exe/ffplay.exe/ffprobe.exe).
Get-ChildItem (Join-Path $FfmpegDir 'bin') -Filter '*.dll' | Copy-Item -Destination $dist
Copy-Item (Join-Path $Root 'README.md') $dist
Copy-Item (Join-Path $FfmpegDir 'LICENSE.txt') (Join-Path $dist 'FFMPEG-LICENSE.txt') -ErrorAction SilentlyContinue
# Inter and JetBrains Mono are embedded in the exe; their OFL licenses must travel with it.
Get-ChildItem (Join-Path $Root 'assets\fonts') -Filter 'OFL-*.txt' | Copy-Item -Destination $dist

$sizeMb = [math]::Round(((Get-ChildItem $dist | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host "  dist\Passthrify  ($sizeMb MB, v$version)"

if ($Zip) {
    $zipOut = Join-Path $Root "dist\Passthrify-$version.zip"
    if (Test-Path $zipOut) { Remove-Item $zipOut -Force }
    Compress-Archive -Path $dist -DestinationPath $zipOut
    Write-Host "  $zipOut"
}

if ($Installer) {
    Write-Step 'Building installer'
    $iscc = (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source
    if (-not $iscc) {
        $iscc = @(
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
            "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
            "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
        ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $iscc) { Fail 'Inno Setup 6 not found. Install it (winget install JRSoftware.InnoSetup) or drop -Installer.' }
    # The exe's icon, rendered by build.rs, doubles as the setup's icon.
    $ico = Get-ChildItem (Join-Path $Root 'target\release\build') -Filter passthrify.ico -Recurse -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $numeric = ($version -split '-')[0] # file version resources take digits only
    $isccArgs = @("/DAppVersion=$version", "/DNumericVersion=$numeric", "/DSourceDir=$dist", "/DOutputDir=$(Join-Path $Root 'dist')", '/Q')
    if ($ico) { $isccArgs += "/DIconFile=$($ico.FullName)" }
    Invoke-Checked $iscc ($isccArgs + (Join-Path $Root 'installer\passthrify.iss'))
    Write-Host "  dist\Passthrify-$version-setup.exe"
}

Write-Host "`nDone: $dist\passthrify.exe" -ForegroundColor Green
if ($Run) { Start-Process (Join-Path $dist 'passthrify.exe') }
