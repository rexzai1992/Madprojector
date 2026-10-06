$ErrorActionPreference = "Stop"

# Builds MapForge for Windows into dist\, bundles FFmpeg and, when NSIS is
# installed (https://nsis.sourceforge.io), makes dist\MapForge-Setup-<version>.exe.
# GitHub Actions runs this same script (.github/workflows/windows-installer.yml).

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Rust was not found. Install the MSVC Rust toolchain from https://rustup.rs and reopen PowerShell."
}

$version = (Select-String -Path ".\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

Write-Host "Running MapForge tests..."
cargo test --workspace
if ($LASTEXITCODE -ne 0) { throw "Tests failed." }

Write-Host "Building optimized Windows binaries..."
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw "Release build failed." }

New-Item -ItemType Directory -Force -Path ".\dist" | Out-Null
Copy-Item ".\target\release\mapforge-player.exe" ".\dist\MapForge-Player.exe" -Force
Copy-Item ".\target\release\mapforge-producer.exe" ".\dist\MapForge-Producer.exe" -Force

# FFmpeg (LGPL build) plays the media; it is installed next to MapForge.
if (-not (Test-Path ".\dist\ffmpeg.exe")) {
    Write-Host "Downloading FFmpeg..."
    $zip = Join-Path $env:TEMP "mapforge-ffmpeg.zip"
    $unpacked = Join-Path $env:TEMP "mapforge-ffmpeg"
    Invoke-WebRequest "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-lgpl.zip" -OutFile $zip
    if (Test-Path $unpacked) { Remove-Item $unpacked -Recurse -Force }
    Expand-Archive $zip -DestinationPath $unpacked
    $root = Get-ChildItem $unpacked -Directory | Select-Object -First 1
    Copy-Item (Join-Path $root.FullName "bin\ffmpeg.exe") ".\dist\ffmpeg.exe" -Force
    Copy-Item (Join-Path $root.FullName "bin\ffprobe.exe") ".\dist\ffprobe.exe" -Force
    Copy-Item (Join-Path $root.FullName "LICENSE.txt") ".\dist\FFmpeg-LICENSE.txt" -Force
}

$makensis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $makensis -and (Test-Path "${env:ProgramFiles(x86)}\NSIS\makensis.exe")) {
    $makensis = "${env:ProgramFiles(x86)}\NSIS\makensis.exe"
}
if ($makensis) {
    Write-Host "Making the installer..."
    & $makensis "/DVERSION=$version" ".\installer\mapforge.nsi"
    if ($LASTEXITCODE -ne 0) { throw "Installer build failed." }
    Write-Host "Done: dist\MapForge-Setup-$version.exe"
} else {
    Write-Host "Build complete in dist\. Install NSIS to also make the one-file installer."
}
