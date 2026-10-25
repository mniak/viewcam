//! viewcam — live webcam preview.
//!
//! Frames are captured through the OS native backend (AVFoundation on macOS,
//! V4L2 on Linux, Media Foundation on Windows) and drawn with `minifb`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use minifb::{Key, KeyRepeat, ScaleMode, Window, WindowOptions};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::query;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::Camera;

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_FPS: u32 = 30;

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

struct Args {
    index: Option<u32>,
    name: Option<String>,
    id: Option<String>,
    list: bool,
    probe: bool,
    formats: bool,
    mirror: bool,
    width: u32,
    height: u32,
    fps: u32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            index: None,
            name: None,
            id: None,
            list: false,
            probe: false,
            formats: false,
            mirror: true,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
        }
    }
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args::default();
        let mut it = std::env::args().skip(1);

        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-l" | "--list" => args.list = true,
                "--probe" => args.probe = true,
                "--formats" => args.formats = true,
                "-m" | "--mirror" => args.mirror = true,
                "--no-mirror" => args.mirror = false,
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "-i" | "--index" => {
                    let value = it.next().ok_or_else(|| anyhow!("--index needs a value"))?;
                    args.index = Some(value.parse().context("--index must be a number")?);
                }
                "-n" | "--name" => {
                    args.name = Some(it.next().ok_or_else(|| anyhow!("--name needs a value"))?);
                }
                "--id" => {
                    args.id = Some(it.next().ok_or_else(|| anyhow!("--id needs a value"))?);
                }
                "-W" | "--width" => {
                    let value = it.next().ok_or_else(|| anyhow!("--width needs a value"))?;
                    args.width = value.parse().context("--width must be a number")?;
                }
                "-H" | "--height" => {
                    let value = it.next().ok_or_else(|| anyhow!("--height needs a value"))?;
                    args.height = value.parse().context("--height must be a number")?;
                }
                "--fps" => {
                    let value = it.next().ok_or_else(|| anyhow!("--fps needs a value"))?;
                    args.fps = value.parse().context("--fps must be a number")?;
                }
                other => {
                    // A bare number means camera index: `viewcam 1`.
                    if let Ok(index) = other.parse::<u32>() {
                        args.index = Some(index);
                    } else {
                        print_usage();
                        bail!("unknown argument: {other}");
                    }
                }
            }
        }

        if args.width == 0 || args.height == 0 {
            bail!("width and height must be greater than zero");
        }
        if args.fps == 0 {
            bail!("--fps must be greater than zero");
        }

        Ok(args)
    }
}

fn print_usage() {
    println!(
        "\
viewcam — live webcam preview

USAGE:
    viewcam [OPTIONS] [INDEX]

OPTIONS:
    -l, --list          List the cameras detected by the system and exit
        --formats       List the formats supported by the chosen camera and exit
        --probe         Capture ~20 frames, print stats and a PNG, then exit
    -i, --index <N>     Camera to open (default: 0)
    -n, --name <TEXT>   Open the first camera whose name contains TEXT
        --id <ID>       Open a camera by its system id (see --list)
    -W, --width <N>     Preferred capture width (default: {DEFAULT_WIDTH})
    -H, --height <N>    Preferred capture height (default: {DEFAULT_HEIGHT})
        --fps <N>       Preferred frame rate (default: {DEFAULT_FPS})
    -m, --mirror        Start mirrored, like a mirror (default)
        --no-mirror     Start unmirrored
    -h, --help          Show this help

KEYS (while the window has focus):
    Esc / Q             Quit
    M                   Toggle mirror
    S                   Save a PNG snapshot to ./snapshots
    Space               Freeze / resume the image
"
    );
}

// ---------------------------------------------------------------------------
// Camera discovery
// ---------------------------------------------------------------------------

fn list_cameras() -> Result<()> {
    let mut cameras = query(ApiBackend::Auto).context("could not query the camera backend")?;
    cameras.sort_by_key(|info| match info.index() {
        CameraIndex::Index(i) => *i,
        CameraIndex::String(_) => u32::MAX,
    });

    if cameras.is_empty() {
        println!("No camera was found.");
        return Ok(());
    }

    println!("{} camera(s) found:\n", cameras.len());
    for (position, info) in cameras.iter().enumerate() {
        let name = info.human_name();
        let description = info.description().trim();
        let detail = if description.is_empty() {
            info.misc().to_string()
        } else {
            format!("{} · {}", info.misc(), description)
        };
        println!("  [{position}] {name}");
        println!("       {detail}");
    }
    println!("\nOpen one with: viewcam --index <N>, viewcam --name <TEXT> or viewcam --id <ID>");

    Ok(())
}

