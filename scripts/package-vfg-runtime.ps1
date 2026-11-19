<#
.SYNOPSIS
    Packages NVIDIA's Video Frame Generation runtime into the optional download Passthrify
    offers in Settings › Display (dist\Passthrify-VFG-runtime-<version>.zip + .sha256).

.DESCRIPTION
    Point -SdkDir at YOUR OWN NVIDIA Video Effects SDK install (its root or bin folder),
    downloaded from NVIDIA NGC with the Video Frame Generation feature added
    (install_feature.ps1). Don't package files copied out of another application.

    Before publishing: SDK 1.3 is under NVIDIA's Software License Agreement and Product-Specific
    Terms for AI Products (in the SDK's license folder, copied into the zip). Software received
    through NVIDIA's developer program may not be distributed (section 1.2.8 of the AI terms),
    so get NVIDIA's written OK (maxinesdk-support@nvidia.com) before uploading the zip.

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
# Accept the SDK root (VideoFX) as well as its bin folder.
if (-not (Test-Path (Join-Path $SdkDir 'NVVideoEffects.dll')) -and (Test-Path (Join-Path $SdkDir 'bin\NVVideoEffects.dll'))) {
    $SdkDir = Join-Path $SdkDir 'bin'
}

function Get-Machine($path) {
    $b = [IO.File]::ReadAllBytes($path)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    [BitConverter]::ToUInt16($b, $pe + 4)
}

# Core: only these are needed for frame generation (NVVideoEffects imports NVCVImage + cudart;
# the ~1 GB of cuBLAS/NPP/TensorRT in the SDK's bin serve other effects).
foreach ($f in 'NVVideoEffects.dll', 'NVCVImage.dll') {
    if (-not (Test-Path (Join-Path $SdkDir $f))) { throw "$f not found in $SdkDir" }
}
if ((Get-Machine (Join-Path $SdkDir 'NVVideoEffects.dll')) -ne 0x8664) {
    throw "$SdkDir holds a non-x64 SDK (the _woa download is for Windows on Arm). Download the Windows x64 SDK Core."
}
# Feature: next to the core, or where install_feature.ps1 puts it (features\nvvfxvideoframegeneration\...).
$feature = Get-ChildItem $SdkDir -Filter 'nvVFXVideoFrameGeneration.dll' | Select-Object -First 1
if (-not $feature) {
    $featureRoot = Join-Path (Split-Path $SdkDir) 'features\nvvfxvideoframegeneration'
    $feature = Get-ChildItem $featureRoot -Recurse -Filter 'nvVFXVideoFrameGeneration.dll' -ErrorAction SilentlyContinue | Select-Object -First 1
}
if (-not $feature) {
    throw "nvVFXVideoFrameGeneration.dll not found. Install it: cd <sdk>\features; .\install_feature.ps1 -features nvvfxvideoframegeneration"
}
$files = @((Join-Path $SdkDir 'NVVideoEffects.dll'), (Join-Path $SdkDir 'NVCVImage.dll'))
# The feature's folder may carry its own dependencies (its cudart, NGX runtime): take them all.
$files += (Get-ChildItem $feature.DirectoryName -Filter '*.dll').FullName
foreach ($dir in $SdkDir, $feature.DirectoryName) {
    foreach ($pattern in 'cudart64_*.dll', 'nvngxruntime.dll') {
        $files += (Get-ChildItem $dir -Filter $pattern -ErrorAction SilentlyContinue).FullName
    }
}
$files = $files | Where-Object { $_ } | Sort-Object -Unique
if (-not ($files | Where-Object { (Split-Path $_ -Leaf) -like 'cudart64_*.dll' })) {
    throw "cudart64_*.dll not found next to the SDK or the feature (needed for fast GPU-side conversion)"
}
# NVIDIA's DLLs need the Visual C++ runtime; ship it app-local in case a PC lacks it.
foreach ($crt in 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
    $p = Join-Path $env:WINDIR "System32\$crt"
    if (Test-Path $p) { $files += $p }
}

# The SDK's version (1.3.0.0) is in videofx_version.h; the DLL's own version differs.
$version = $null
$header = Join-Path (Split-Path $SdkDir) 'videofx_version.h'
if (Test-Path $header) {
    $m = Select-String -Path $header -Pattern 'NVIDIA_VIDEOEFFECTS_SDK_VERSION_STRING\s+"([0-9.]+)"' | Select-Object -First 1
    if ($m) { $version = $m.Matches[0].Groups[1].Value }
}
if (-not $version) { $version = (Get-Item (Join-Path $SdkDir 'NVVideoEffects.dll')).VersionInfo.ProductVersion }
$version = ($version -replace '[^0-9.]', '').Trim('.')
if (-not $version) { $version = 'unknown' }

$stage = Join-Path ([IO.Path]::GetTempPath()) "passthrify-vfg-runtime-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force $stage | Out-Null
try {
    foreach ($f in $files) { Copy-Item $f $stage }
    # NVIDIA's license / notice files from the SDK (its license\ folder holds the PDFs).
    $sdkRoot = Split-Path $SdkDir
    $licenseDir = Join-Path $stage 'license'
    New-Item -ItemType Directory -Force $licenseDir | Out-Null
    Get-ChildItem (Join-Path $sdkRoot 'license') -File -ErrorAction SilentlyContinue | Copy-Item -Destination $licenseDir
    Get-ChildItem $SdkDir, $sdkRoot, $feature.DirectoryName -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match 'licen[cs]e|eula|notice|third.?party' -and $_.Extension -in '.txt', '.pdf', '.md' } |
        ForEach-Object { Copy-Item $_.FullName (Join-Path $licenseDir $_.Name) -Force }
    @"
NVIDIA Maxine Video Effects SDK runtime (Video Frame Generation), version $version

These files are NVIDIA software, redistributed with Passthrify under NVIDIA's license terms
(see the license folder) and used only by Passthrify. They are not covered by Passthrify's
own license. The Visual C++ runtime DLLs are Microsoft's redistributable files.
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
Write-Host "  gh release create vfg-runtime `"$zip`" `"$zip.sha256`" --title `"NVIDIA frame generation runtime`" --notes `"Optional download for Settings > Display > Frame generation. NVIDIA software under NVIDIA's license terms (license folder in the zip).`" --latest=false"
Write-Host "Replace it later (delete the old zip asset first if the version changed):"
Write-Host "  gh release upload vfg-runtime `"$zip`" `"$zip.sha256`" --clobber"
