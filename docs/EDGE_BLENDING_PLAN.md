# Edge blending and real-media implementation plan

## Implemented simulation slice

MapForge now treats two projector outputs as overlapping views of one virtual stage. The default 1920×1080 stage uses:

- Projector 1: stage X 0–1020, with a 120 px right feather.
- Projector 2: stage X 900–1920, with a 120 px left feather.
- Shared overlap: stage X 900–1020.

The Player decodes image pixels directly and video frames through FFmpeg's bounded RGBA pipe. Each simulated projector crops its own stage region from the same decoded texture, so one story/video continues across both outputs. The complete source movie is never loaded into memory.

Each output saves independent left, right, top, and bottom feather widths. Where two projectors overlap their light adds, so without a blend the overlap is roughly twice as bright. Each feather follows Paul Bourke's complementary ramp: light = a·(2t)^p for the outer half and 1 − (1 − a)·(2(1 − t))^p for the inner half, then pixel = light^(1/γ). With seam level a = 50%, two opposing ramps always sum to 100% light. The Player draws each ramp as one gradient mesh.

The Producer's **Edit projectors** mode shows blend widths as a percentage of the projector, the seam level, curve power p, projector gamma γ, per-projector brightness and black-level lift. It plots both ramps and their sum, marks overlaps that are not blended in red, and can auto-blend every overlap from the projector positions. Grid, white and 50% gray test patterns can be sent to the Player from the same panel.

## Physical calibration workflow

1. Assign and fullscreen each output on the correct Windows display.
2. Project a grid and align geometry before touching blend values.
3. Measure the real overlap and enter that width for the opposing edges.
4. Adjust both feather widths together so their ramps occupy the same physical region (or use **Auto-blend**).
5. With the white and 50% gray patterns, adjust seam level and projector gamma until the overlap matches the rest of the image.
6. Match projector brightness, contrast, RGB, and gamma.
7. On a black image, raise **Black lift** until the non-overlap area matches the brighter overlap; optical projector black remains additive and cannot be corrected by a fade alone.
8. Save calibration by venue and projector identity.

## Required production work

- Replace the CPU-fed preview texture with the planned hardware-decoder/renderer abstraction.
- Preserve frame timestamps and implement true pause, seek, loop, and audio-clock synchronization.
- Add monitor enumeration, borderless fullscreen output, corner pinning, mesh warp, masks, and calibration patterns.
- Blend in linear-light shader space and add per-output colour/black-level correction.
- Measure dropped frames and cross-PC drift on the real Windows GPUs and projectors.

The current blend is visually useful for development and content layout, but it is not a claim of calibrated physical-projector output.