/// Turn `--index` / `--name` / `--id` into a stable [`CameraIndex`].
///
/// Numeric indices shift when devices appear or disappear (a Continuity iPhone
/// camera, a USB camera being plugged), so the system id is used whenever we
/// can resolve one.
fn resolve_target(args: &Args) -> Result<CameraIndex> {
    let cameras = query(ApiBackend::Auto).context("could not query the camera backend")?;

    if let Some(id) = &args.id {
        let found = cameras.iter().any(|info| &info.misc() == id);
        if !found {
            bail!("no camera with id {id:?}; run `viewcam --list` to see the ids");
        }
        return Ok(CameraIndex::String(id.clone()));
    }

    if cameras.is_empty() {
        bail!(
            "no camera was found — check that it is connected and that the \
             application running viewcam may use the camera"
        );
    }

    if let Some(name) = &args.name {
        let needle = name.to_lowercase();
        let matches = cameras
            .iter()
            .filter(|info| info.human_name().to_lowercase().contains(&needle))
            .collect::<Vec<_>>();

        return match matches.as_slice() {
            [] => bail!(
                "no camera name contains {name:?}; available: {}",
                camera_names(&cameras)
            ),
            [only] => Ok(CameraIndex::String(only.misc().to_string())),
            many => bail!(
                "{} cameras match {name:?}: {} — use --index",
                many.len(),
                camera_names(&cameras)
            ),
        };
    }

    let wanted = args.index.unwrap_or(0) as usize;
    cameras.get(wanted).map_or_else(
        || {
            bail!(
                "camera index {wanted} does not exist ({} camera(s) found: {})",
                cameras.len(),
                camera_names(&cameras)
            )
        },
        |info| Ok(CameraIndex::String(info.misc().to_string())),
    )
}

