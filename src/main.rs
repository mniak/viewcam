//! viewcam — live webcam preview.
//!
//! Frames are captured through the OS native backend (AVFoundation on macOS,
//! V4L2 on Linux, Media Foundation on Windows) and drawn with `minifb`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use minifb::{Key, KeyRepeat, Menu, MenuHandle, ScaleMode, Window, WindowOptions};
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
    stats: bool,
    bench: bool,
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
            stats: false,
            bench: false,
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
                "--stats" => args.stats = true,
                "--bench" => args.bench = true,
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
        --bench         Run a synthetic test pattern for 5 s and report GUI speed
        --stats         Print per-stage timings every second while running
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

MENU BAR:
    View → Mirror       Turn the mirror effect on or off
    View → Camera       Switch to another camera while running
    View → Freeze / Save snapshot / Quit
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

/// A camera as offered by the backend: display name plus the stable system id.
struct CameraEntry {
    name: String,
    id: String,
}

/// Cameras in the order the backend reports them; empty if the query fails.
fn camera_entries() -> Vec<CameraEntry> {
    query(ApiBackend::Auto)
        .map(|cameras| {
            cameras
                .into_iter()
                .map(|info| CameraEntry {
                    name: info.human_name(),
                    id: info.misc(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Turn `--index` / `--name` / `--id` into a position in `entries`.
///
/// Numeric indices shift when devices appear or disappear (a Continuity iPhone
/// camera, a USB camera being plugged), so the system id is what we actually
/// open with.
fn resolve_selection(args: &Args, entries: &[CameraEntry]) -> Result<usize> {
    if let Some(id) = &args.id {
        return entries
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or_else(|| {
                anyhow!("no camera with id {id:?}; run `viewcam --list` to see the ids")
            });
    }

    if entries.is_empty() {
        bail!(
            "no camera was found — check that it is connected and that the \
             application running viewcam may use the camera"
        );
    }

    if let Some(name) = &args.name {
        let needle = name.to_lowercase();
        let matches = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&needle))
            .map(|(position, _)| position)
            .collect::<Vec<_>>();

        return match matches.as_slice() {
            [] => bail!(
                "no camera name contains {name:?}; available: {}",
                camera_names(entries)
            ),
            [only] => Ok(*only),
            many => bail!(
                "{} cameras match {name:?}: {} — use --index",
                many.len(),
                camera_names(entries)
            ),
        };
    }

    let wanted = args.index.unwrap_or(0) as usize;
    if wanted >= entries.len() {
        bail!(
            "camera index {wanted} does not exist ({} camera(s) found: {})",
            entries.len(),
            camera_names(entries)
        );
    }
    Ok(wanted)
}

fn camera_names(entries: &[CameraEntry]) -> String {
    entries
        .iter()
        .map(|entry| entry.name.as_str())
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
// Diagnostics
// ---------------------------------------------------------------------------

/// Time spent in each stage of one frame.
#[derive(Default, Clone, Copy)]
struct Stages {
    capture: Duration,
    decode: Duration,
    convert: Duration,
    present: Duration,
}

/// Rolling per-stage averages, printed once per second by `--stats`.
#[derive(Default)]
struct StageStats {
    since: Option<Instant>,
    frames: u32,
    total: Stages,
}

impl StageStats {
    fn record(&mut self, stages: Stages) {
        self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.total.capture += stages.capture;
        self.total.decode += stages.decode;
        self.total.convert += stages.convert;
        self.total.present += stages.present;
    }

    fn report_if_due(&mut self) {
        let Some(since) = self.since else { return };
        let elapsed = since.elapsed();
        if elapsed < Duration::from_secs(1) || self.frames == 0 {
            return;
        }

        let n = f64::from(self.frames);
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n;
        println!(
            "fps {:5.1} | capture {:5.1} ms | decode {:5.1} ms | convert {:4.1} ms | present {:5.1} ms",
            f64::from(self.frames) / elapsed.as_secs_f64(),
            ms(self.total.capture),
            ms(self.total.decode),
            ms(self.total.convert),
            ms(self.total.present),
        );

        *self = StageStats::default();
    }
}

fn make_window(width: usize, height: usize) -> Result<Window> {
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
    // Never sleep between frames: we are already paced by the camera.
    window.set_target_fps(0);
    Ok(window)
}

/// A moving gradient, so every frame is genuinely different.
fn fill_test_pattern(rgb: &mut [u8], width: usize, height: usize, tick: u8) {
    for y in 0..height {
        let green = (y * 255 / height.max(1)) as u8;
        for x in 0..width {
            let i = (y * width + x) * 3;
            rgb[i] = (x * 255 / width.max(1)) as u8;
            rgb[i + 1] = green;
            rgb[i + 2] = tick;
        }
    }
}

/// Measure the GUI half of the pipeline with no camera involved.
fn bench_gui(args: &Args) -> Result<()> {
    let width = args.width as usize;
    let height = args.height as usize;
    let mut window = make_window(width, height)?;

    let mut rgb = vec![0u8; width * height * 3];
    let mut pixels = vec![0u32; width * height];
    let mut frames = 0u32;
    let mut convert = Duration::ZERO;
    let mut present = Duration::ZERO;

    println!(
        "Benchmark: {width}x{height}, {:.2} MP/frame, window {:?}",
        (width * height) as f64 / 1e6,
        window.get_size()
    );

    let started = Instant::now();
    while window.is_open() && started.elapsed() < Duration::from_secs(5) {
        fill_test_pattern(&mut rgb, width, height, frames as u8);

        let t = Instant::now();
        blit_rgb_to_pixels(&rgb, &mut pixels, width, args.mirror);
        convert += t.elapsed();

        let t = Instant::now();
        window.update_with_buffer(&pixels, width, height)?;
        present += t.elapsed();

        frames += 1;
    }

    let elapsed = started.elapsed().as_secs_f64();
    let n = f64::from(frames.max(1));
    println!(
        "{frames} frames in {elapsed:.2}s = {:.1} fps",
        f64::from(frames) / elapsed
    );
    println!(
        "per frame: convert {:.1} ms, present {:.1} ms, total {:.1} ms",
        convert.as_secs_f64() * 1000.0 / n,
        present.as_secs_f64() * 1000.0 / n,
        (convert + present).as_secs_f64() * 1000.0 / n,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

const MENU_MIRROR: usize = 1;
const MENU_FREEZE: usize = 2;
const MENU_SNAPSHOT: usize = 3;
const MENU_QUIT: usize = 4;
/// Camera items use `MENU_CAMERA_BASE + position`.
const MENU_CAMERA_BASE: usize = 100;

// Commands must never overlap the camera id range.
const _: () = assert!(MENU_CAMERA_BASE > MENU_QUIT);

/// Rebuilding the menu is the only way to refresh its labels (minifb cannot
/// change the text of an existing item), so the state lives in the caller.
fn build_menu(
    entries: &[CameraEntry],
    selected: usize,
    mirror: bool,
    frozen: bool,
) -> Result<Menu> {
    let mut menu = Menu::new("View")?;

    menu.add_item(
        if mirror { "Mirror: on" } else { "Mirror: off" },
        MENU_MIRROR,
    )
    .build();

    let mut camera_menu = Menu::new("Camera")?;
    if entries.is_empty() {
        camera_menu
            .add_item("No camera found", MENU_CAMERA_BASE)
            .enabled(false)
            .build();
    } else {
        for (position, entry) in entries.iter().enumerate() {
            // minifb exposes no check mark, so the current one is marked here.
            let label = if position == selected {
                format!("• {}", entry.name)
            } else {
                format!("   {}", entry.name)
            };
            camera_menu
                .add_item(&label, MENU_CAMERA_BASE + position)
                .build();
        }
    }
    menu.add_sub_menu("Camera", &camera_menu);

    menu.add_separator();
    menu.add_item(if frozen { "Resume" } else { "Freeze" }, MENU_FREEZE)
        .build();
    menu.add_item("Save snapshot", MENU_SNAPSHOT).build();
    menu.add_separator();
    menu.add_item("Quit", MENU_QUIT).build();

    Ok(menu)
}

fn install_menu(
    window: &mut Window,
    handle: &mut Option<MenuHandle>,
    entries: &[CameraEntry],
    selected: usize,
    mirror: bool,
    frozen: bool,
) {
    if let Some(old) = handle.take() {
        window.remove_menu(old);
    }
    match build_menu(entries, selected, mirror, frozen) {
        Ok(menu) => *handle = Some(window.add_menu(&menu)),
        Err(err) => eprintln!("could not build the menu: {err:#}"),
    }
}

/// Open another camera, keeping the current one if the new device fails.
fn switch_camera(
    position: usize,
    entries: &[CameraEntry],
    selected: &mut usize,
    args: &Args,
    camera: &mut Camera,
) -> Option<(usize, usize)> {
    let entry = entries.get(position)?;
    if position == *selected {
        return None;
    }

    let target = CameraIndex::String(entry.id.clone());
    match open_camera(&target, args) {
        Ok(new_camera) => {
            *camera = new_camera;
            *selected = position;

            let format = camera.camera_format();
            println!(
                "Camera now: {} — {}x{} @{} fps ({})",
                entry.name,
                format.width(),
                format.height(),
                format.frame_rate(),
                format.format()
            );
            Some((format.width() as usize, format.height() as usize))
        }
        Err(err) => {
            eprintln!("could not switch to {}: {err:#}", entry.name);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse()?;

    if args.list {
        return list_cameras();
    }
    if args.bench {
        return bench_gui(&args);
    }

    let entries = camera_entries();
    let mut selected = resolve_selection(&args, &entries)?;
    let target = CameraIndex::String(entries[selected].id.clone());

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
    println!("The menu bar has the same options, including a camera picker.");

    let mut window = make_window(width, height)?;

    let mut mirror = args.mirror;
    let mut frozen = false;
    let mut pixels: Vec<u32> = vec![0; width * height];
    // Last decoded frame, kept so a mirror toggle can repaint a frozen image.
    let mut frame_rgb: Vec<u8> = Vec::new();
    let mut have_frame = false;
    let mut redraw = true;
    let mut consecutive_errors = 0u32;
    let mut measured_fps = 0f32;
    let mut stats = StageStats::default();
    let mut warned_slow = false;
    let run_started = Instant::now();
    let mut title_at = Instant::now();
    let mut menu_handle: Option<MenuHandle> = None;

    install_menu(
        &mut window,
        &mut menu_handle,
        &entries,
        selected,
        mirror,
        frozen,
    );

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let loop_started = Instant::now();
        let mut stage = Stages::default();
        let mut menu_changed = false;
        let mut mirror_changed = false;
        let mut snapshot_requested = false;

        // Menu bar: one click per event.
        while let Some(id) = window.is_menu_pressed() {
            match id {
                MENU_MIRROR => {
                    mirror = !mirror;
                    mirror_changed = true;
                    menu_changed = true;
                    redraw = true;
                }
                MENU_FREEZE => {
                    frozen = !frozen;
                    menu_changed = true;
                }
                MENU_SNAPSHOT => snapshot_requested = true,
                MENU_QUIT => return Ok(()),
                id if id >= MENU_CAMERA_BASE => {
                    if let Some((new_width, new_height)) = switch_camera(
                        id - MENU_CAMERA_BASE,
                        &entries,
                        &mut selected,
                        &args,
                        &mut camera,
                    ) {
                        width = new_width;
                        height = new_height;
                        pixels = vec![0; width * height];
                        frame_rgb.clear();
                        have_frame = false;
                        redraw = true;
                        menu_changed = true;
                    }
                }
                _ => {}
            }
        }

        if window.is_key_pressed(Key::Q, KeyRepeat::No) {
            break;
        }
        if window.is_key_pressed(Key::M, KeyRepeat::No) {
            mirror = !mirror;
            mirror_changed = true;
            menu_changed = true;
            redraw = true;
        }
        if window.is_key_pressed(Key::Space, KeyRepeat::No) {
            frozen = !frozen;
            menu_changed = true;
            println!("{}", if frozen { "Frozen" } else { "Live" });
        }

        if !frozen {
            let t = Instant::now();
            let captured = camera.frame();
            stage.capture = t.elapsed();

            match captured {
                Ok(frame) => {
                    let t = Instant::now();
                    let decoded = frame.decode_image::<RgbFormat>();
                    stage.decode = t.elapsed();

                    match decoded {
                        Ok(decoded) => {
                            consecutive_errors = 0;
                            let (w, h) = (decoded.width() as usize, decoded.height() as usize);

                            if (w, h) != (width, height) {
                                width = w;
                                height = h;
                                pixels = vec![0; width * height];
                            }

                            let t = Instant::now();
                            frame_rgb = decoded.into_raw();
                            blit_rgb_to_pixels(&frame_rgb, &mut pixels, width, mirror);
                            stage.convert = t.elapsed();

                            have_frame = true;
                            redraw = true;
                        }
                        Err(err) => {
                            consecutive_errors += 1;
                            eprintln!("decode error: {err}");
                        }
                    }
                }
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

        // A frozen frame must still follow the mirror setting.
        if frozen && mirror_changed && have_frame && !frame_rgb.is_empty() {
            let t = Instant::now();
            blit_rgb_to_pixels(&frame_rgb, &mut pixels, width, mirror);
            stage.convert = t.elapsed();
        }

        if (snapshot_requested || window.is_key_pressed(Key::S, KeyRepeat::No)) && have_frame {
            // Save what is on screen, so mirror mode is honoured.
            let mut snapshot_rgb = Vec::new();
            pixels_to_rgb(&pixels, &mut snapshot_rgb);
            match save_snapshot(&snapshot_rgb, width, height) {
                Ok(path) => println!("Snapshot saved to {}", path.display()),
                Err(err) => eprintln!("snapshot failed: {err:#}"),
            }
        }

        if menu_changed {
            install_menu(
                &mut window,
                &mut menu_handle,
                &entries,
                selected,
                mirror,
                frozen,
            );
        }

        if redraw && have_frame {
            let t = Instant::now();
            window
                .update_with_buffer(&pixels, width, height)
                .context("could not update the window")?;
            stage.present = t.elapsed();
            redraw = false;

            // End-to-end rate: capture + decode + convert + present.
            let elapsed = loop_started.elapsed().as_secs_f32();
            if elapsed > 0.0 {
                let instant = 1.0 / elapsed;
                measured_fps = if measured_fps == 0.0 {
                    instant
                } else {
                    measured_fps * 0.9 + instant * 0.1
                };
            }

            // The title bar is a macOS window property: updating it at camera
            // rate makes the window server redraw it 30 times a second.
            if title_at.elapsed() >= Duration::from_millis(500) {
                title_at = Instant::now();
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
            }

            if args.stats {
                stats.record(stage);
                stats.report_if_due();
            }

            // One-shot hint if the pipeline cannot keep up.
            if !warned_slow
                && measured_fps > 0.0
                && measured_fps < 15.0
                && run_started.elapsed() > Duration::from_secs(2)
            {
                warned_slow = true;
                if cfg!(debug_assertions) {
                    println!(
                        "hint: only {measured_fps:.0} fps — this is an unoptimized build, \
                         `cargo run --release` is much faster"
                    );
                } else {
                    println!(
                        "hint: only {measured_fps:.0} fps — run with --stats to see whether \
                         capture or decode is the limit"
                    );
                }
            }
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

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn entries(names: &[(&str, &str)]) -> Vec<CameraEntry> {
        names
            .iter()
            .map(|(name, id)| CameraEntry {
                name: (*name).to_string(),
                id: (*id).to_string(),
            })
            .collect()
    }

    #[test]
    fn selects_by_position() {
        let list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let args = Args {
            index: Some(1),
            ..Args::default()
        };
        assert_eq!(resolve_selection(&args, &list).unwrap(), 1);
    }

    #[test]
    fn selects_by_name_substring_case_insensitively() {
        let list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let args = Args {
            name: Some("usb".to_string()),
            ..Args::default()
        };
        assert_eq!(resolve_selection(&args, &list).unwrap(), 1);
    }

    #[test]
    fn selects_by_id() {
        let list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let args = Args {
            id: Some("b".to_string()),
            ..Args::default()
        };
        assert_eq!(resolve_selection(&args, &list).unwrap(), 1);
    }

    #[test]
    fn rejects_ambiguous_name() {
        let list = entries(&[("USB Camera 1", "a"), ("USB Camera 2", "b")]);
        let args = Args {
            name: Some("usb".to_string()),
            ..Args::default()
        };
        assert!(resolve_selection(&args, &list).is_err());
    }

    #[test]
    fn rejects_out_of_range_position() {
        let list = entries(&[("FaceTime HD Camera", "a")]);
        let args = Args {
            index: Some(5),
            ..Args::default()
        };
        assert!(resolve_selection(&args, &list).is_err());
    }
}
