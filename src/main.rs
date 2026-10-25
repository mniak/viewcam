//! viewcam — live webcam preview.
//!
//! Frames are captured through the OS native backend (AVFoundation on macOS,
//! V4L2 on Linux, Media Foundation on Windows) and drawn with `minifb`.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use minifb::{Key, KeyRepeat, Menu, MenuHandle, ScaleMode, Window, WindowOptions};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::query;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::{Buffer, Camera, NokhwaError};

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_FPS: u32 = 30;

/// Silence long enough to call the camera gone once it has delivered frames.
const STALL_TIMEOUT: Duration = Duration::from_millis(1000);
/// The first frame gets a longer grace: some USB cameras take seconds to start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for a frame before the GUI gets a chance to run again.
const RECV_WAIT: Duration = Duration::from_millis(100);
/// Retry interval while no camera is available.
const RECOVER_INTERVAL: Duration = Duration::from_secs(2);
/// How often the device list is re-read. A query costs well under a
/// millisecond, so this also drives hot-plug detection and the menu.
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// After a device vanishes from the list, how long its stream may still be
/// silent before the camera counts as gone.
const REMOVAL_GRACE: Duration = Duration::from_millis(250);
/// Opening a camera can take a while, and a busy device may fail on the first
/// try. The open runs in the background and gets a few attempts.
const OPEN_ATTEMPTS: u32 = 3;
/// Give up on a background open after this long (the thread is left to finish).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a status note stays in the window title.
const NOTICE_TIME: Duration = Duration::from_secs(4);

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Args {
    index: Option<u32>,
    name: Option<String>,
    id: Option<String>,
    list: bool,
    probe: bool,
    formats: bool,
    /// `Some` only when the user asked for a mirror state on the command line:
    /// otherwise the saved preference decides.
    mirror: Option<bool>,
    /// Read the saved preferences but never write them.
    no_save: bool,
    /// Delete the saved preferences and exit.
    forget: bool,
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
            mirror: None,
            no_save: false,
            forget: false,
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
                "-m" | "--mirror" => args.mirror = Some(true),
                "--no-mirror" => args.mirror = Some(false),
                "--no-save" => args.no_save = true,
                "--forget" => args.forget = true,
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

    /// True when the command line itself picked a camera.
    fn chose_camera(&self) -> bool {
        self.id.is_some() || self.name.is_some() || self.index.is_some()
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
    -i, --index <N>     Camera to open (default: the last one used, else 0)
    -n, --name <TEXT>   Open the first camera whose name contains TEXT
        --id <ID>       Open a camera by its system id (see --list)
    -W, --width <N>     Preferred capture width (default: {DEFAULT_WIDTH})
    -H, --height <N>    Preferred capture height (default: {DEFAULT_HEIGHT})
        --fps <N>       Preferred frame rate (default: {DEFAULT_FPS})
    -m, --mirror        Start mirrored and remember it (default)
        --no-mirror     Start unmirrored and remember it
        --no-save       Do not write the camera and mirror choices to disk
        --forget        Delete the saved preferences and exit
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

PREFERENCES:
    The camera picked here (by --index/--name/--id or from the menu) and the
    mirror state are remembered in the user config file, so the next run opens
    the same way. Automatic fallback after a device disappears does not
    overwrite them; the saved camera is used again once it is back.
    Set VIEWCAM_CONFIG=<path> to use another file.

RECOVERY:
    The device list is re-read every 500 ms: a camera connected while the
    program is running appears in the menu by itself. If the current camera
    stops delivering frames (unplugged, driver stalled), the program goes back
    to the camera it came from, or to any other camera it finds, and keeps
    retrying every 2 s until one works again.
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
#[derive(Clone)]
struct CameraEntry {
    name: String,
    id: String,
}

/// Devices currently offered by the backend.
fn query_cameras() -> Result<Vec<CameraEntry>> {
    let cameras = query(ApiBackend::Auto).context("could not query the camera backend")?;
    Ok(cameras
        .into_iter()
        .map(|info| CameraEntry {
            name: info.human_name(),
            id: info.misc(),
        })
        .collect())
}

/// Cameras in the order the backend reports them; empty if the query fails.
fn camera_entries() -> Vec<CameraEntry> {
    query_cameras().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Preferences
// ---------------------------------------------------------------------------

/// What the user picked on purpose: a camera (from the command line or from the
/// menu) and the mirror switch. Automatic fallback never writes here, so a
/// camera that was unplugged comes back as the saved choice when replugged.
#[derive(Clone, Debug, Default, PartialEq)]
struct Preferences {
    /// Stable system id (`CameraIndex::misc`), never a position: indices shift
    /// when devices appear and disappear.
    camera_id: Option<String>,
    /// The name the camera had when it was saved; only used in messages.
    camera_name: Option<String>,
    mirror: Option<bool>,
}

/// File name inside the per-user configuration directory.
const PREFERENCES_FILE: &str = "config";

impl Preferences {
    /// `$VIEWCAM_CONFIG` when set, else the usual per-user location:
    /// `~/Library/Application Support/viewcam/config` on macOS.
    fn path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("VIEWCAM_CONFIG") {
            return Some(PathBuf::from(path));
        }

        let home = PathBuf::from(std::env::var_os("HOME")?);
        let base = if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else if let Some(dirs) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(dirs)
        } else {
            home.join(".config")
        };
        Some(base.join("viewcam").join(PREFERENCES_FILE))
    }

    /// `key=value` lines. Unknown keys are ignored so an older viewcam can read
    /// a file written by a newer one.
    fn parse(text: &str) -> Self {
        let mut prefs = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "mirror" => {
                    prefs.mirror = match value.to_ascii_lowercase().as_str() {
                        "on" | "true" | "yes" | "1" => Some(true),
                        "off" | "false" | "no" | "0" => Some(false),
                        _ => None,
                    };
                }
                "camera_id" if !value.is_empty() => prefs.camera_id = Some(value.to_string()),
                "camera_name" if !value.is_empty() => prefs.camera_name = Some(value.to_string()),
                _ => {}
            }
        }
        prefs
    }

    fn render(&self) -> String {
        let mut out = String::from("# viewcam preferences — written by viewcam, safe to delete\n");
        if let Some(mirror) = self.mirror {
            out.push_str(if mirror {
                "mirror=on\n"
            } else {
                "mirror=off\n"
            });
        }
        if let Some(id) = &self.camera_id {
            out.push_str(&format!("camera_id={}\n", one_line(id)));
        }
        if let Some(name) = &self.camera_name {
            out.push_str(&format!("camera_name={}\n", one_line(name)));
        }
        out
    }

    /// A missing or unreadable file just means "no preferences".
    fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("warning: could not read {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Write through a temporary file so a crash cannot leave a half file.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, self.render())
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    }
}