fn camera_names(cameras: &[nokhwa::utils::CameraInfo]) -> String {
    cameras
        .iter()
        .map(|info| info.human_name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every `(resolution, pixel format, frame rate)` triple the device offers.
#[cfg(target_os = "macos")]
fn device_formats(index: &CameraIndex) -> Result<Vec<CameraFormat>> {
    let device = nokhwa_bindings_macos::AVCaptureDevice::new(index)
        .context("could not open the AVFoundation device")?;
    device
        .supported_formats()
        .context("could not read the camera formats")
}

#[cfg(not(target_os = "macos"))]
fn device_formats(index: &CameraIndex) -> Result<Vec<CameraFormat>> {
    let mut camera = Camera::new(
        index.clone(),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::None),
    )
    .context("could not open the camera to query its formats")?;
    camera
        .compatible_camera_formats()
        .context("could not read the camera formats")
}

/// Lower is better: MJPEG is cheap to move, YUYV is cheap to decode.
fn format_rank(format: FrameFormat) -> u8 {
    match format {
        FrameFormat::MJPEG => 0,
        FrameFormat::YUYV => 1,
        FrameFormat::RAWRGB => 2,
        FrameFormat::NV12 => 3,
        FrameFormat::GRAY => 4,
        FrameFormat::RAWBGR => 9,
    }
}

/// Pick the format closest to what the user asked for, preferring formats we
/// can decode to RGB and frame rates that actually feel live.
fn pick_format(formats: &[CameraFormat], args: &Args) -> Option<CameraFormat> {
    let (want_w, want_h) = (i64::from(args.width), i64::from(args.height));

    formats
        .iter()
        .copied()
        .filter(|f| f.width() > 0 && f.height() > 0 && f.frame_rate() > 0)
        .filter(|f| format_rank(f.format()) < 9)
        .min_by_key(|f| {
            let (w, h) = (i64::from(f.width()), i64::from(f.height()));
            (
                // Do not silently go above the requested size (4K is slow).
                i64::from(w > want_w || h > want_h),
                (w - want_w).abs() + (h - want_h).abs(),
                i64::from(f.frame_rate() < 20),
                std::cmp::Reverse(f.frame_rate()),
                format_rank(f.format()),
            )
        })
}

fn print_formats(target: &CameraIndex, _args: &Args) -> Result<()> {
    let formats = device_formats(target)?;

    let mut lines = BTreeSet::new();
    for format in &formats {
        lines.insert((
            format_rank(format.format()),
            std::cmp::Reverse(format.width() * format.height()),
            format.frame_rate(),
            format.to_string(),
        ));
    }

    println!("Camera {target} supports {} format(s):\n", lines.len());
    for (_, _, _, line) in &lines {
        println!("  {line}");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Camera capture
// ---------------------------------------------------------------------------

fn try_open(index: CameraIndex, requested: RequestedFormat<'_>) -> Result<Camera> {
    let mut camera = Camera::new(index, requested).map_err(|e| anyhow!(e))?;
    camera.open_stream().map_err(|e| anyhow!(e))?;
    Ok(camera)
}

/// Open the camera with an explicitly chosen format.
///
/// `RequestedFormatType::Closest` is not used: nokhwa looks the frame rate up
/// under the *requested* resolution, so it fails whenever that exact
/// resolution is absent and silently degrades to the device default.
fn open_camera(target: &CameraIndex, args: &Args) -> Result<Camera> {
    let mut errors: Vec<String> = Vec::new();

    let chosen = match device_formats(target) {
        Ok(formats) => pick_format(&formats, args),
        Err(err) => {
            eprintln!("warning: could not enumerate camera formats: {err:#}");
            None
        }
    };

    if let Some(chosen) = chosen {
        let formats = [chosen.format()];
        let requested = RequestedFormat::with_formats(RequestedFormatType::Exact(chosen), &formats);
        match try_open(target.clone(), requested) {
            Ok(camera) => return Ok(camera),
            Err(err) => errors.push(format!("{chosen}: {err}")),
        }
    }

    // Fallback: let the backend hand us whatever it considers default.
    match try_open(
        target.clone(),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::None),
    ) {
        Ok(camera) => Ok(camera),
        Err(err) => {
            errors.push(format!("device default: {err}"));
            bail!(
                "could not open camera {target}\n  {}\n\
                 Check the camera permissions (macOS: System Settings → Privacy & Security → Camera)\n\
                 and whether another application is already using it.",
                errors.join("\n  ")
            )
        }
    }
}

fn describe(camera: &Camera) -> String {
    let format = camera.camera_format();
    format!(
        "{} — {}x{} @{} fps ({})",
        camera.info().human_name(),
        format.width(),
        format.height(),
        format.frame_rate(),
        format.format()
    )
}

/// Open the camera, capture a handful of frames, report what happened and save
/// one PNG. Useful to check permissions and real throughput without a window.
fn probe_camera(target: &CameraIndex, args: &Args) -> Result<()> {
    let mut camera = open_camera(target, args)?;
    println!("Negotiated format: {}", describe(&camera));

    const FRAMES: u32 = 20;
    let mut frames = 0u32;
    let mut luma_sum = 0f64;
    let mut capture_time = Duration::ZERO;
    let mut decode_time = Duration::ZERO;
    let mut last_rgb: Vec<u8> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);

    let started = Instant::now();
    for _ in 0..FRAMES {
        let t0 = Instant::now();
        let frame = camera.frame().context("capture failed")?;
        capture_time += t0.elapsed();

        let t1 = Instant::now();
        let decoded = frame.decode_image::<RgbFormat>().context("decode failed")?;
        decode_time += t1.elapsed();

        width = decoded.width() as usize;
        height = decoded.height() as usize;
        last_rgb = decoded.into_raw();

        if !last_rgb.is_empty() {
            // Sample every ~64th byte: cheap brightness estimate.
            let step = 64;
            let sum: u64 = last_rgb.iter().step_by(step).map(|b| u64::from(*b)).sum();
            let count = last_rgb.len().div_ceil(step) as f64;
            luma_sum += sum as f64 / count;
            frames += 1;
        }
    }
    let elapsed = started.elapsed().as_secs_f32();

    println!(
        "{frames} frame(s) in {elapsed:.2}s = {:.1} fps",
        frames as f32 / elapsed.max(0.001)
    );
    if frames > 0 {
        println!(
            "per frame: capture {:.1} ms, decode {:.1} ms, brightness {:.0}/255",
            capture_time.as_secs_f64() * 1000.0 / f64::from(frames),
            decode_time.as_secs_f64() * 1000.0 / f64::from(frames),
            luma_sum / f64::from(frames),
        );
    }
    if luma_sum / f64::from(frames.max(1)) < 8.0 {
        println!(
            "warning: frames are almost black — the room may be dark, or the camera \
             may be blocked by System Settings → Privacy & Security → Camera"
        );
    }

    if !last_rgb.is_empty() {
        let path = save_snapshot(&last_rgb, width, height)?;
        println!("Saved {} ({}x{})", path.display(), width, height);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Image helpers
// ---------------------------------------------------------------------------

/// Convert packed RGB bytes into minifb's `0RGB` u32 pixels, optionally
/// flipping each row horizontally (mirror mode).
fn blit_rgb_to_pixels(rgb: &[u8], pixels: &mut [u32], width: usize, mirror: bool) {
    if width == 0 {
        return;
    }
    let height = (pixels.len() / width).min(rgb.len() / (width * 3));

    for y in 0..height {
        let row = &rgb[y * width * 3..(y + 1) * width * 3];
        let out = &mut pixels[y * width..(y + 1) * width];

        if mirror {
            for (x, dst) in out.iter_mut().enumerate() {
                let src = (width - 1 - x) * 3;
                *dst =
                    ((row[src] as u32) << 16) | ((row[src + 1] as u32) << 8) | row[src + 2] as u32;
            }
        } else {
            for (dst, px) in out.iter_mut().zip(row.as_chunks::<3>().0) {
                *dst = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | px[2] as u32;
            }
        }
    }
}

fn pixels_to_rgb(pixels: &[u32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(pixels.len() * 3);
    for px in pixels {
        out.push((px >> 16) as u8);
        out.push((px >> 8) as u8);
        out.push(*px as u8);
    }
}

fn snapshot_path(name: Option<&str>) -> Result<PathBuf> {
    let dir = PathBuf::from("snapshots");
    fs::create_dir_all(&dir).context("could not create ./snapshots")?;
    let file = match name {
        Some(name) => format!("{name}.png"),
        None => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("viewcam-{stamp}.png")
        }
    };
    Ok(dir.join(file))
}

fn save_snapshot(rgb: &[u8], width: usize, height: usize) -> Result<PathBuf> {
    save_snapshot_as(rgb, width, height, None)
}

fn save_snapshot_as(
    rgb: &[u8],
    width: usize,
    height: usize,
    name: Option<&str>,
) -> Result<PathBuf> {
    let path = snapshot_path(name)?;
    image::save_buffer(
        &path,
        rgb,
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgb8,
    )
    .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse()?;

    if args.list {
        return list_cameras();
    }

    let target = resolve_target(&args)?;

    if args.formats {
        return print_formats(&target, &args);
    }
    if args.probe {
        return probe_camera(&target, &args);
    }

    let mut camera = open_camera(&target, &args)?;
    let format = camera.camera_format();
    let mut width = format.width() as usize;
    let mut height = format.height() as usize;

    println!("Camera {target} open — {}", describe(&camera));
    if width as u32 != args.width || height as u32 != args.height {
        println!(
            "note: requested {}x{}, the camera gave {}x{}",
            args.width, args.height, width, height
        );
    }
    println!("Keys: Esc/Q quit · M mirror · S snapshot · Space freeze");

    let mut window = Window::new(
        "viewcam",
        width.max(320),
        height.max(240),
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::AspectRatioStretch,
            ..WindowOptions::default()
        },
    )
    .context("could not create the window")?;
    window.set_target_fps(0);

    let mut mirror = args.mirror;
    let mut frozen = false;
    let mut pixels: Vec<u32> = vec![0; width * height];
    let mut snapshot_rgb: Vec<u8> = Vec::new();
    let mut have_frame = false;
    let mut redraw = true;
    let mut consecutive_errors = 0u32;
    let mut measured_fps = 0f32;

    while window.is_open() && !window.is_key_down(Key::Escape) {
        if window.is_key_pressed(Key::Q, KeyRepeat::No) {
            break;
        }
        if window.is_key_pressed(Key::M, KeyRepeat::No) {
            mirror = !mirror;
            redraw = true;
        }
        if window.is_key_pressed(Key::Space, KeyRepeat::No) {
            frozen = !frozen;
            println!("{}", if frozen { "Frozen" } else { "Live" });
        }

        if !frozen {
            let started = Instant::now();
            match camera.frame() {
                Ok(frame) => match frame.decode_image::<RgbFormat>() {
                    Ok(decoded) => {
                        consecutive_errors = 0;
                        let (w, h) = (decoded.width() as usize, decoded.height() as usize);

                        if (w, h) != (width, height) {
                            width = w;
                            height = h;
                            pixels = vec![0; width * height];
                        }

                        let rgb = decoded.into_raw();
                        blit_rgb_to_pixels(&rgb, &mut pixels, width, mirror);
                        snapshot_rgb = rgb;
                        have_frame = true;
                        redraw = true;

                        let elapsed = started.elapsed().as_secs_f32();
                        if elapsed > 0.0 {
                            let instant = 1.0 / elapsed;
                            measured_fps = if measured_fps == 0.0 {
                                instant
                            } else {
                                measured_fps * 0.9 + instant * 0.1
                            };
                        }
                    }
                    Err(err) => {
                        consecutive_errors += 1;
                        eprintln!("decode error: {err}");
                    }
                },
                Err(err) => {
                    consecutive_errors += 1;
                    eprintln!("capture error: {err}");
                    std::thread::sleep(Duration::from_millis(30));
                }
            }

            if consecutive_errors > 100 {
                bail!("camera stopped delivering frames ({consecutive_errors} failures in a row)");
            }
        }

        if window.is_key_pressed(Key::S, KeyRepeat::No) && have_frame {
            // Save what is on screen, so mirror mode is honoured.
            pixels_to_rgb(&pixels, &mut snapshot_rgb);
            match save_snapshot(&snapshot_rgb, width, height) {
                Ok(path) => println!("Snapshot saved to {}", path.display()),
                Err(err) => eprintln!("snapshot failed: {err:#}"),
            }
        }

        if redraw && have_frame {
            window
                .update_with_buffer(&pixels, width, height)
                .context("could not update the window")?;
            redraw = false;

            window.set_title(&format!(
                "viewcam — {width}x{height} — {measured_fps:.0} fps — {}",
                if frozen {
                    "frozen"
                } else if mirror {
                    "mirrored"
                } else {
                    "live"
                }
            ));
        } else {
            // No new frame (frozen or waiting): keep window events flowing.
            window.update();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nokhwa::utils::Resolution;

    fn args_for(width: u32, height: u32) -> Args {
        Args {
            width,
            height,
            ..Args::default()
        }
    }

    #[test]
    fn blit_packs_pixels_as_0rgb() {
        let rgb = vec![1, 2, 3, 4, 5, 6];
        let mut pixels = vec![0u32; 2];
        blit_rgb_to_pixels(&rgb, &mut pixels, 2, false);
        assert_eq!(pixels, vec![0x0001_0203, 0x0004_0506]);
    }

    #[test]
    fn blit_mirrors_each_row_separately() {
        // 2x2 image: (1,2,3) (4,5,6) / (7,8,9) (10,11,12)
        let rgb = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut pixels = vec![0u32; 4];
        blit_rgb_to_pixels(&rgb, &mut pixels, 2, true);
        assert_eq!(
            pixels,
            vec![0x0004_0506, 0x0001_0203, 0x000a_0b0c, 0x0007_0809]
        );
    }

    #[test]
    fn pixels_round_trip_to_rgb() {
        let pixels = vec![0x0012_3456, 0x00ff_ffff, 0];
        let mut rgb = Vec::new();
        pixels_to_rgb(&pixels, &mut rgb);
        assert_eq!(rgb, vec![0x12, 0x34, 0x56, 0xff, 0xff, 0xff, 0, 0, 0]);
    }

    #[test]
    fn pick_format_takes_the_requested_resolution() {
        let formats = vec![
            CameraFormat::new(Resolution::new(640, 480), FrameFormat::YUYV, 30),
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::YUYV, 30),
            CameraFormat::new(Resolution::new(1920, 1080), FrameFormat::YUYV, 30),
        ];
        let picked = pick_format(&formats, &args_for(1280, 720)).expect("a format");
        assert_eq!((picked.width(), picked.height()), (1280, 720));
    }

    #[test]
    fn pick_format_stays_below_the_requested_size() {
        let formats = vec![
            CameraFormat::new(Resolution::new(1920, 1080), FrameFormat::YUYV, 30),
            CameraFormat::new(Resolution::new(640, 480), FrameFormat::YUYV, 30),
        ];
        let picked = pick_format(&formats, &args_for(1280, 720)).expect("a format");
        assert_eq!((picked.width(), picked.height()), (640, 480));
    }

    #[test]
    fn pick_format_prefers_decodable_pixel_formats() {
        let formats = vec![
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::NV12, 30),
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::MJPEG, 30),
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::YUYV, 30),
        ];
        let picked = pick_format(&formats, &args_for(1280, 720)).expect("a format");
        assert_eq!(picked.format(), FrameFormat::MJPEG);
    }

    #[test]
    fn pick_format_ignores_unusable_entries() {
        let formats = vec![
            CameraFormat::new(Resolution::new(1280, 720), FrameFormat::YUYV, 0),
            CameraFormat::new(Resolution::new(0, 0), FrameFormat::YUYV, 30),
        ];
        assert!(pick_format(&formats, &args_for(1280, 720)).is_none());
    }
}
