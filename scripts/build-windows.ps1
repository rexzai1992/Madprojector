$ErrorActionPreference = "Stop"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Rust was not found. Install the MSVC Rust toolchain from https://rustup.rs and reopen PowerShell."
}

Write-Host "Running MapForge tests..."
cargo test --workspace
if ($LASTEXITCODE -ne 0) { throw "Tests failed." }

Write-Host "Building optimized Windows binaries..."
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) { throw "Release build failed." }

New-Item -ItemType Directory -Force -Path ".\dist" | Out-Null
Copy-Item ".\target\release\mapforge-player.exe" ".\dist\MapForge-Player.exe" -Force
Copy-Item ".\target\release\mapforge-producer.exe" ".\dist\MapForge-Producer.exe" -Force

Write-Host "Build complete. Start dist\MapForge-Player.exe first, then dist\MapForge-Producer.exe."
