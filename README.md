# viewcam

Real-time webcam viewer written in Rust. Frames are captured through the
system's native API (AVFoundation on macOS, V4L2 on Linux, Media Foundation on
Windows) and drawn in a lightweight window.

![throughput](https://img.shields.io/badge/capture-~30%20fps%20at%201280x720-green)

## Requirements

- Stable Rust (tested with 1.99)
- A webcam
- On macOS, camera permission for the terminal/editor that runs the binary
  (System Settings → Privacy & Security → Camera)

## Build and run

```sh
cargo run --release
```

The first build compiles the dependencies (`nokhwa` + `minifb`) and takes a few
minutes. The development profile already optimizes dependencies (see
`[profile.dev]` in `Cargo.toml`), so `cargo run` also reaches ~30 fps; the
final binary lands in `target/release/viewcam`:

```sh
./target/release/viewcam
```

## Usage

```text
viewcam [OPTIONS] [INDEX]

-l, --list          List the cameras and exit
    --formats       List the formats supported by the chosen camera and exit
    --probe         Capture ~20 frames, print stats, save a PNG, then exit
    --bench         Run a synthetic pattern for 5 s and measure the GUI only
    --stats         Print per-stage timings every second while running
-i, --index <N>     Camera to open (default: 0)
-n, --name <TEXT>   Open the first camera whose name contains TEXT
    --id <ID>       Open a camera by its system id (see --list)
-W, --width <N>     Preferred capture width (default: 1280)
-H, --height <N>    Preferred capture height (default: 720)
    --fps <N>       Preferred frame rate (default: 30)
-m, --mirror        Start mirrored, like a mirror (default)
    --no-mirror     Start unmirrored
-h, --help          Show help
```

Examples:

```sh
viewcam --list                   # which cameras exist
viewcam --name face              # the FaceTime HD Camera
viewcam --formats -n face        # what that camera can deliver
viewcam --probe -n face          # measure real fps and save snapshots/*.png
viewcam --stats -n face          # capture/decode/convert/present timings
viewcam --bench -W 2560 -H 1440  # measure the window alone, without a camera
viewcam -W 1920 -H 1080          # ask for 1080p
```

## Performance diagnostics

Every frame goes through **capture → decode (YUYV/MJPEG → RGB) → bitmap
conversion → window present**. `--stats` reports the cost of each stage:

```text
fps  30.9 | capture  25.1 ms | decode   5.5 ms | convert  1.7 ms | present  0.9 ms
```

Reference numbers measured on this machine (FaceTime HD, 1280x720@30):

| Stage   | Cost   | Notes                                              |
| ------- | ------ | -------------------------------------------------- |
| capture | ~25 ms | waiting: the camera delivers 30 fps (33 ms/frame)  |
| decode  | ~5 ms  | YUYV → RGB conversion (dev build: ~9 ms)           |
| convert | ~1.7 ms| bitmap packing + mirroring                         |
| present | ~1 ms  | OpenGL texture upload (composition is async)       |

`--bench` measures only the graphics side, without a camera: 1280x720 → 59 fps,
2560x1440 → 59 fps (capped by the 60 Hz vsync). The window is therefore not the
bottleneck at that size; the realistic ceiling is the camera's own rate.

If `capture` sits far above `1000/camera_fps`, the camera is the limit (low
light commonly drops webcams to 15 fps). If `decode` dominates, the build is
running without optimization.

### Keys

| Key        | Action                          |
| ---------- | ------------------------------- |
| `Esc`/`Q`  | Quit                            |
| `M`        | Toggle mirroring                |
| `S`        | Save a PNG into `./snapshots`   |
| `Space`    | Freeze / resume the image       |

The window title shows resolution, measured fps and the current state (`live`,
`mirrored`, `frozen`). The window is resizable and the image keeps its aspect
ratio.

## How the format is chosen

The program asks the camera for its supported formats and picks the closest one
to `--width/--height/--fps`, in this order of preference:

1. never exceed the requested resolution (accidental 4K is too slow);
2. smallest distance to the requested resolution;
3. frame rate ≥ 20 when available;
4. MJPEG before YUYV, and both before NV12/GRAY.

If the camera rejects the chosen format, it falls back to the device default and
says so on stdout.

This explicit step exists because `nokhwa` 0.10's
`RequestedFormatType::Closest` looks the frame rate up under the *requested*
resolution — if that resolution does not exist exactly, the lookup fails and the
library takes the first format in the list (sometimes 160x120@25). That is also
why `--name`/`--id` exist: numeric indices shift when a USB camera or an iPhone
via Continuity Camera appears or disappears.

## Troubleshooting

**The image is black.** Camera permission was denied. Run `viewcam --probe` to
confirm: it warns when frames are nearly black. Allow the app that runs the
binary in System Settings → Privacy & Security → Camera and start the binary
again.

**"could not open camera … Already in use".** Another program holds the camera
(Zoom, a browser, OBS). Close it.

**Low fps.** Run `viewcam --stats` to see the cost of each stage. If capture
itself is slow (say ~66 ms/frame), the camera or the lighting is the limit —
many webcams drop to 15 fps in low light. If `decode` dominates, the build is
unoptimized: `Cargo.toml` already optimizes dependencies in the dev profile, so
`cargo run` reaches ~31 fps, and `cargo run --release` is still ~40% faster at
decoding (5 ms versus 9 ms per 720p frame).

**Fullscreen.** The current `minifb` (0.29) does not expose fullscreen; maximize
the window with the macOS green button.

## Layout

```text
src/main.rs   CLI, format selection, capture loop and drawing
```

Tests for the image helpers and the format selection:

```sh
cargo test
```
