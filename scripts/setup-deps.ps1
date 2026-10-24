# Prepares the native build dependencies of PS5 LAN Capture in .tools/ (project-local, nothing global
# except the optional winget installs listed below).
#   - chiaki-ng sources (third_party/chiaki-ng, with submodules)
#   - vcpkg + static libraries: openssl, json-c, libevent, miniupnpc, opus, protobuf (protoc), pkgconf
#   - FFmpeg 8.1 LGPL shared development build (BtbN)
# Requirements checked: Rust (rustup), Git, Python 3, Visual Studio 2022 Build Tools (C++), LLVM clang-cl.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$tools = Join-Path $root '.tools'
New-Item -ItemType Directory -Force $tools | Out-Null

function Require($name, $hint) {
    if (-not (Get-Command $name -ErrorAction SilentlyContinue)) { throw "$name not found. $hint" }
}
Require git 'Install: winget install Git.Git'
Require cargo 'Install Rust: https://rustup.rs'
Require python 'Install: winget install Python.Python.3.12'
if (-not (Test-Path 'C:\Program Files\LLVM\bin\clang-cl.exe') -and -not (Get-Command clang-cl -ErrorAction SilentlyContinue)) {
    Write-Host 'Installing LLVM (clang-cl)...'
    winget install --id LLVM.LLVM -e --silent --accept-package-agreements --accept-source-agreements
}
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere) -or -not (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)) {
    throw 'Visual Studio 2022 Build Tools with "Desktop development with C++" are required: winget install Microsoft.VisualStudio.2022.BuildTools'
}

python -m pip install --user --quiet protobuf

git -C $root submodule update --init --recursive

$vcpkg = Join-Path $tools 'vcpkg'
if (-not (Test-Path (Join-Path $vcpkg 'vcpkg.exe'))) {
    git clone --depth 1 https://github.com/microsoft/vcpkg.git $vcpkg
    & (Join-Path $vcpkg 'bootstrap-vcpkg.bat') -disableMetrics
}
& (Join-Path $vcpkg 'vcpkg.exe') install --triplet x64-windows-static-md openssl json-c libevent miniupnpc opus protobuf pkgconf

$ffmpeg = Join-Path $tools 'ffmpeg'
if (-not (Test-Path (Join-Path $ffmpeg 'include\libavcodec\avcodec.h'))) {
    $zip = Join-Path $tools 'ffmpeg.zip'
    Invoke-WebRequest 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip' -OutFile $zip
    Expand-Archive $zip -DestinationPath $tools -Force
    Move-Item (Join-Path $tools 'ffmpeg-n8.1-latest-win64-lgpl-shared-8.1') $ffmpeg
    Remove-Item $zip
}
Write-Host 'Dependencies ready. Build with: cargo build --release'
