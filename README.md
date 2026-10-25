# viewcam

Real-time webcam viewer written in Rust. Frames are captured through the
operating system's native API (AVFoundation on macOS, V4L2 on Linux, Media
Foundation on Windows) and drawn in a lightweight window.

## Requirements

- Stable Rust (tested with 1.99)
- A webcam
- On macOS, camera permission for the terminal or editor that launches the
  binary (System Settings → Privacy & Security → Camera)

## Build and run

```sh
cargo run --release
```

The first build compiles the dependencies (`nokhwa` + `minifb`) and takes a few
minutes. The development profile already optimizes dependencies (see
`[profile.dev]` in `Cargo.toml`), so `cargo run` also runs at ~30 fps. The final
binary lands in `target/release/viewcam`:

```sh
./target/release/viewcam
```

## Usage

```text
viewcam [OPTIONS] [INDEX]

-l, --list          List the cameras detected by the system and exit
    --formats       List the formats supported by the chosen camera and exit
    --probe         Capture ~20 frames, print stats and a PNG, then exit
    --bench         Run a synthetic test pattern for 5 s and report GUI speed
    --stats         Print per-stage timings every second while running
-i, --index <N>     Camera to open (default: 0)
-n, --name <TEXT>   Open the first camera whose name contains TEXT
    --id <ID>       Open a camera by its system id (see --list)
-W, --width <N>     Preferred capture width (default: 1280)
-H, --height <N>    Preferred capture height (default: 720)
    --fps <N>       Preferred frame rate (default: 30)
-m, --mirror        Start mirrored, like a mirror (default)
    --no-mirror     Start unmirrored
-h, --help          Show this help
```

Examples:

```sh
viewcam --list                     # which cameras exist
viewcam --name face                # the FaceTime HD Camera
viewcam --formats -n face          # what that camera can deliver
viewcam --probe -n face            # measure real fps and write a PNG
viewcam --stats -n face            # capture/decode/convert/present timings
viewcam --bench -W 2560 -H 1440     # GUI only, no camera
viewcam -W 1920 -H 1080             # ask for 1080p
```

## Menu bar and keys

The macOS menu bar exposes the same controls as the keyboard:

| Menu item            | Effect                                            |
| -------------------- | ------------------------------------------------- |
| `View → Mirror: on/off` | Turns the mirror (selfie) effect on or off      |
| `View → Camera → …`  | Switches to another camera while it is running     |
| `View → Freeze`      | Freezes or resumes the image                       |
| `View → Save snapshot` | Writes a PNG to `./snapshots`                    |
| `View → Quit`        | Closes the program                                 |

The current camera is marked with `•` in the `Camera` submenu. The menu bar
belongs to the app, so it shows up when viewcam is the active application (click
the video window if your terminal still owns the menu bar).

| Key          | Effect                    |
| ------------ | ------------------------- |
| `Esc` / `Q`  | Quit                      |
| `M`          | Toggle mirroring          |
| `S`          | Save a PNG in `./snapshots` |
| `Space`      | Freeze / resume the image |

The window title shows resolution, measured fps and the current state (`live`,
`mirrored`, `frozen`). The window can be resized; the image keeps its aspect
ratio.

## How the capture format is chosen

The program asks the camera for its supported formats and picks the one closest
to `--width/--height/--fps`, in this order of preference:

1. never exceed the requested resolution (an accidental 4K stream is too slow);
2. smallest distance to the requested resolution;
3. frame rate ≥ 20 when available;
4. MJPEG before YUYV, and both before NV12/GRAY.

If the camera rejects the selected format, it falls back to the device default
and says so on the console.

This explicit step exists because `nokhwa` 0.10's `RequestedFormatType::Closest`
looks the frame rate up under the *requested* resolution: when that resolution
is not present exactly, the lookup fails and the library silently takes the
first format in its list (sometimes 160x120@25). For the same reason the program
uses `--name`/`--id`: numeric indices shift when a USB camera or a Continuity
iPhone appears or disappears.

## Performance diagnostics

The path of every frame is **capture → decode (YUYV/MJPEG → RGB) → convert to
bitmap → draw**. `--stats` reports the cost of each stage:

```text
fps  30.9 | capture  25.1 ms | decode   5.5 ms | convert  1.7 ms | present  0.9 ms
```

Reference numbers measured on this machine (FaceTime HD, 1280x720@30):

| Stage   | Cost   | Note                                                  |
| ------- | ------ | ----------------------------------------------------- |
| capture | ~25 ms | mostly waiting: the camera delivers 30 fps (33 ms/frame) |
| decode  | ~5 ms  | YUYV → RGB conversion (about 9 ms in a dev build)      |
| convert | ~1.7 ms| bitmap packing plus mirroring                          |
| present | ~1 ms  | OpenGL texture upload (composition is asynchronous)    |

`--bench` measures the GUI half alone, without a camera: 1280x720 → 59 fps,
2560x1440 → 59 fps (capped by the 60 Hz vsync). The window is therefore not the
bottleneck; the realistic ceiling is the camera's own frame rate.

If `capture` is far above `1000/camera_fps`, the camera itself is limited (low
light often drops webcams to 15 fps). If `decode` dominates, the build is not
optimized.

## Troubleshooting

**The image is black.** Camera permission was denied. Run `viewcam --probe`, it
warns when frames are almost black. Allow the application that launches the
binary in System Settings → Privacy & Security → Camera, then start it again.

**"could not open camera … Already in use".** Another program holds the camera
(Zoom, a browser, OBS). Close it.

**Low frame rate.** Run `viewcam --stats` to see where the time goes. If capture
already takes ~66 ms per frame, the camera or the lighting is the limit. If
`decode` dominates, you are on an unoptimized build: `cargo run --release` is
~40% faster in that stage (5 ms vs 9 ms per 720p frame).

**Full screen.** `minifb` 0.29 does not expose full screen; use the green macOS
window button.

## Layout

```text
src/main.rs   CLI, menu bar, format selection, capture loop and drawing
```

Image helpers and format selection are covered by tests:

```sh
cargo test
```
