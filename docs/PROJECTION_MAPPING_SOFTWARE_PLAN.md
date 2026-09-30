# Projection Mapping Software Plan

Status: Initial saved plan
Working name: MapForge Studio
Target platform: Windows first

## Confirmed Hardware Setup

- 1 control laptop running the Producer application.
- 2 playback PCs running the Player application.
- Multiple projectors connected to the playback PCs.
- 1 iPad used as the wireless live-show controller.
- The laptop and playback PCs use wired Ethernet through a network switch.
- The iPad connects through Wi-Fi on the same private network.

## System Layout

```text
iPad Controller (Wi-Fi)
          |
          v
Control Laptop / Producer
          |
          | Dedicated wired Ethernet
          |
          +---- Playback PC 1 ---- Projectors 1, 2, 3...
          |
          +---- Playback PC 2 ---- Projectors 4, 5, 6...
```

The playback PCs keep local copies of all show media. The control laptop sends commands and timing information; it does not stream the main video to the players during a show.

## Application Roles

### Producer on the control laptop

- Create, edit, save, and load shows.
- Build scenes using video, images, audio, text, and layers.
- Arrange media on a large virtual stage.
- Create timelines, cues, loops, and transitions.
- Assign projector outputs to either playback PC.
- Configure projector warping, masks, and edge blending.
- Distribute and verify media files on both playback PCs.
- Monitor player connections, frame rate, dropped frames, storage, and GPU status.
- Control play, pause, stop, scene changes, volume, mute, and blackout.

### Player on both playback PCs

- Start automatically with Windows if enabled.
- Receive show projects and media from the Producer.
- Cache all required media locally.
- Decode and render media using the GPU.
- Drive multiple projector outputs.
- Maintain synchronized playback with the other Player.
- Continue the active scene if the laptop or iPad disconnects.
- Reconnect automatically when the network becomes available.

## Content Sizing and Projector Routing

Every scene layer must be independently adjustable without modifying the original media file:

- Set exact width and height in pixels.
- Resize proportionally or unlock the aspect ratio.
- Move, rotate, flip, crop, and zoom content.
- Fit, fill, stretch, or show at original size.
- Use alignment guides, snapping, and numeric position controls.
- Adjust opacity, colour, brightness, and playback speed.
- Span one video across the complete virtual stage.
- Send a layer to one projector, selected projectors, one playback PC, or every projector.
- Show different videos on different projectors within the same scene.
- Crop different regions of one large video for different projectors.
- Duplicate a layer across several projectors while keeping playback synchronized.
- Preview projector boundaries and identify every physical projector by name and number.
- Save projector groups such as Left Wall, Centre, Right Wall, Floor, or All Projectors.

Example routing:

```text
Projector 1 -> Left section of panoramic video
Projector 2 -> Centre-left section
Projector 3 -> Centre-right section
Projector 4 -> Right section
Projector 5 -> Separate logo or presentation
Projector 6 -> Blackout or another scene
```

The Producer uses one large virtual canvas. Each projector receives only the region assigned to it, including its warp, mask, colour correction, and edge-blending adjustments.

## Long-Duration and Large Video Files

The software must support videos that are both long in duration and large in storage size without loading the entire file into memory.

- Stream video from the local NVMe or SSD using buffered reads.
- Use GPU hardware decoding when the codec and GPU support it.
- Maintain a configurable read-ahead buffer for uninterrupted playback.
- Preload only the opening section and decoder state before a scene starts.
- Support accurate seeking, pause, resume, looping, and timeline markers.
- Generate lightweight proxy files and thumbnails for smooth editing on the laptop.
- Use the original full-quality file on the playback PCs during the show.
- Copy large media to both Players before show time, with progress and remaining-time display.
- Resume interrupted media transfers instead of starting again.
- Verify transferred files with checksums before marking a Player ready.
- Warn when disk speed, free space, codec, bitrate, or resolution may cause dropped frames.
- Allow a media file to be shared by multiple projector regions without keeping unnecessary duplicate copies on the same Player.
- Keep playback running from local Player storage; never depend on Wi-Fi or internet streaming.
- Support optimized show-media conversion when an original file is unsuitable for reliable multi-output playback.

Initial target formats should include H.264, H.265/HEVC, HAP, ProRes, PNG, JPEG, and WAV. Final codec and resolution limits will be set after testing the GPUs and storage in both playback PCs.

### iPad controller

The first iPad version will be an installable web app, avoiding an App Store release during early development.

- Scene buttons with names, colours, and preview thumbnails.
- Play, pause, stop, restart, previous scene, and next scene.
- Go button to trigger the next prepared cue.
- Master volume slider.
- Per-scene or per-audio-bus volume controls.
- Mute, fade-out, and fade-in.
- Blackout and restore-output controls.
- Lock-screen control to prevent accidental scene changes.
- Confirmation for emergency stop and destructive show controls.
- Current scene, timeline position, and remaining-time display.
- Preview of the active scene and the scene queued next.
- Online/offline state for the laptop, players, and projectors.
- Warning display for missing media, playback errors, or an out-of-sync Player.
- Custom operator layouts.
- Operator PIN and administrator access.
- Live two-way updates using WebSocket.

