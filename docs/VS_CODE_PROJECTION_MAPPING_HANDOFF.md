# VS Code Handoff: Projection Mapping Software

## Product Goal

Build an original Windows-first projection-mapping and multi-display show-control system. It is inspired by the category of products such as Dataton WATCHOUT, but it must use its own interface, branding, implementation, and project format.

The real target hardware is:

- One Windows laptop running the Producer and system controller.
- Two Windows playback PCs running Player nodes.
- Multiple projectors connected across the two playback PCs.
- One iPad running an installable browser-based live controller.
- Wired Ethernet between all Windows machines.
- Wi-Fi for the iPad on the same private network.

The complete requirements are maintained in `docs/PROJECTION_MAPPING_SOFTWARE_PLAN.md`. Treat that document as the product source of truth.

## Recommended Technical Direction

- C++20 for the native Producer and Player core.
- Qt 6 and QML for the desktop interface.
- CMake for builds.
- FFmpeg for media probing, decoding, seeking, and proxy generation.
- Vulkan-backed rendering through a renderer abstraction.
- SQLite for local indexes and operational state.
- JSON with explicit schema versions for portable show/project documents.
- WebSocket plus an HTTP API for the iPad controller.
- mDNS or UDP discovery only as a convenience; support manual IP configuration.
- Local SSD/NVMe media caching on every Player.
- Automated tests for project serialization, routing, timing, and network state.

Do not build the playback engine as a browser application. The iPad controller may be a PWA, but rendering and projector output must remain native.

## Proposed Repository Layout

```text
projection-mapper/
  CMakeLists.txt
  README.md
  docs/
    architecture.md
    project-format.md
    network-protocol.md
    hardware-test-plan.md
  apps/
    producer/
    player/
    controller-web/
  core/
    model/
    media/
    rendering/
    timing/
    networking/
    persistence/
  shared/
    protocol/
    logging/
  tests/
    unit/
    integration/
    fixtures/
```

## Core Domain Model

Start with these concepts and keep them independent of the user interface:

- `ShowProject`: metadata, schema version, assets, scenes, projector configuration, and settings.
- `Asset`: source path, checksum, media metadata, proxy path, and availability per Player.
- `Scene`: ordered layers, transition, audio state, duration, and cue behaviour.
- `Layer`: asset, transform, crop, opacity, playback settings, and output routing.
- `VirtualStage`: total canvas size and coordinate system.
- `ProjectorOutput`: Player assignment, GPU output, stage rectangle, warp, mask, blend, and colour correction.
- `ProjectorGroup`: named collection such as Left Wall, Centre, Floor, or All.
- `PlayerNode`: identity, address, capabilities, cache state, readiness, and health.
- `Cue`: scene action, trigger, schedule, and transition.
- `ShowState`: stopped, preparing, ready, playing, paused, blackout, or error.

Use stable UUIDs for assets, scenes, layers, outputs, and nodes. Never use list positions as persistent identities.

## First Vertical Slice

The first milestone is not the complete product. Deliver a demonstrable end-to-end slice that works on one development machine without physical projectors:

1. Producer window with a virtual stage and two simulated projector rectangles.
2. Import one image or video and probe its metadata.
3. Create one scene and one layer.
4. Move and resize the layer numerically and by dragging.
5. Route the layer to Projector 1, Projector 2, or both.
6. Save and reload the project using a versioned JSON format.
7. Start a local Player process and connect it to Producer.
8. Send prepare, play, pause, stop, volume, and blackout commands.
9. Display Player connection and readiness state.
10. Run a minimal browser controller with scene, play, volume, mute, and blackout controls.
11. Keep the Player operating if Producer or the browser controller disconnects.
12. Provide automated tests and documented local run instructions.

Simulated outputs should be normal resizable windows that behave like projector outputs. This allows development without access to the installation hardware.

## Second Milestone

After the first vertical slice is stable:

- Multiple scenes and transitions.
- Media preloading and buffered playback.
- Accurate seeking and looping.
- Crop, rotate, fit, fill, and stretch controls.
- Projector groups and one-layer-to-many-output routing.
- Calibration grid.
- Four-corner correction.
- Polygon mask and soft edge controls.
- Manual edge blending.
- Local proxy generation for Producer previews.

## Third Milestone

- Run Players on two physical PCs.
- Discover and register nodes.
- Copy large assets with resumable transfer.
- Verify SHA-256 checksums.
- Report storage and GPU capabilities.
- Prepare assets on both Players before accepting `ready`.
- Schedule playback at a future shared timestamp.
- Measure and log timing drift.
- Test disconnect and reconnection behaviour.

## Critical Behaviour

- Players render from local storage, not from a Wi-Fi or internet media stream.
- A long or large video is buffered; it is never fully loaded into RAM.
- One large panoramic video can span multiple projectors.
- Different projectors can show different layers or scenes.
- Each output has independent crop, warp, mask, blend, brightness, gamma, black level, and RGB correction.
- Scene and volume changes from the iPad are reflected immediately in Producer and Player state.
- The current scene continues when the iPad or Producer disconnects.
- Blackout affects visual output without destroying the current playback state.
- Commands are idempotent and contain command IDs to prevent duplicate execution after reconnects.
- Network messages and project files have explicit protocol/schema versions.
- No cloud or internet connection is required during a show.

## Engineering Rules

- Inspect the repository and existing work before editing.
- Do not overwrite unrelated user files or uncommitted changes.
- Keep the media/rendering/timing core separate from Qt UI code.
- Avoid copying WATCHOUT branding, user interface, proprietary formats, or documentation.
- Use deterministic project serialization and atomic saves.
- Add structured logs without secrets or unnecessary personal data.
- Validate paths and network messages.
- Bound file sizes, message sizes, queue sizes, and timeouts.
- Do not silently transcode or alter original user media.
- Use a simulation mode for development and automated testing.
- Distinguish implemented behaviour from planned behaviour in documentation.
- Do not claim frame-accurate multi-PC synchronization until it has been measured on the physical machines.
- Do not begin 3D mapping, camera calibration, NDI, DMX, genlock, or plugins before the first three milestones are stable.

## Completion Criteria for the First Vertical Slice

- A clean build works from documented commands.
- Producer and Player run as separate processes.
- The simulated outputs correctly respect layer routing.
- Project save/reload preserves the stage, scenes, layers, transforms, and routing.
- Browser/iPad controls can change scene state, volume, mute, and blackout.
- Disconnecting the controller does not stop Player output.
- Reconnecting restores current state without restarting the show.
- Unit and integration tests pass.
- Known limitations and the next milestone are recorded truthfully.

## Information to Record When Physical Hardware Is Available

- Windows version on every computer.
- CPU, RAM, GPU, VRAM, storage type, and free capacity.
- GPU output count and connector types on both Players.
- Projector count, model, native resolution, refresh rate, and connection type.
- Network switch model and link speed.
- Required canvas resolution and maximum media resolution/bitrate.
- Which machine and audio interface provide show audio.
- Physical projection layout and expected overlap between projectors.
