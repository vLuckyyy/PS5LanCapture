# Builds the release and writes the GitHub release files to dist\:
#   PS5-LAN-Capture-Setup-<version>.exe, PS5-LAN-Capture-<version>-portable.zip,
#   PS5-LAN-Capture-<version>-source.zip, SHA256SUMS.txt
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist 'stage'

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force "$stage\licenses" | Out-Null

$release = Join-Path $root 'target\release'
Copy-Item "$release\ps5-lan-capture.exe" "$stage\PS5LanCapture.exe"
foreach ($dll in 'pslc_core.dll', 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item "$release\$dll" $stage
}
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -property installationPath
$crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC14*.CRT\vcruntime140.dll" | Sort-Object FullName | Select-Object -Last 1
Copy-Item $crt.FullName $stage
Copy-Item README.md $stage
Copy-Item LICENSE "$stage\LICENSE.txt"

$chiaki = Join-Path $root 'third_party\chiaki-ng'
$share = Join-Path $root '.tools\vcpkg\installed\x64-windows-static-md\share'
$licenses = @{
    'chiaki-ng.txt'   = "$chiaki\LICENSES\AGPL-3.0-only-OpenSSL.txt"
    'FFmpeg.txt'      = "$root\.tools\ffmpeg\LICENSE.txt"
    'curl.txt'        = "$chiaki\third-party\curl\COPYING"
    'nanopb.txt'      = "$chiaki\third-party\nanopb\LICENSE.txt"
    'jerasure.txt'    = "$chiaki\third-party\jerasure\COPYING"
    'gf-complete.txt' = "$chiaki\third-party\gf-complete\COPYING"
}
foreach ($pkg in 'openssl', 'opus', 'json-c', 'libevent', 'miniupnpc') {
    $licenses["$pkg.txt"] = "$share\$pkg\copyright"
}
foreach ($entry in $licenses.GetEnumerator()) {
    Copy-Item $entry.Value (Join-Path "$stage\licenses" $entry.Key)
}
python -I "$root\scripts\third_party_notices.py" "$root" "$stage\licenses\rust-crates.md"
if ($LASTEXITCODE -ne 0) { throw 'third-party notices failed' }
Set-Content -Encoding utf8 "$stage\licenses\FFmpeg-source.txt" @"
avcodec-62.dll, avutil-60.dll and swresample-6.dll are unmodified FFmpeg 8.1 LGPL builds from
https://github.com/BtbN/FFmpeg-Builds (ffmpeg-n8.1-latest-win64-lgpl-shared-8.1).
FFmpeg source: https://ffmpeg.org/releases/ffmpeg-8.1.tar.xz
"@

Compress-Archive -Path "$stage\*" -DestinationPath "$dist\PS5-LAN-Capture-$version-portable.zip" -CompressionLevel Optimal

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }
& $iscc /Q "/DAppVersion=$version" "/DSourceDir=$stage" "/DOutputDir=$dist" "$root\installer\ps5-lan-capture.iss"
if ($LASTEXITCODE -ne 0) { throw 'installer build failed' }

$src = Join-Path $dist 'src-stage\ps5-lan-capture'
New-Item -ItemType Directory -Force "$src\third_party\chiaki-ng" | Out-Null
git ls-files | Where-Object { $_ -ne 'third_party/chiaki-ng' } | ForEach-Object {
    $target = Join-Path $src $_
    New-Item -ItemType Directory -Force (Split-Path $target) | Out-Null
    Copy-Item $_ $target
}
foreach ($item in 'lib', 'third-party', 'cmake', 'CMakeLists.txt', 'COPYING', 'LICENSES', 'README.md', 'vcpkg.json') {
    Copy-Item -Recurse (Join-Path $chiaki $item) "$src\third_party\chiaki-ng\"
}
Get-ChildItem -Recurse -Force $src -Filter '.git' | Remove-Item -Recurse -Force
Compress-Archive -Path $src -DestinationPath "$dist\PS5-LAN-Capture-$version-source.zip" -CompressionLevel Optimal
Remove-Item -Recurse -Force (Join-Path $dist 'src-stage'), $stage

Get-ChildItem $dist -File | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | ForEach-Object {
    "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name
} | Set-Content -Encoding ascii "$dist\SHA256SUMS.txt"
Get-ChildItem $dist | Format-Table Name, Length
