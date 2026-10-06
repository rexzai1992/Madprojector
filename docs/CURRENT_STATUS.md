# Current implementation status

Updated: 2026-10-01

## Implemented and locally verifiable

- Rust workspace with shared model/protocol core and separate Producer/Player binaries.
- Schema v1 model using stable UUIDs for project, scene, asset, layer, and output identity.
- Deterministic JSON serialization, validation, a 32 MiB project-file safety limit, and atomic save replacement.
- SHA-256 media checksum using streaming file reads.
- Projectors with native resolution and an assigned Player PC IP. Adding one places it beside the last at native size without resizing others; a layout calculator fits N projectors to the canvas (e.g. six 1920×1080 on 10400×1080 = 224 px overlaps). New shows start on 10400×1080 with six projectors.
- Each projector can target a numbered physical Windows display. The Player enumerates monitor desktop rectangles and opens a borderless, undecorated output at that display's native position and size; unassigned or unavailable displays safely use preview windows.
- Multi-PC: the show goes to every Player address, each Player draws only its own projectors, and transport commands go to all Players.
- Protocol v2 measures each Player's wall-clock offset at the network round-trip midpoint. Scene/cue/play actions are prepared 1.5 s ahead and carry a per-Player local start timestamp, so command arrival order no longer determines the start frame. Players report scheduled-start error and the Producer reports playback-position spread.
- Identify pattern (big number, name, IP, resolution, border, corner marks) for matching and aligning projectors.
- LAN tools per Player PC: latency, a 64 MB speed test, media check, and streamed media upload into the Player's media cache with SHA-256 verification.
- Changing the canvas size never resizes projectors or media.
- Image, video and music import, dropped on the timeline (from the library or the OS) at a time and track; thumbnails and waveforms.
- Timeline with one track per layer: move clips in time or between tracks, trim either edge, snapping to playhead, cues and clip edges.
- The Player follows a show clock: each clip plays only inside its timeline window; music and optional video sound play through FFmpeg and the system audio device, with per-layer volume.
- Scene end actions: loop, hold last frame, stop, or go to the next scene.
- Loop regions inside a scene: repeat a section forever or N times until released (Enter, a per-loop exit key, the Producer button or the iPad). Each loop has a hotkey and iPad button that jump to its start.
- Seamless loops and scene changes: the next pass is pre-rolled 2 s ahead and swapped in at the jump, so no black frame appears.
- Undo/redo for every show edit (⌘Z / ⇧⌘Z), and copy/paste of layers between scenes.
- Projectors keep their native resolution on the canvas; they can be moved but never stretched.
- Cues: named start points in a scene, each with its own hotkey and iPad button (e.g. "sea"). Scene hotkeys are assignable (default F1–F12); Space plays/pauses, Esc stops, B blackout, M adds a cue.
- Live sync: Producer edits reach the Player within ~120 ms; Producer networking runs on a background worker.
- Player decodes every asset in the prepared scene (images, and FFmpeg video paced at 30 fps) with real pause, stop, seek restart and per-layer looping.
- Edge blending uses a complementary ramp (seam level, curve, projector gamma) so overlapping light sums to 100%; per-projector brightness and black-level lift; the Producer plots the blend and flags unblended overlaps.
- White, 50% gray and stage-space grid test patterns for alignment and blend tuning.
- Four-corner perspective correction uses a projective homography. Corners can be dragged in a calibration preview or entered numerically; media uses a subdivided texture mesh and calibration grids follow the correction.
- Per-projector polygon masks support 3–64 editable output-space points. The Player triangulates the area outside the polygon and blacks it out after rendering.
- Web controller customisable from the Producer: title, note, accent colour, columns, visible sections, and per-scene button text, colour and visibility.
- Show mode: on first start each Player asks whether it is the master or a sub (subs enter the master's IP) and saves the answer. Every Player keeps the last show it received and reopens it on start. The master can autostart the first scene once every sub answers (or after 20 s), relays iPad commands to the subs with a clock-adjusted start 0.8 s ahead (clock offset taken from the quickest of several round trips), and brings a restarted sub back to the same position. Subs copy the show and missing media from the master (SHA-256 checked) and pass iPad taps to it. Sound plays on the master only unless enabled for every PC. Ports can be moved with `MAPFORGE_PORT` and `MAPFORGE_HTTP_PORT` to test two Players on one machine.
- Player TCP listener on port 4777 with protocol v2 envelopes and bounded duplicate-command tracking.
- Browser controller HTTP server on port 8080 with state recovery by polling and scene buttons.
- Transport, volume, mute, and non-destructive blackout state.
- Unit tests for serialization, older-project defaults, blend ramps summing to full light, output arrangement and auto-blend.

## Prototype limitations

- FFmpeg must be installed on the Player PC for video playback; still images work without it.
- Video is decoded on the CPU at up to 1920 px wide and 30 fps, and audio/video sync is wall-clock based rather than frame-accurate. Hardware decode is not implemented yet.
- Scheduled multi-PC starts compensate for measured wall-clock offsets, but there is no continuous drift correction yet. Long scenes can separate as hardware clocks drift, and the current 10 ms scheduler is not frame-lock or genlock.
- The first controller uses HTTP polling rather than the planned WebSocket channel.
- No authentication is enabled; use only on a trusted private LAN.
- Blending is drawn as black-alpha overlays in the simulated windows; a linear-light GPU shader is still planned. Black-level lift is a uniform raise outside the feathers.
- Cross-PC clock sync, transitions, GPU telemetry, multi-point warp meshes and feathered masks are not implemented.
- Polygon masks are currently hard-edged. Perspective texture sampling uses a 16×16 subdivided mesh; a future dedicated GPU render pass should replace this approximation and combine warp, mask, colour and blending.
- Physical display enumeration and borderless placement compile for Windows, but still require validation on the user's actual display topology, GPU and projectors. Mixed-DPI Windows layouts need particular testing.
- Windows execution still needs testing on the user's actual PC; local tests only prove the current development platform build.

## Next engineering slice

1. Add continuous cross-PC drift monitoring and gentle correction.
2. Move warp, masking and blending into a linear-light GPU render pass.
3. Add mask feathering and multi-point mesh warping.
4. Add output FPS and dropped-frame telemetry.
5. Replace controller polling with authenticated WebSocket state updates.