/// Values inside a line-oriented file must not break the next line.
fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Preferences plus where they came from, so every explicit change can be
/// stored right away.
struct PrefsStore {
    prefs: Preferences,
    path: Option<PathBuf>,
    /// `--no-save` turns writing off without affecting reading.
    enabled: bool,
}

impl PrefsStore {
    fn load(enabled: bool) -> Self {
        let path = Preferences::path();
        let prefs = path.as_deref().map(Preferences::load).unwrap_or_default();
        Self {
            prefs,
            path,
            enabled,
        }
    }

    fn saved_mirror(&self) -> Option<bool> {
        self.prefs.mirror
    }

    fn saved_camera(&self) -> Option<&str> {
        self.prefs.camera_id.as_deref()
    }

    /// Delete the file backing the preferences.
    fn forget() -> Result<()> {
        let Some(path) = Preferences::path() else {
            println!("No preferences file location is available.");
            return Ok(());
        };
        match fs::remove_file(&path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                println!("Nothing saved at {}", path.display());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("could not remove {}", path.display()));
            }
        }
        Ok(())
    }

    fn remember_mirror(&mut self, mirror: bool) {
        if self.prefs.mirror == Some(mirror) {
            return;
        }
        self.prefs.mirror = Some(mirror);
        self.write();
    }

    fn remember_camera(&mut self, entry: &CameraEntry) {
        if self.prefs.camera_id.as_deref() == Some(entry.id.as_str())
            && self.prefs.camera_name.as_deref() == Some(entry.name.as_str())
        {
            return;
        }
        self.prefs.camera_id = Some(entry.id.clone());
        self.prefs.camera_name = Some(entry.name.clone());
        self.write();
    }

    fn write(&self) {
        if !self.enabled {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        if let Err(err) = self.prefs.save(path) {
            eprintln!(
                "warning: could not save preferences to {}: {err:#}",
                path.display()
            );
        }
    }
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

/// The camera to open at startup. An explicit command-line choice always wins;
/// otherwise the saved camera is used when it is connected, and the first
/// camera when it is not.
fn resolve_startup(args: &Args, prefs: &PrefsStore, entries: &[CameraEntry]) -> Result<usize> {
    if args.chose_camera() {
        return resolve_selection(args, entries);
    }
    if entries.is_empty() {
        return resolve_selection(args, entries);
    }

    if let Some(saved) = prefs.saved_camera() {
        if let Some(position) = entries.iter().position(|entry| entry.id == saved) {
            return Ok(position);
        }
        let label = prefs
            .prefs
            .camera_name
            .as_deref()
            .unwrap_or(saved)
            .to_string();
        eprintln!(
            "note: saved camera {label:?} is not connected; using {}",
            entries[0].name
        );
    }

    Ok(0)
}

/// Open the camera at `selected`. When the choice came from the saved
/// preferences and that device fails, another camera is tried instead, so a
/// busy remembered camera does not stop the program from starting. An explicit
/// command-line choice is never second-guessed.
fn open_selected(
    args: &Args,
    entries: &[CameraEntry],
    selected: &mut usize,
    allow_fallback: bool,
) -> Result<(Feed, usize, usize)> {
    let target = CameraIndex::String(entries[*selected].id.clone());
    match open_and_spawn(&target, args) {
        Ok(opened) => Ok(opened),
        Err(err) if allow_fallback => {
            eprintln!(
                "warning: the saved camera {} could not be opened: {err:#}",
                entries[*selected].name
            );
            for (position, entry) in entries.iter().enumerate() {
                if position == *selected {
                    continue;
                }
                let target = CameraIndex::String(entry.id.clone());
                match open_and_spawn(&target, args) {
                    Ok(opened) => {
                        *selected = position;
                        return Ok(opened);
                    }
                    Err(other) => {
                        eprintln!("could not open {}: {other:#}", entry.name);
                    }
                }
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

fn camera_names(entries: &[CameraEntry]) -> String {
    entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Same devices, same order, same names.
fn same_cameras(left: &[CameraEntry], right: &[CameraEntry]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(a, b)| a.id == b.id && a.name == b.name)
}

/// Replace the device list, keeping `selected` on the same camera. Positions
/// shift when a device appears or disappears, so matching by id is the only
/// stable link. Returns true when the list actually changed.
fn apply_refresh(
    entries: &mut Vec<CameraEntry>,
    selected: &mut usize,
    fresh: Vec<CameraEntry>,
) -> bool {
    if same_cameras(entries, &fresh) {
        return false;
    }

    let live = entries.get(*selected).cloned();
    *entries = fresh;
    *selected = live
        .and_then(|entry| entries.iter().position(|other| other.id == entry.id))
        .unwrap_or(usize::MAX);
    true
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
/// A camera that stops answering makes this fail instead of hanging.
fn probe_camera(target: &CameraIndex, args: &Args) -> Result<()> {
    let (feed, _, _) = open_and_spawn(target, args)?;

    const FRAMES: u32 = 20;
    const BUDGET: Duration = Duration::from_secs(10);

    let mut frames = 0u32;
    let mut luma_sum = 0f64;
    let mut capture_time = Duration::ZERO;
    let mut decode_time = Duration::ZERO;
    let mut last_rgb: Vec<u8> = Vec::new();
    let (mut width, mut height) = (0usize, 0usize);

    let started = Instant::now();
    while frames < FRAMES {
        if started.elapsed() >= BUDGET {
            bail!(
                "the camera delivered {frames}/{FRAMES} frames in {} s — it may be \
                 busy, blocked or disconnected",
                BUDGET.as_secs()
            );
        }

        let t0 = Instant::now();
        let received = match feed.next_frame() {
            Ok(received) => received,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("the capture thread stopped"),
        };
        capture_time += t0.elapsed();

        let frame = match received {
            Ok(frame) => frame,
            Err(err) => {
                eprintln!("capture error: {err}");
                continue;
            }
        };

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
    feed.shutdown();
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
        blit_rgb_to_pixels(&rgb, &mut pixels, width, args.mirror.unwrap_or(true));
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
// Capture thread
// ---------------------------------------------------------------------------

/// A worker that owns the camera and pushes frames to the GUI thread.
///
/// `nokhwa`'s `frame()` blocks on a channel that AVFoundation only feeds while
/// the device is alive; when the camera is unplugged the call never returns and
/// would freeze the whole program. Running it on its own thread keeps the GUI
/// responsive, so a dead camera can be noticed and replaced.
struct Feed {
    frames: Receiver<Result<Buffer, NokhwaError>>,
    stop: Arc<AtomicBool>,
    exited: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl Feed {
    fn spawn(mut camera: Camera) -> Self {
        // Capacity 1: a paused GUI cannot let frames pile up in memory.
        let (sender, frames) = sync_channel::<Result<Buffer, NokhwaError>>(1);
        let (exited_sender, exited) = sync_channel::<()>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let captured = camera.frame();
                let failed = captured.is_err();
                if sender.send(captured).is_err() {
                    // The GUI moved on (or quit); nothing left to feed.
                    break;
                }
                if failed {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
            let _ = camera.stop_stream();
            let _ = exited_sender.send(());
        });

        Self {
            frames,
            stop,
            exited,
            handle: Some(handle),
        }
    }

    /// Drop the worker without waiting: used when the device is already gone,
    /// so there is nothing left to release.
    fn abandon(self) {
        let Self { frames, stop, .. } = self;
        stop.store(true, Ordering::Relaxed);
        drop(frames);
    }

    /// Ask the worker to finish, giving up on it if the camera call never
    /// returns (an unplugged device keeps the worker blocked forever).
    fn shutdown(self) {
        let Self {
            frames,
            stop,
            exited,
            handle,
        } = self;
        stop.store(true, Ordering::Relaxed);
        // Closing the channel wakes a worker that is blocked handing over a
        // frame; a worker stuck in the backend stays blocked and is detached.
        drop(frames);
        if exited.recv_timeout(Duration::from_millis(500)).is_ok() {
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }
    }

    /// Newest frame, discarding any backlog.
    fn next_frame(&self) -> std::result::Result<Result<Buffer, NokhwaError>, RecvTimeoutError> {
        let mut newest = self.frames.recv_timeout(RECV_WAIT)?;
        while let Ok(frame) = self.frames.try_recv() {
            newest = frame;
        }
        Ok(newest)
    }
}

/// Open `target` and start feeding it on a worker thread.
fn open_and_spawn(target: &CameraIndex, args: &Args) -> Result<(Feed, usize, usize)> {
    let camera = open_camera(target, args)?;
    let format = camera.camera_format();
    let width = format.width() as usize;
    let height = format.height() as usize;
    println!("Camera {target} open — {}", describe(&camera));
    Ok((Feed::spawn(camera), width, height))
}

/// A camera being opened off the GUI thread, so the current image keeps
/// playing and the window stays responsive while the driver works.
struct PendingOpen {
    label: String,
    target_id: String,
    /// Camera that was live when the switch was requested: the fallback if the
    /// new one turns out to be dead.
    previous_id: Option<String>,
    result: Receiver<Result<(Feed, usize, usize), anyhow::Error>>,
    started_at: Instant,
}

/// Start opening `target` in the background. A device that is still busy (a
/// session being torn down, a just-unplugged driver) often succeeds a moment
/// later, so the open is retried a few times before giving up.
fn begin_open(
    label: String,
    target_id: String,
    args: Args,
    previous_id: Option<String>,
) -> PendingOpen {
    let (sender, result) = sync_channel(1);
    let thread_label = label.clone();
    let thread_target = CameraIndex::String(target_id.clone());

    std::thread::spawn(move || {
        let mut last_error = None;
        for attempt in 1..=OPEN_ATTEMPTS {
            match open_and_spawn(&thread_target, &args) {
                Ok(opened) => {
                    // If the GUI moved on, this fails and the camera is closed.
                    let _ = sender.send(Ok(opened));
                    return;
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt < OPEN_ATTEMPTS {
                        eprintln!(
                            "could not open {thread_label} (attempt {attempt}/{OPEN_ATTEMPTS}), retrying"
                        );
                        std::thread::sleep(Duration::from_millis(400 * u64::from(attempt)));
                    }
                }
            }
        }
        let _ = sender.send(Err(
            last_error.unwrap_or_else(|| anyhow!("the camera did not open"))
        ));
    });

    PendingOpen {
        label,
        target_id,
        previous_id,
        result,
        started_at: Instant::now(),
    }
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

/// The camera disappeared (unplugged, or the driver stopped delivering). Look
/// at the devices again and try the camera we came from before any other.
fn recover_camera(
    entries: &mut Vec<CameraEntry>,
    selected: &mut usize,
    previous: &mut Option<String>,
    args: &Args,
    quiet: bool,
) -> Option<(Feed, usize, usize)> {
    *entries = camera_entries();

    let mut order: Vec<String> = previous.iter().cloned().collect();
    for entry in entries.iter() {
        if !order.contains(&entry.id) {
            order.push(entry.id.clone());
        }
    }

    for id in order {
        let Some(position) = entries.iter().position(|entry| entry.id == id) else {
            continue;
        };
        let target = CameraIndex::String(id);
        match open_and_spawn(&target, args) {
            Ok((feed, width, height)) => {
                println!("Back on {}", entries[position].name);
                *selected = position;
                return Some((feed, width, height));
            }
            Err(err) => {
                if !quiet {
                    eprintln!("could not reopen {}: {err:#}", entries[position].name);
                }
            }
        }
    }

    None
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
    if args.forget {
        return PrefsStore::forget();
    }

    let mut prefs = PrefsStore::load(!args.no_save);

    let mut entries = camera_entries();
    let mut selected = resolve_startup(&args, &prefs, &entries)?;

    if args.formats {
        let target = CameraIndex::String(entries[selected].id.clone());
        return print_formats(&target, &args);
    }
    if args.probe {
        let target = CameraIndex::String(entries[selected].id.clone());
        return probe_camera(&target, &args);
    }

    // A remembered camera that refuses to open (another program took it, the
    // driver is still coming up) must not keep the program from starting.
    let used_saved = !args.chose_camera()
        && prefs
            .saved_camera()
            .is_some_and(|id| entries[selected].id == id);
    let (feed, mut width, mut height) = open_selected(&args, &entries, &mut selected, used_saved)?;

    if width as u32 != args.width || height as u32 != args.height {
        println!(
            "note: requested {}x{}, the camera gave {}x{}",
            args.width, args.height, width, height
        );
    }

    // A camera is only worth remembering once it really delivers a frame: a
    // device that opens and stays dark is not a working choice. The command
    // line is an explicit choice too, so it starts the same way.
    let mut save_camera_after_frame: Option<CameraEntry> = if args.chose_camera() {
        entries.get(selected).cloned()
    } else {
        None
    };

    // Command line, then the saved preference, then the default.
    let mut mirror = args.mirror.or(prefs.saved_mirror()).unwrap_or(true);
    if let Some(wanted) = args.mirror {
        prefs.remember_mirror(wanted);
    }

    println!("Camera: {}", entries[selected].name);
    println!("Mirror: {}", if mirror { "on" } else { "off" });
    println!("Keys: Esc/Q quit · M mirror · S snapshot · Space freeze");
    println!("The menu bar has the same options, including a camera picker.");
    if prefs.enabled {
        if let Some(path) = &prefs.path {
            println!("Preferences file: {}", path.display());
        }
    }

    let mut window = make_window(width, height)?;
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

    let mut feed = Some(feed);
    // Camera we switched away from: the first candidate if the current one dies.
    let mut previous: Option<String> = None;
    let mut last_frame_at = Instant::now();
    let mut frames_from_feed = 0u32;
    let mut last_refresh_at = Instant::now();
    // A device list that differs for a single poll is not trusted: flaky
    // devices flap in and out, and every rebuild redraws the menu bar.
    let mut pending_entries: Option<Vec<CameraEntry>> = None;
    let mut next_attempt_at = Instant::now();
    let mut first_attempt = true;
    // Camera being opened in the background, and the last status note.
    let mut pending_open: Option<PendingOpen> = None;
    let mut notice: Option<(String, Instant)> = None;

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

        // Finish a background camera open. The old feed keeps running until the
        // new camera is really there, so a failed switch costs no image.
        if let Some(open) = pending_open.take() {
            match open.result.try_recv() {
                Ok(Ok((new_feed, _, _))) => {
                    if let Some(old) = feed.take() {
                        // Release the old device without stalling the GUI.
                        std::thread::spawn(move || old.shutdown());
                    }
                    feed = Some(new_feed);
                    // The window keeps the current picture until the first new
                    // frame arrives; the capture loop adopts the new size then.
                    selected = entries
                        .iter()
                        .position(|entry| entry.id == open.target_id)
                        .unwrap_or(usize::MAX);
                    if let Some(entry) = entries.get(selected) {
                        save_camera_after_frame = Some(entry.clone());
                    }
                    previous = open.previous_id;
                    last_frame_at = Instant::now();
                    frames_from_feed = 0;
                    consecutive_errors = 0;
                    redraw = true;
                    menu_changed = true;
                    notice = None;
                    println!("Camera now: {}", open.label);
                }
                Ok(Err(err)) => {
                    eprintln!("could not switch to {}: {err:#}", open.label);
                    notice = Some((format!("{} unavailable", open.label), Instant::now()));
                }
                Err(TryRecvError::Empty) => {
                    if open.started_at.elapsed() < OPEN_TIMEOUT {
                        pending_open = Some(open);
                    } else {
                        eprintln!(
                            "gave up opening {} after {} s",
                            open.label,
                            OPEN_TIMEOUT.as_secs()
                        );
                        notice = Some((format!("{} unavailable", open.label), Instant::now()));
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    eprintln!("the camera open thread stopped unexpectedly");
                    notice = Some((format!("{} unavailable", open.label), Instant::now()));
                }
            }
        }

        // Menu bar: one click per event.
        while let Some(id) = window.is_menu_pressed() {
            match id {
                MENU_MIRROR => {
                    mirror = !mirror;
                    mirror_changed = true;
                    menu_changed = true;
                    redraw = true;
                    prefs.remember_mirror(mirror);
                }
                MENU_FREEZE => {
                    frozen = !frozen;
                    menu_changed = true;
                }
                MENU_SNAPSHOT => snapshot_requested = true,
                MENU_QUIT => return Ok(()),
                id if id >= MENU_CAMERA_BASE => {
                    let position = id - MENU_CAMERA_BASE;
                    if let Some(entry) = entries.get(position) {
                        // Do not open a camera twice, and do not restart an open
                        // for the camera that is already on its way.
                        let already_opening = pending_open
                            .as_ref()
                            .is_some_and(|open| open.target_id == entry.id);
                        if position != selected && !already_opening {
                            let previous_id =
                                entries.get(selected).map(|current| current.id.clone());
                            pending_open = Some(begin_open(
                                entry.name.clone(),
                                entry.id.clone(),
                                args.clone(),
                                previous_id,
                            ));
                            notice = None;
                        }
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
            prefs.remember_mirror(mirror);
        }
        if window.is_key_pressed(Key::Space, KeyRepeat::No) {
            frozen = !frozen;
            menu_changed = true;
            println!("{}", if frozen { "Frozen" } else { "Live" });
        }

        let mut lost = false;
        let mut lost_name: Option<String> = None;

        if !frozen {
            if let Some(current) = feed.as_ref() {
                // Waiting for the worker doubles as the frame pacing; a camera
                // that stops delivering only costs this wait, never the thread.
                let t = Instant::now();
                let received = current.next_frame();
                stage.capture = t.elapsed();

                match received {
                    Ok(Ok(frame)) => {
                        last_frame_at = Instant::now();
                        frames_from_feed += 1;
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

                                // The camera works: it is now a real preference.
                                if let Some(entry) = save_camera_after_frame.take() {
                                    prefs.remember_camera(&entry);
                                }
                            }
                            Err(err) => {
                                consecutive_errors += 1;
                                eprintln!("decode error: {err}");
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        consecutive_errors += 1;
                        eprintln!("capture error: {err}");
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // A camera that never delivered anything gets more time
                        // to spin up than one that was working.
                        let grace = if frames_from_feed == 0 {
                            STARTUP_TIMEOUT
                        } else {
                            STALL_TIMEOUT
                        };
                        if last_frame_at.elapsed() >= grace {
                            lost = true;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => lost = true,
                }
            }

            if consecutive_errors >= 60 {
                lost = true;
            }
        }

        // Hot-plug and unplug: re-read the device list (well under a
        // millisecond) so cameras connected while the program is running reach
        // the menu, and a removed one is noticed without waiting for a stall.
        if last_refresh_at.elapsed() >= REFRESH_INTERVAL {
            last_refresh_at = Instant::now();
            if let Ok(fresh) = query_cameras() {
                let live = entries.get(selected).cloned();
                let listed = live
                    .as_ref()
                    .is_some_and(|entry| fresh.iter().any(|other| other.id == entry.id));

                if same_cameras(&entries, &fresh) {
                    pending_entries = None;
                } else if pending_entries
                    .as_ref()
                    .is_some_and(|pending| same_cameras(pending, &fresh))
                {
                    // Same change twice in a row: it is real.
                    pending_entries = None;
                    if apply_refresh(&mut entries, &mut selected, fresh) {
                        menu_changed = true;
                    }
                } else {
                    pending_entries = Some(fresh);
                }

                // A camera that left the list and went silent is gone: no point
                // waiting for the stall timer. Frames still arriving win, so a
                // device that merely blinked out of the list keeps streaming.
                if let Some(entry) = live {
                    if !frozen
                        && feed.is_some()
                        && !listed
                        && last_frame_at.elapsed() >= REMOVAL_GRACE
                    {
                        lost = true;
                        lost_name = Some(entry.name);
                    }
                }
            }
        }

        if lost {
            let name = lost_name
                .or_else(|| entries.get(selected).map(|entry| entry.name.clone()))
                .unwrap_or_else(|| "camera".to_string());
            eprintln!(
                "camera lost: {name} (no frames for {:.1} s)",
                last_frame_at.elapsed().as_secs_f32()
            );
            if let Some(dead) = feed.take() {
                // The device is gone; do not wait for it to release anything.
                dead.abandon();
            }
            // Forget which entry is live; the menu should not claim one is.
            selected = usize::MAX;
            consecutive_errors = 0;
            frames_from_feed = 0;
            // A camera that never showed anything must not be remembered.
            save_camera_after_frame = None;
            first_attempt = true;
            next_attempt_at = Instant::now();
        }

        // (Re)connect: either a camera just died or we are still waiting for one.
        if !frozen && feed.is_none() && Instant::now() >= next_attempt_at {
            let quiet = !first_attempt;
            match recover_camera(&mut entries, &mut selected, &mut previous, &args, quiet) {
                Some((new_feed, new_width, new_height)) => {
                    feed = Some(new_feed);
                    width = new_width;
                    height = new_height;
                    pixels = vec![0; width * height];
                    frame_rgb.clear();
                    have_frame = false;
                    redraw = true;
                    menu_changed = true;
                    notice = None;
                    last_frame_at = Instant::now();
                    frames_from_feed = 0;
                    consecutive_errors = 0;
                    // The fallback camera was not picked by the user.
                    save_camera_after_frame = None;
                }
                None => {
                    if first_attempt {
                        eprintln!(
                            "no camera available; retrying every {} s",
                            RECOVER_INTERVAL.as_secs()
                        );
                    }
                    first_attempt = false;
                    next_attempt_at = Instant::now() + RECOVER_INTERVAL;
                    // The device list changed even though nothing reopened.
                    menu_changed = true;
                }
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

        // Hot-plug: re-read the device list so a camera connected while the
        // program is running shows up in the menu. Only touch the menu when
        // something actually changed, so an open menu is not closed for nothing.
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
            if args.stats {
                stats.record(stage);
                stats.report_if_due();
            }

            // One-shot hint if the pipeline cannot keep up. Ignore the first
            // seconds, and the first frames of a freshly opened camera, whose
            // loop time still includes the opening handshake.
            if !warned_slow
                && measured_fps > 0.0
                && measured_fps < 15.0
                && frames_from_feed >= 20
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
            // No new frame (frozen, no camera or waiting): keep events flowing.
            window.update();
            let pause = if feed.is_none() { 50 } else { 5 };
            std::thread::sleep(Duration::from_millis(pause));
        }

        // Outside the frame branch: the state must reach the title bar even
        // when the image is frozen or no camera is available.
        if title_at.elapsed() >= Duration::from_millis(400) {
            title_at = Instant::now();
            let live_state = if feed.is_none() {
                "no camera"
            } else if frozen {
                "frozen"
            } else if frames_from_feed == 0 && last_frame_at.elapsed() > Duration::from_millis(500)
            {
                "waiting for frames"
            } else if last_frame_at.elapsed() > Duration::from_millis(600) {
                "no signal"
            } else if mirror {
                "mirrored"
            } else {
                "live"
            };
            let state = if let Some(open) = &pending_open {
                format!("opening {}", open.label)
            } else if let Some((text, at)) = &notice {
                if at.elapsed() < NOTICE_TIME {
                    text.clone()
                } else {
                    live_state.to_string()
                }
            } else {
                live_state.to_string()
            };
            window.set_title(&format!(
                "viewcam — {width}x{height} — {measured_fps:.0} fps — {state}"
            ));
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

    #[test]
    fn refresh_ignores_an_unchanged_list() {
        let mut list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let mut selected = 1;
        assert!(!apply_refresh(
            &mut list,
            &mut selected,
            entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")])
        ));
        assert_eq!(selected, 1);
    }

    #[test]
    fn refresh_follows_the_camera_when_positions_shift() {
        let mut list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let mut selected = 1;
        // A new device shows up in front of the list.
        assert!(apply_refresh(
            &mut list,
            &mut selected,
            entries(&[
                ("iPhone Camera", "c"),
                ("FaceTime HD Camera", "a"),
                ("USB Camera", "b")
            ])
        ));
        assert_eq!(list[selected].id, "b");
        assert_eq!(selected, 2);
    }

    #[test]
    fn refresh_drops_the_selection_when_the_camera_disappears() {
        let mut list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let mut selected = 1;
        assert!(apply_refresh(
            &mut list,
            &mut selected,
            entries(&[("FaceTime HD Camera", "a")])
        ));
        assert_eq!(selected, usize::MAX);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn refresh_leaves_an_empty_selection_alone() {
        let mut list = Vec::new();
        let mut selected = usize::MAX;
        assert!(apply_refresh(
            &mut list,
            &mut selected,
            entries(&[("FaceTime HD Camera", "a")])
        ));
        assert_eq!(selected, usize::MAX);
    }
}

#[cfg(test)]
mod preferences_tests {
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

    fn store(camera: Option<(&str, &str)>, mirror: Option<bool>) -> PrefsStore {
        PrefsStore {
            prefs: Preferences {
                camera_id: camera.map(|(_, id)| id.to_string()),
                camera_name: camera.map(|(name, _)| name.to_string()),
                mirror,
            },
            path: None,
            enabled: false,
        }
    }

    fn temp_config(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("viewcam-{tag}-{}", std::process::id()));
        let path = dir.join("config");
        (dir, path)
    }

    #[test]
    fn preferences_round_trip_through_text() {
        let prefs = Preferences {
            camera_id: Some("0x113000005e30515".to_string()),
            camera_name: Some("USB 2.0 = Camera".to_string()),
            mirror: Some(false),
        };
        assert_eq!(Preferences::parse(&prefs.render()), prefs);
    }

    #[test]
    fn preferences_ignore_junk_and_unknown_keys() {
        let prefs = Preferences::parse(
            "\n# a comment\nfuture_option=1\nmirror=maybe\ncamera_id=\ncamera_name=Cam\n",
        );
        assert_eq!(prefs.mirror, None);
        assert_eq!(prefs.camera_id, None);
        assert_eq!(prefs.camera_name.as_deref(), Some("Cam"));
    }

    #[test]
    fn preferences_accept_common_boolean_spellings() {
        for text in ["mirror=on", "mirror=true", "mirror=1", "mirror=Yes"] {
            assert_eq!(Preferences::parse(text).mirror, Some(true), "{text}");
        }
        for text in ["mirror=off", "mirror=false", "mirror=0"] {
            assert_eq!(Preferences::parse(text).mirror, Some(false), "{text}");
        }
    }

    #[test]
    fn preferences_keep_names_on_one_line() {
        let prefs = Preferences {
            camera_name: Some("Line\nbreak".to_string()),
            ..Preferences::default()
        };
        assert_eq!(
            Preferences::parse(&prefs.render()).camera_name.as_deref(),
            Some("Line break")
        );
    }

    #[test]
    fn startup_uses_the_saved_camera() {
        let list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let prefs = store(Some(("USB Camera", "b")), None);
        assert_eq!(resolve_startup(&Args::default(), &prefs, &list).unwrap(), 1);
    }

    #[test]
    fn startup_falls_back_when_the_saved_camera_is_gone() {
        let list = entries(&[("FaceTime HD Camera", "a")]);
        let prefs = store(Some(("USB Camera", "b")), None);
        assert_eq!(resolve_startup(&Args::default(), &prefs, &list).unwrap(), 0);
    }

    #[test]
    fn startup_command_line_wins_over_the_saved_camera() {
        let list = entries(&[("FaceTime HD Camera", "a"), ("USB Camera", "b")]);
        let prefs = store(Some(("USB Camera", "b")), None);
        let args = Args {
            index: Some(0),
            ..Args::default()
        };
        assert_eq!(resolve_startup(&args, &prefs, &list).unwrap(), 0);
    }

    #[test]
    fn store_writes_and_reads_the_file() {
        let (dir, path) = temp_config("round-trip");
        let _ = fs::remove_dir_all(&dir);

        let mut prefs = PrefsStore {
            prefs: Preferences::default(),
            path: Some(path.clone()),
            enabled: true,
        };
        prefs.remember_mirror(false);
        prefs.remember_camera(&CameraEntry {
            name: "USB Camera".to_string(),
            id: "b".to_string(),
        });

        let loaded = Preferences::load(&path);
        assert_eq!(loaded.mirror, Some(false));
        assert_eq!(loaded.camera_id.as_deref(), Some("b"));
        assert_eq!(loaded.camera_name.as_deref(), Some("USB Camera"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_respects_no_save() {
        let (dir, path) = temp_config("no-save");
        let _ = fs::remove_dir_all(&dir);

        let mut prefs = PrefsStore {
            prefs: Preferences::default(),
            path: Some(path.clone()),
            enabled: false,
        };
        prefs.remember_mirror(true);
        prefs.remember_camera(&CameraEntry {
            name: "USB Camera".to_string(),
            id: "b".to_string(),
        });

        assert!(!path.exists());
        assert!(!dir.exists());
    }
}
