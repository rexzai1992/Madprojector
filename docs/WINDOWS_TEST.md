# Windows test instructions

## 1. Install

Run **MapForge-Setup-<version>.exe** on each PC (see the README for where to download it). Choose:

- **MapForge Player** on every show PC, plus **Open the Player when Windows starts**.
- **MapForge Producer** on the design PC.

FFmpeg, shortcuts and the firewall rules are included. Windows SmartScreen may warn because the installer is not code-signed yet: choose **More info → Run anyway**.

## 2. Build it yourself (optional)

Install **Rust (MSVC)** from <https://rustup.rs> (with Visual Studio Build Tools, *Desktop development with C++*) and **NSIS** from <https://nsis.sourceforge.io>. Then, in the project folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

It runs the tests, builds `dist\MapForge-Player.exe` and `dist\MapForge-Producer.exe`, downloads FFmpeg, and makes `dist\MapForge-Setup-<version>.exe`.

## 4. Local one-PC test

1. Open **MapForge Player** from the desktop. The first time, choose **Master PC**.
2. Confirm the two simulated projector windows open.
3. Open **MapForge Producer**.
4. Leave the Player address as `127.0.0.1:4777` and click **Connect / Sync Project**.
5. Click **Import Media**, select a PNG/JPG (or a video for metadata), and drag the new layer on the stage.
6. Change width, height, and output routing in the right panel.
7. Click **Connect / Sync Project** again, then **Prepare**, then **Play**.
8. Confirm the real image/video is cropped continuously across both simulated projectors and fades through the shared overlap.
9. Test **Blackout** and **Restore**.
10. Save the project, close Producer, reopen it, and load the saved JSON file.

## 5. iPad/phone controller test

On Windows, run `ipconfig` and find the PC's IPv4 address, for example `192.168.1.50`. With the Player open and the mobile device on the same private network, browse to:

```text
http://192.168.1.50:8080
```

Test play, pause, stop, volume, mute, blackout, and restore. Close Producer while Player is in Play; the Player state should remain active and the browser should still work.

## 6. Multi-PC LAN test

1. Start **MapForge Player** on every playback PC and note each PC's IPv4 address (`ipconfig`). Allow the Player through Windows Firewall on private networks (TCP 4777 for show control, TCP 8080 for media transfer and the phone controller).
2. In Producer, open **Edit projectors**, select each projector and set **Player PC** to the IP of the PC it is plugged into, and its **Resolution**. Several projectors can share one PC.
3. The Players menu (top right) should show every PC online with its latency.
4. Click **Identify projectors**: every projector shows its number, name, Player IP and resolution. Check the physical order and fix any swapped cables or IPs.
5. In **LAN & media**, click **Test speed** for each PC. Gigabit wired LAN should report several hundred Mbit/s or more; around 90 Mbit/s means a 100 Mbit link or Wi-Fi.
6. Click **Send media** for each PC and wait for the progress bars. Files are stored in `%USERPROFILE%\MapForge Media` (override with the `MAPFORGE_MEDIA_DIR` environment variable) and checked against their SHA-256 before use. **Check media** should then report all files ready.
7. Play a scene and press its F-key and a cue hotkey. The Players should enter **Ready**, then start together after the 1.5-second preparation window.
8. Open the Players popup in Producer. Record each clock offset, scheduled-start error, and reported playback spread. Repeat at least ten starts and test once while copying a large file over the LAN.

Record Windows version, CPU, RAM, GPU, VRAM, storage, display connections, and projector resolution/refresh rate before the physical-output milestone.

## 7. Show mode (no Producer on show day)

1. Start the Player on each PC. The first time, it asks **Master PC** or **Sub PC** and shows this PC's IP.
   - Master: tick **Start the show automatically when this PC starts** if wanted, and note the IP.
   - Sub: type the master's IP.
   The answer is saved in `player-settings.json` in the media folder; later starts skip the question. **Change setup** reopens it.
2. In Producer, set each projector's Player PC IP and display, then open **Controller → Show PCs**: every PC should show Master or Sub. Keep **Play sound on the master PC only** ticked unless every PC has speakers.
3. Click **Send show to all Players** (or send it to the master only) and **Send media** to the master. Subs copy the show and any missing media from the master by themselves, with progress shown in their window.
4. Close Producer and every Player. Start the subs, then the master. With autostart on, the master waits until every sub answers (up to 20 s), then all PCs start the first scene together.
5. Use the iPad at `http://MASTER-IP:8080` (a sub's page also works; it passes taps to the master). Change scenes, cues, loops, pause/play, volume and blackout, and confirm every PC follows. The status line warns if a PC is offline.
6. Close one sub's Player mid-show and start it again: it should rejoin at the same position.
7. To start on boot, put a shortcut to `mapforge-player.exe` in `shell:startup` on each PC and set Windows to sign in automatically.

## Physical display assignment

1. Open Windows **Display Settings**, choose **Extend these displays**, and note the numbered display attached to each projector.
2. In Producer, open **Edit projectors**, select a projector, and set **Player display** to the matching display number. Leave it on **Preview window** until the number is confirmed.
3. Send/sync the project, then enable **Identify projectors**. The assigned Player opens that output as an undecorated, native-size window over the complete monitor rectangle.
4. On the Player control window, verify the detected display count, resolutions and desktop coordinates. Use **Refresh displays** after reconnecting hardware.
5. If an assigned display is unavailable, verify that its output opens as a normal preview instead of covering an unrelated display.

Record results for primary and secondary monitors, negative desktop coordinates, mixed DPI scaling, projector reconnects, and Windows display reordering. Borderless placement is compiled for Windows but is not considered physically proven until this checklist passes on the show hardware.

## Corner correction and masks

1. Turn on the **Grid** test pattern and select a projector in **Edit projectors**.
2. Under **Corner correction**, drag all four handles. Confirm the grid corners follow the physical surface and straight grid lines remain straight under perspective correction.
3. Enter corner percentages numerically and confirm they produce the same result. Press **Reset corners** and confirm the full rectangular image returns.
4. Enable **Polygon mask**, move the four default points inward, and confirm everything outside the polygon becomes black.
5. Add and remove mask points, keeping them ordered around the visible region. Save, close and reload the project and confirm the warp and mask persist.

Masks are hard-edged in this build. Do not record feathering as passed; it remains a later GPU-rendering milestone.
