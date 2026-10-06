# MapForge Studio

MapForge is a Windows-first, native projection-mapping prototype. This repository currently implements the first testable vertical slice: a Producer desktop app, a separate Player desktop app, two simulated projector windows, a versioned show format, and a phone/iPad browser controller.

## What works now

- Native Producer and Player processes (Rust + egui/wgpu; the playback UI is not a browser).
- A resizable virtual stage spanning any number of projector outputs; assign each output to a numbered Windows display for borderless native-size playback, or keep it in a safe preview window.
- Import images and videos (or drag and drop them), then drag, resize, fit, fill, loop and route layers per projector.
- A timeline with tracks for images, video and music: drag media in, move and trim clips, mark cues (start points) with their own hotkeys and iPad buttons.
- Multiple scenes with assignable hotkeys (default F1–F12), plus Space/Esc/B for play-pause, stop and blackout.
- Projectors with native resolution assigned to Player PCs on the LAN, an Identify pattern, and LAN speed test and media transfer per PC.
- Real still-image and FFmpeg video playback with pause, stop and looping, continuous across all outputs.
- Edge blending set in percentages, with a curve that keeps the overlap at the same brightness, per-projector brightness and black lift, and grid/white/gray test patterns.
- Per-projector four-corner perspective correction and editable hard-edged polygon masks, saved with the show and applied to media and calibration output.
- Atomic save and reload of schema-versioned JSON projects.
- TCP commands for project sync, prepare, clock-adjusted scheduled play, pause, stop, volume, mute, and blackout.
- Duplicate command protection and explicit protocol versioning.
- A LAN controller at `http://PLAYER-IP:8080`, customisable from the Producer's **Controller** window, with scene buttons.
- Player state and output continue if Producer or the browser disconnects.
- Show mode without Producer: each Player is set up once as master or sub. Players keep the last show and reopen it on start; the master can start the show by itself and passes iPad commands to the subs so they play together, and subs copy the show and media from the master. Sound plays on the master only by default.

Continuous cross-PC drift correction, GPU video decoding, mesh warping and feathered masks remain later implementation work. See [current status](docs/CURRENT_STATUS.md) and the [edge blending plan](docs/EDGE_BLENDING_PLAN.md).

## Install on Windows

Download **MapForge-Setup-<version>.exe** (built by GitHub Actions: open the repository's **Actions → Windows installer** run and download the *MapForge-Setup* artifact, or take it from **Releases**) and run it on each PC:

- Every show PC: **MapForge Player** (always installed) and **Open the Player when Windows starts**.
- The design PC: also tick **MapForge Producer**.

The installer includes FFmpeg, adds Start menu and desktop shortcuts and the Windows Firewall rules, and adds an uninstaller. The first time the Player opens it asks whether the PC is the **Master** or a **Sub**.

To build the installer yourself on a Windows PC with Rust and NSIS installed:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

## Development

```bash
cargo test --workspace
cargo run -p mapforge-player
cargo run -p mapforge-producer
```

The source of truth for product scope remains [the software plan](docs/PROJECTION_MAPPING_SOFTWARE_PLAN.md).
