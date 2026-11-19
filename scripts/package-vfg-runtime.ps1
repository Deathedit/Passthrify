<#
.SYNOPSIS
    Packages NVIDIA's Video Frame Generation runtime into the optional download Passthrify
    offers in Settings › Display (dist\Passthrify-VFG-runtime-<version>.zip + .sha256).

.DESCRIPTION
    Point -SdkDir at the bin folder of YOUR OWN NVIDIA Video Effects SDK install, downloaded
    from NVIDIA NGC with the Video Frame Generation feature added (install_feature.ps1). The
    right to redistribute these files comes from accepting NVIDIA's Maxine SDK license
    yourself; don't package files copied out of another application.

    License conditions to keep (Maxine SDK license, v. April 2021): the files are used only by
    Passthrify, the app credits NVIDIA (Settings shows "powered by NVIDIA Maxine"), and the
    runtime isn't put under an open-source license. The zip carries NVIDIA's license notice.

.EXAMPLE
    .\scripts\package-vfg-runtime.ps1 -SdkDir 'C:\NVIDIA\VFX_SDK\bin'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SdkDir,
    [string]$OutDir = (Join-Path $PSScriptRoot '..\dist')
)

$ErrorActionPreference = 'Stop'
$SdkDir = (Resolve-Path $SdkDir).Path

# What Passthrify loads (src\vfg\sdk.rs) plus the libraries NVVideoEffects.dll pulls in.
$required = 'NVVideoEffects.dll', 'NVCVImage.dll', 'nvVFXVideoFrameGeneration.dll'
foreach ($f in $required) {
    if (-not (Test-Path (Join-Path $SdkDir $f))) { throw "$f not found in $SdkDir" }
}
$cudart = Get-ChildItem $SdkDir -Filter 'cudart64_*.dll' | Select-Object -First 1
if (-not $cudart) { throw "cudart64_*.dll not found in $SdkDir (needed for fast GPU-side conversion)" }
$files = @($required | ForEach-Object { Join-Path $SdkDir $_ }) + $cudart.FullName
foreach ($opt in 'nvngxruntime.dll') {
    $p = Join-Path $SdkDir $opt
    if (Test-Path $p) { $files += $p }
}

$version = (Get-Item (Join-Path $SdkDir 'NVVideoEffects.dll')).VersionInfo.ProductVersion
if (-not $version) { $version = (Get-Item (Join-Path $SdkDir 'NVVideoEffects.dll')).VersionInfo.FileVersion }
$version = ($version -replace '[^0-9.]', '').Trim('.')
if (-not $version) { $version = 'unknown' }

$stage = Join-Path ([IO.Path]::GetTempPath()) "passthrify-vfg-runtime-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force $stage | Out-Null
try {
    foreach ($f in $files) { Copy-Item $f $stage }
    # NVIDIA's own license / notice files from the SDK, when present.
    $sdkRoot = Split-Path $SdkDir
    Get-ChildItem $SdkDir, $sdkRoot -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match 'licen[cs]e|eula|notice|third.?party' } |
        ForEach-Object { Copy-Item $_.FullName (Join-Path $stage "NVIDIA-$($_.Name)") }
    @"
NVIDIA Maxine Video Effects SDK runtime (Video Frame Generation), version $version

These files are NVIDIA software, redistributed with Passthrify under the NVIDIA Maxine SDK
license and used only by Passthrify. They are not covered by Passthrify's own license.
NVIDIA license: https://developer.nvidia.com/downloads/maxine-sdk-license
Copyright (c) NVIDIA Corporation. All rights reserved.
"@ | Out-File -Encoding utf8 (Join-Path $stage 'NOTICE.txt')

    New-Item -ItemType Directory -Force $OutDir | Out-Null
    $zip = Join-Path (Resolve-Path $OutDir).Path "Passthrify-VFG-runtime-$version.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    "$hash  $(Split-Path $zip -Leaf)" | Out-File -Encoding ascii "$zip.sha256"
} finally {
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
}

$mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "Packaged $zip ($mb MB)" -ForegroundColor Green
Write-Host "Contents: $((($files | ForEach-Object { Split-Path $_ -Leaf }) + 'NOTICE.txt') -join ', ')"
Write-Host "`nPublish it once:"
Write-Host "  gh release create vfg-runtime `"$zip`" `"$zip.sha256`" --title `"NVIDIA frame generation runtime`" --notes `"Optional download for Settings > Display > Frame generation. NVIDIA software under the Maxine SDK license.`" --latest=false"
Write-Host "Replace it later (delete the old zip asset first if the version changed):"
Write-Host "  gh release upload vfg-runtime `"$zip`" `"$zip.sha256`" --clobber"