## Scene System

Each scene may contain:

- Video, image, audio, text, and live-input layers.
- Layer position, size, rotation, crop, opacity, and colour settings.
- Projector/output assignments.
- Per-layer size, crop, position, and selected-projector routing.
- Warp meshes, masks, and edge-blending settings.
- Master and per-layer volume.
- Cut, crossfade, fade-to-black, or timed transitions.
- Loop, hold, auto-advance, or manual-trigger behaviour.
- Keyboard, iPad, MIDI, OSC, DMX, or API triggers.

The next scene should be preloaded to prevent loading delays or black frames during transitions.

## Edge Blending and Mapping

Every projector output should support:

- Four-corner correction.
- Multi-point mesh warping.
- Polygon masks and soft feathered masks.
- Adjustable overlap on every edge.
- Blend-width and blend-curve controls.
- Gamma, brightness, contrast, and black-level compensation.
- Individual red, green, and blue correction.
- Calibration grids and test patterns.
- Saved calibration profiles for each venue and projector.
- Fine calibration control from the iPad while standing near the projection surface.

## Multi-PC Synchronization

Initial synchronization will use a shared high-resolution network clock:

- The Producer schedules a scene to start at a future shared timestamp.
- Both Players preload the required media before acknowledging readiness.
- Both Players start at the scheduled timestamp.
- Playback clocks are monitored and gently corrected for drift.
- The Producer reports late, disconnected, or out-of-sync Players.

Future professional support may add PTP, LTC, genlock, and frame-lock hardware.

## Audio

- One selected machine acts as the audio master.
- Master, scene, layer, and bus volume controls.
- Smooth fades without clicks.
- Mute, solo, and level meters.
- Configurable audio output device.
- Audio follows scene transitions.
- Volume changes made from the laptop or iPad remain synchronized.

## Reliability Requirements

- Players continue operating if the Producer or iPad disconnects.
- Reconnected controllers recover the current show state.
- No normal show playback depends on internet access.
- Projects autosave and recover after an application or power failure.
- Media files use checksums to confirm both Players have identical copies.
- A local emergency blackout remains available on every machine.
- Logs record commands, connection failures, missing media, and playback errors.
- Optional automatic show launch after Windows starts.

## Development Stages

### Stage 1: Playback prototype

- Smooth video playback on one PC.
- Multiple full-screen GPU outputs.
- Basic image and video layers.
- Basic scene switching and volume control.

### Stage 2: Producer and project editor

- Stage canvas, asset library, scenes, timeline, and project saving.
- Content sizing, cropping, projector grouping, output assignment, and test patterns.
- Cut and crossfade transitions.

### Stage 3: iPad controller

- Installable controller web app.
- Scene, transport, volume, mute, and blackout controls.
- Real-time status and customizable button layouts.

### Stage 4: Projection tools

- Four-corner correction, mesh warping, masks, calibration grids, and edge blending.
- Venue and projector calibration profiles.

### Stage 5: Two-PC playback

- Automatic Player discovery.
- Resumable large-file distribution, local caching, and checksum verification.
- Preloading and synchronized start commands.
- Drift monitoring and correction.

### Stage 6: Production reliability

- Health monitoring, automatic recovery, startup mode, logs, and failure testing.
- Testing with the real laptop, both playback PCs, iPad, and projectors.

### Later stages

- 3D model import and 3D projection mapping.
- Camera-assisted calibration.
- NDI and capture-card inputs.
- MIDI, OSC, DMX, Art-Net, Stream Deck, and automation plugins.
- Hardware genlock/frame-lock and redundant backup control.

## First Release Target

The first usable release will target:

- 1 control laptop.
- 2 playback PCs.
- Multiple projector outputs across both PCs.
- 1 iPad live-show controller.
- 2D projection mapping.
- Scenes, timeline, video, images, and audio.
- Flexible sizing, cropping, and routing of every layer to selected projectors.
- Reliable local playback of long-duration and large video files.
- Scene selection, Go, playback, volume, mute, fade, blackout, and system status from the iPad.
- Warping, masking, and manual edge blending.
- Wired multi-PC synchronization.
- Offline operation and recovery after disconnection.

## Decisions Still Needed Before Implementation

- Exact Windows versions and hardware specifications of all three computers.
- GPU model and number of video outputs on each playback PC.
- Number, resolution, refresh rate, and connection type of the projectors.
- Maximum video resolution and preferred media formats.
- Whether one playback PC or the laptop will produce the final audio output.
- Whether the first installation is a flat panoramic surface, room, stage, or building facade.

This document is the continuing source of truth for the initial product scope. New requirements should be added here before implementation expands.
