# MapForge Studio

MapForge is a Windows-first, native projection-mapping prototype. This repository currently implements the first testable vertical slice: a Producer desktop app, a separate Player desktop app, two simulated projector windows, a versioned show format, and a phone/iPad browser controller.

## What works now

- Native Producer and Player processes (Rust + egui/wgpu; the playback UI is not a browser).
- One 1920×1080 virtual stage split across two simulated projector outputs.
- Import image/video metadata and calculate a SHA-256 checksum without loading the whole file into memory.
- Create, drag, numerically position/resize, change opacity, and route a layer to either output.
- Atomic save and reload of schema-versioned JSON projects.
- TCP commands for project sync, prepare, play, pause, stop, volume, mute, and blackout.
- Duplicate command protection and explicit protocol versioning.
- A LAN controller at `http://PLAYER-IP:8080`.
- Player state and output continue if Producer or the browser disconnects.

The simulated outputs currently draw routed layer placeholders. Actual image/video decoding, audio output, frame timing, asset transfer, warping, and physical-projector fullscreen output remain later implementation work. See [current status](docs/CURRENT_STATUS.md).

## Build and run on Windows

See [Windows test instructions](docs/WINDOWS_TEST.md). The shortest path after installing Rust is:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

Then launch `dist\MapForge-Player.exe` first and `dist\MapForge-Producer.exe` second.

## Development

```bash
cargo test --workspace
cargo run -p mapforge-player
cargo run -p mapforge-producer
```

The source of truth for product scope remains [the software plan](docs/PROJECTION_MAPPING_SOFTWARE_PLAN.md).
