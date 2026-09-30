# Windows test instructions

## 1. Copy the project

Copy the `Madprojector` folder to the Windows PC. You may omit the generated `target` folder (it is large and macOS build output cannot run on Windows). Keep the source folders, `Cargo.toml`, `Cargo.lock`, and `scripts` folder.

## 2. Install build prerequisites once

Install **Rust (MSVC)** from <https://rustup.rs>. If the installer asks for Visual Studio components, allow it to install **Visual Studio Build Tools 2022** with **Desktop development with C++** and a Windows SDK.

Optional: install FFmpeg and make `ffprobe.exe` available on `PATH` if you want video duration/resolution probing. Images do not require FFmpeg.

## 3. Build

Open PowerShell in the project folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

The script runs all tests and creates:

- `dist\MapForge-Player.exe`
- `dist\MapForge-Producer.exe`

## 4. Local one-PC test

1. Open `MapForge-Player.exe`. Approve the Windows Firewall private-network prompt.
2. Confirm the two simulated projector windows open.
3. Open `MapForge-Producer.exe`.
4. Leave the Player address as `127.0.0.1:4777` and click **Connect / Sync Project**.
5. Click **Import Media**, select a PNG/JPG (or a video for metadata), and drag the new layer on the stage.
6. Change width, height, and output routing in the right panel.
7. Click **Connect / Sync Project** again, then **Prepare**, then **Play**.
8. Confirm routing changes which simulated projector contains the layer block.
9. Test **Blackout** and **Restore**.
10. Save the project, close Producer, reopen it, and load the saved JSON file.

## 5. iPad/phone controller test

On Windows, run `ipconfig` and find the PC's IPv4 address, for example `192.168.1.50`. With the Player open and the mobile device on the same private network, browse to:

```text
http://192.168.1.50:8080
```

Test play, pause, stop, volume, mute, blackout, and restore. Close Producer while Player is in Play; the Player state should remain active and the browser should still work.

## 6. Two-PC network test

Run Player on the playback PC. On Producer, replace `127.0.0.1:4777` with the playback PC's IPv4 address and port, such as `192.168.1.50:4777`. Both PCs must be on the same trusted LAN and Windows Firewall must allow MapForge Player on private networks.

Record Windows version, CPU, RAM, GPU, VRAM, storage, display connections, and projector resolution/refresh rate before the physical-output milestone.
