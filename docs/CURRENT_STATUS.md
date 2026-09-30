# Current implementation status

Updated: 2026-09-30

## Implemented and locally verifiable

- Rust workspace with shared model/protocol core and separate Producer/Player binaries.
- Schema v1 model using stable UUIDs for project, scene, asset, layer, and output identity.
- Deterministic JSON serialization, validation, a 32 MiB project-file safety limit, and atomic save replacement.
- SHA-256 media checksum using streaming file reads.
- Two-output virtual stage, draggable layer blocks, numeric transforms, opacity, and per-output routing.
- Player TCP listener on port 4777 with protocol v1 envelopes and bounded duplicate-command tracking.
- Browser controller HTTP server on port 8080 with state recovery by polling.
- Transport, volume, mute, and non-destructive blackout state.
- Unit tests for serialization and default stage/output coverage.

## Prototype limitations

- Outputs render representative layer rectangles, not image pixels or decoded video frames yet.
- `ffprobe` is optional and must be installed separately for video width, height, and duration. Import still succeeds without it.
- The Player receives the project model but does not copy media to its cache yet.
- The first controller uses HTTP polling rather than the planned WebSocket channel.
- No authentication is enabled; use only on a trusted private LAN.
- Two-PC synchronization, real audio, transitions, full-screen monitor selection, GPU telemetry, masks, warp meshes, and edge blending are not implemented.
- Windows execution still needs testing on the user's actual PC; local tests only prove the current development platform build.

## Next engineering slice

1. Decode and render real still images in Producer and Player.
2. Add FFmpeg video decode with buffered local playback and accurate transport state.
3. Add fullscreen display/monitor assignment and persistent output geometry.
4. Replace controller polling with authenticated WebSocket state updates.
5. Add resumable asset transfer and checksum verification.
