use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::Duration,
};

use v4l::{
    buffer::Type,
    format::fourcc::FourCC,
    frameinterval::FrameIntervalEnum,
    framesize::FrameSizeEnum,
    io::traits::CaptureStream,
    prelude::*,
    video::{Capture, capture::Parameters},
};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub enum FrameFormat {
    Rgb24,
    Yuyv,
}

/// A video frame — either RGB24 (software path) or YUYV (V4L2 hardware
/// passthrough).
#[derive(Clone)]
pub struct Frame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
}

impl Frame {
    /// Convenience: build a Frame that takes ownership of a `Vec<u8>`.
    /// Wraps the buffer in `Arc` so the consumer can hold it cheaply
    /// while the capture thread reuses its ring slot for the next frame.
    pub fn from_owned(data: Vec<u8>, width: u32, height: u32, format: FrameFormat) -> Self {
        Self {
            data: Arc::new(data),
            width,
            height,
            format,
        }
    }
}

/// A user-selectable capture mode (format + resolution + estimated fps).
#[derive(Clone, Debug)]
pub struct CapMode {
    pub is_mjpeg: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CapMode {
    pub fn label(&self) -> String {
        format!(
            "{} {}×{} @{}fps",
            if self.is_mjpeg { "MJPEG" } else { "YUYV" },
            self.width,
            self.height,
            self.fps
        )
    }
    fn fourcc(&self) -> FourCC {
        if self.is_mjpeg {
            FourCC::new(b"MJPG")
        } else {
            FourCC::new(b"YUYV")
        }
    }
}

/// Describes a detected V4L2 device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub modes: Vec<CapMode>,
    /// Pre-formatted one-liner derived from modes.
    pub caps_summary: String,
}

// ── Device enumeration ────────────────────────────────────────────────────────

/// List all `/dev/video*` devices that can be opened and queried.
pub fn enumerate_devices() -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    for i in 0..16 {
        let path = PathBuf::from(format!("/dev/video{i}"));
        if !path.exists() {
            continue;
        }
        if let Ok(dev) = Device::with_path(&path)
            && let Ok(caps) = dev.query_caps()
        {
            let card = caps.card.trim_end_matches('\0').trim().to_string();
            if !card.is_empty() {
                let modes = query_device_modes(&path);
                if modes.is_empty() {
                    continue;
                } // no usable formats — hide this device
                let caps_summary = modes
                    .iter()
                    .map(|m: &CapMode| m.label())
                    .collect::<Vec<_>>()
                    .join("  ·  ");
                devices.push(DeviceInfo {
                    path: path.clone(),
                    name: card,
                    modes,
                    caps_summary,
                });
            }
        }
    }
    devices
}

/// Query all usable capture modes for a device without starting a stream.
pub fn query_device_modes(path: &PathBuf) -> Vec<CapMode> {
    let Ok(dev) = Device::with_path(path) else {
        return vec![];
    };
    let Ok(formats) = dev.enum_formats() else {
        return vec![];
    };

    let fourcc_mjpeg = FourCC::new(b"MJPG");
    let fourcc_yuyv = FourCC::new(b"YUYV");
    let mut modes: Vec<CapMode> = Vec::new();

    for &(target_fourcc, is_mjpeg) in &[(fourcc_mjpeg, true), (fourcc_yuyv, false)] {
        if !formats.iter().any(|f| f.fourcc == target_fourcc) {
            continue;
        }
        let sizes = dev.enum_framesizes(target_fourcc).unwrap_or_default();
        for fs in &sizes {
            let candidates: Vec<(u32, u32)> = match &fs.size {
                FrameSizeEnum::Discrete(d) => vec![(d.width, d.height)],
                FrameSizeEnum::Stepwise(s) => [
                    (s.max_width, s.max_height),
                    (1920, 1080),
                    (1280, 720),
                    (640, 480),
                ]
                .iter()
                .filter(|&&(w, h)| {
                    w >= s.min_width && w <= s.max_width && h >= s.min_height && h <= s.max_height
                })
                .copied()
                .collect(),
            };
            const ALLOWED_HEIGHTS: &[u32] = &[480, 720, 800, 1080];
            for (w, h) in candidates {
                if !ALLOWED_HEIGHTS.contains(&h) {
                    continue;
                }
                // Collect every distinct fps the driver reports for this resolution
                let fps_list: Vec<u32> = match dev.enum_frameintervals(target_fourcc, w, h) {
                    Ok(ivals) if !ivals.is_empty() => {
                        let mut fps_set = std::collections::BTreeSet::new();
                        for fi in &ivals {
                            let fp = match &fi.interval {
                                FrameIntervalEnum::Discrete(frac) if frac.numerator > 0 => {
                                    frac.denominator / frac.numerator
                                }
                                FrameIntervalEnum::Stepwise(s) if s.min.numerator > 0 => {
                                    s.min.denominator / s.min.numerator
                                }
                                _ => 0,
                            };
                            if fp > 0 {
                                fps_set.insert(fp);
                            }
                        }
                        if fps_set.is_empty() {
                            vec![estimate_fps(is_mjpeg, w, h)]
                        } else {
                            fps_set.into_iter().collect()
                        }
                    }
                    _ => vec![estimate_fps(is_mjpeg, w, h)],
                };
                for fps in fps_list {
                    if !is_mjpeg && fps < 5 {
                        continue;
                    }
                    modes.push(CapMode {
                        is_mjpeg,
                        width: w,
                        height: h,
                        fps,
                    });
                }
            }
        }
    }

    modes.sort_by(|a, b| {
        b.is_mjpeg
            .cmp(&a.is_mjpeg)
            .then((b.width * b.height).cmp(&(a.width * a.height)))
            .then(b.fps.cmp(&a.fps))
    });
    // Dedup only exact duplicates (same format + res + fps); keep different fps as separate modes
    modes.dedup_by(|a, b| {
        a.is_mjpeg == b.is_mjpeg && a.width == b.width && a.height == b.height && a.fps == b.fps
    });
    modes
}

/// Estimate achievable fps based on format and resolution.
/// MJPEG is compressed so USB bandwidth is rarely a bottleneck — allow up to 60fps.
/// YUYV is raw (2 bytes/pixel) — estimate based on USB 2.0 practical bandwidth (~42 MB/s).
fn estimate_fps(is_mjpeg: bool, w: u32, h: u32) -> u32 {
    if is_mjpeg {
        return 60;
    }
    let bytes_per_frame = (w * h * 2) as u64;
    let usb2_bandwidth: u64 = 42_000_000; // ~42 MB/s practical USB 2.0 isochronous
    ((usb2_bandwidth / bytes_per_frame) as u32).clamp(1, 60)
}

// ── Capture thread ────────────────────────────────────────────────────────────

/// Spawn a background thread that continuously captures frames from `device_path`.
///
/// Returns a `Receiver` plus an `Arc<AtomicU32>` that the capture thread updates
/// every second with the current capture fps (stored as fps × 10, e.g. 299 = 29.9 fps).
pub fn start_capture(
    device_path: PathBuf,
    mode: Option<CapMode>,
) -> (Receiver<Option<Frame>>, Arc<AtomicU32>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let (tx, rx): (SyncSender<Option<Frame>>, Receiver<Option<Frame>>) = sync_channel(1);
    let fps_counter = Arc::new(AtomicU32::new(0));
    let fps_counter_thread = Arc::clone(&fps_counter);
    // Named thread for `htop` / `top` / crash diagnostics.
    let _ = thread::Builder::new()
        .name("v4l-capture".into())
        .spawn(move || capture_loop(device_path, mode, tx, fps_counter_thread, stop_thread));
    (rx, fps_counter, stop)
}

/// Returns a null receiver that always yields `None` (black screen mode).
pub fn null_receiver() -> Receiver<Option<Frame>> {
    let (tx, rx) = sync_channel(1);
    // Send one None so the first recv() doesn't block
    let _ = tx.send(None);
    rx
}

// ── Capture-thread tuning (CPU pin + RT scheduling) ──────────────────────────

/// Pin the calling thread to a single CPU and try to elevate its scheduler
/// priority.  All syscalls are best-effort: failures are logged and ignored
/// so the capture loop continues even when the user lacks `CAP_SYS_NICE`.
///
/// Strategy (in order):
/// 1. Pin to the lowest-numbered CPU in the current affinity mask — gives
///    the capture thread a stable home so it isn't migrated by the kernel
///    under heavy load (which would invalidate cache and spike jitter).
/// 2. Try `SCHED_FIFO` priority 30.  This is real-time scheduling and only
///    succeeds if we have `CAP_SYS_NICE`; if it works, the capture thread
///    preempts everything except other real-time tasks.
/// 3. Fall back to `nice(-5)` via `setpriority`, which only requires being
///    able to lower our own priority (always allowed for the owning user).
fn tune_capture_thread() {
    #[cfg(target_os = "linux")]
    unsafe {
        use std::mem::MaybeUninit;

        let cpuset_size = std::mem::size_of::<libc::cpu_set_t>();

        // 1. Pin to the lowest-numbered CPU in the current affinity mask.
        //    `MaybeUninit::zeroed()` gives us a backing store of all-zero
        //    bits (a valid empty cpu_set_t).  We never hand the MaybeUninit
        //    to anyone until the syscall has filled it, and we only
        //    reborrow it as `&cpu_set_t` / `&mut cpu_set_t` for libc calls.
        let mut cur = MaybeUninit::<libc::cpu_set_t>::zeroed();
        if libc::sched_getaffinity(0, cpuset_size, cur.as_mut_ptr()) == 0 {
            let bits = cpuset_size * 8;
            let mut picked: Option<usize> = None;
            for i in 0..bits {
                if libc::CPU_ISSET(i, &*cur.as_ptr()) {
                    picked = Some(i);
                    break;
                }
            }
            if let Some(cpu) = picked {
                let mut pin = MaybeUninit::<libc::cpu_set_t>::zeroed();
                libc::CPU_SET(cpu, &mut *pin.as_mut_ptr());
                if libc::pthread_setaffinity_np(libc::pthread_self(), cpuset_size, &*pin.as_ptr())
                    == 0
                {
                    eprintln!("capture: pinned to CPU {cpu}");
                }
            }
        }

        // 2. Try SCHED_FIFO 30 (real-time; needs CAP_SYS_NICE).
        let param = libc::sched_param { sched_priority: 30 };
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) == 0 {
            eprintln!("capture: SCHED_FIFO priority 30");
        } else {
            // 3. Fall back to nice(-5) via setpriority on our own TID.
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            if libc::setpriority(libc::PRIO_PROCESS, tid, -5) == 0 {
                eprintln!("capture: nice(-5)");
            }
        }
    }
}

// ── Internal capture loop ─────────────────────────────────────────────────────

fn negotiate_format(
    dev: &Device,
    prefer_yuyv: bool,
    target_w: u32,
    target_h: u32,
    forced: Option<(FourCC, u32, u32)>,
) -> Option<v4l::Format> {
    let fourcc_mjpeg = FourCC::new(b"MJPG");
    let fourcc_yuyv = FourCC::new(b"YUYV");

    // Build candidate list: forced entry first (if any), then the waterfall
    let waterfall: &[(FourCC, u32, u32)] = if prefer_yuyv {
        &[(fourcc_yuyv, 640, 480)]
    } else {
        &[
            (fourcc_yuyv, 640, 480),
            (fourcc_mjpeg, target_w, target_h),
            (fourcc_mjpeg, 1280, 720),
            (fourcc_mjpeg, 640, 480),
        ]
    };

    let forced_slice: [(FourCC, u32, u32); 1] =
        forced
            .map(|f| [f])
            .unwrap_or([(FourCC::new(b"MJPG"), 0, 0)]);
    let iter: Box<dyn Iterator<Item = &(FourCC, u32, u32)>> = if forced.is_some() {
        Box::new(forced_slice.iter().chain(waterfall.iter()))
    } else {
        Box::new(waterfall.iter())
    };

    for &(cc, w, h) in iter {
        if w == 0 {
            continue;
        } // skip placeholder
        let Ok(mut fmt) = dev.format() else {
            continue;
        };
        fmt.width = w;
        fmt.height = h;
        fmt.fourcc = cc;
        if dev.set_format(&fmt).is_ok()
            && let Ok(actual) = dev.format()
        {
            let f = actual.fourcc;
            if f == fourcc_mjpeg {
                return Some(actual);
            }
            // Accept YUYV only at 640×480 — the only USB 2.0-safe YUYV resolution at 30fps
            if f == fourcc_yuyv && actual.width <= 640 && actual.height <= 480 {
                return Some(actual);
            }
        }
    }
    None
}

fn capture_loop(
    path: PathBuf,
    user_mode: Option<CapMode>,
    tx: SyncSender<Option<Frame>>,
    fps_out: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
) {
    let debug = std::env::var("ANNOTATOR_DEBUG").is_ok();

    // Pin to a dedicated CPU and try to elevate scheduling priority.  Doing
    // this once at thread start means we stay pinned across all the
    // 'restart iterations (driver close/reopen, format renegotiation).
    tune_capture_thread();
    let fourcc_mjpeg = FourCC::new(b"MJPG");

    // Derive initial negotiation hint from user's chosen mode
    let forced_mode = user_mode.as_ref().map(|m| (m.fourcc(), m.width, m.height));
    let target_width = user_mode.as_ref().map(|m| m.width).unwrap_or(1920);
    let target_height = user_mode.as_ref().map(|m| m.height).unwrap_or(1080);
    let mut prefer_yuyv = user_mode.as_ref().map(|m| !m.is_mjpeg).unwrap_or(false);
    let mut first_attempt = true;

    'restart: loop {
        // Re-open the device on each attempt.  If the previous capture thread is still
        // releasing its V4L2 streaming session, VIDIOC_S_FMT or VIDIOC_STREAMON will
        // return EBUSY — check the stop flag then retry with a short sleep.
        if stop.load(Ordering::Relaxed) {
            return;
        }

        let dev = match Device::with_path(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("capture: open {}: {e}, retrying...", path.display());
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };

        let fmt = match negotiate_format(
            &dev,
            prefer_yuyv,
            target_width,
            target_height,
            if first_attempt { forced_mode } else { None },
        ) {
            Some(f) => f,
            None => {
                eprintln!("capture: negotiate_format failed (device busy?), retrying...");
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };

        // Request the user's chosen fps from the driver, up to 60 (best-effort; driver may adjust)
        let target_fps = user_mode.as_ref().map(|m| m.fps).unwrap_or(60).clamp(1, 60);
        let params = Parameters::with_fps(target_fps);
        if let Err(e) = dev.set_params(&params) {
            eprintln!("capture: could not set {target_fps}fps: {e}");
        } else if let Ok(p) = dev.params() {
            let i = p.interval;
            eprintln!(
                "capture: frame interval set to {}/{}",
                i.numerator, i.denominator
            );
        }

        let is_mjpeg = fmt.fourcc == fourcc_mjpeg;
        let cap_w = fmt.width;
        let cap_h = fmt.height;

        eprintln!(
            "capture: {} {}x{} ({})",
            path.display(),
            cap_w,
            cap_h,
            if is_mjpeg { "MJPEG" } else { "YUYV" }
        );

        // 8 mmap buffers gives ~260 ms of headroom at 30 fps before the kernel
        // has to recycle a buffer the user-space consumer hasn't yet processed.
        // This dramatically reduces the chance of getting stale frames on a
        // busy system under heavy annotation rendering.
        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 8) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("capture: stream start failed (device busy?): {e}, retrying...");
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };
        first_attempt = false;

        // ── Buffer ring ─────────────────────────────────────────────────────
        // Per-frame `Vec<u8>` allocation is a measurable cost at high
        // resolution (6 MB/frame at 1080p RGB).  Keep a small ring of
        // pre-sized buffers; each frame we clear the next slot, fill it,
        // and hand ownership to the consumer via `mem::take`.  The slot's
        // capacity is preserved across iterations — the only allocation
        // is the very first one per format change.
        const RING_SIZE: usize = 3;
        let slot_bytes = (cap_w as usize) * (cap_h as usize) * if is_mjpeg { 3 } else { 2 };
        let mut ring: [Vec<u8>; RING_SIZE] =
            std::array::from_fn(|_| Vec::with_capacity(slot_bytes.max(1)));
        let mut ring_idx: usize = 0;

        let mut frame_count = 0u64;
        let mut drop_count = 0u64;
        let mut decode_errors = 0u64;
        let mut consec_decode_errs = 0u32;
        let mut stat_timer = std::time::Instant::now();

        loop {
            let (buf, _meta) = match stream.next() {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("capture: stream.next() error: {e}");
                    // If the receiver is gone, stop
                    if tx.try_send(None).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            frame_count += 1;

            let decoded = if is_mjpeg {
                decode_mjpeg(buf).map(|(d, w, h)| (d, w, h, FrameFormat::Rgb24))
            } else {
                let slot = &mut ring[ring_idx];
                passthrough_yuyv_into(buf, cap_w, cap_h, slot)
                    .map(|()| (std::mem::take(slot), cap_w, cap_h, FrameFormat::Yuyv))
            };

            match decoded {
                Some((data, fw, fh, fmt)) => {
                    consec_decode_errs = 0;
                    let frame = Frame::from_owned(data, fw, fh, fmt);
                    ring_idx = (ring_idx + 1) % RING_SIZE;
                    match tx.try_send(Some(frame)) {
                        Ok(_) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            drop_count += 1;
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            // Receiver was dropped — device was switched, exit cleanly
                            return;
                        }
                    }
                }
                None => {
                    decode_errors += 1;
                    consec_decode_errs += 1;
                    if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = tx.try_send(None) {
                        return;
                    }

                    // If MJPEG keeps failing, fall back to YUYV and restart the stream
                    if is_mjpeg && consec_decode_errs >= 5 {
                        eprintln!("capture: too many MJPEG decode errors — retrying with YUYV");
                        prefer_yuyv = true;
                        drop(stream);
                        continue 'restart;
                    }
                }
            }

            if stat_timer.elapsed().as_secs_f64() >= 1.0 {
                let secs = stat_timer.elapsed().as_secs_f64().max(0.001);
                let fps = frame_count as f64 / secs;
                fps_out.store((fps * 10.0).round() as u32, Ordering::Relaxed);
                if debug {
                    eprintln!(
                        "capture: frames={} drops={} decode_errors={} (~{:.1} fps)",
                        frame_count, drop_count, decode_errors, fps
                    );
                }
                frame_count = 0;
                drop_count = 0;
                decode_errors = 0;
                stat_timer = std::time::Instant::now();
            }
        } // inner loop
    } // 'restart loop
}

// ── MJPEG decode (pure Rust, no libjpeg) ─────────────────────────────────────

fn decode_mjpeg(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;
    let mut decoder = JpegDecoder::new(data);
    // UVC MJPEG streams are encoded as YCbCr.  Ask for RGB explicitly so
    // SDL2's RGB24 texture receives frame data in the format it expects.
    // zune-jpeg folds the YCbCr→RGB conversion into the IDCT pass on
    // SIMD-capable targets (NEON on aarch64, SSE2/AVX2 on x86_64).
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    decoder.set_options(options);
    let pixels = match decoder.decode() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("capture: MJPEG decode error: {e}");
            return None;
        }
    };
    let info = decoder.info()?;
    Some((pixels, info.width as u32, info.height as u32))
}

// ── YUYV → RGB24 ──────────────────────────────────────────────────────────────

/// Copy the YUYV frame from the mmap'd V4L2 buffer into `slot`, reusing the
/// ring buffer allocated by the capture loop.  Returns `Some(())` on success;
/// `None` if the input buffer is shorter than the expected frame size (which
/// indicates the driver returned a partial frame).
fn passthrough_yuyv_into(data: &[u8], w: u32, h: u32, slot: &mut Vec<u8>) -> Option<()> {
    let expected = (w as usize) * (h as usize) * 2;
    if data.len() < expected {
        return None;
    }
    slot.clear();
    slot.extend_from_slice(&data[..expected]);
    Some(())
}

// ── Screenshot capture ─────────────────────────────────────────────────────────

use sdl2::pixels::PixelFormatEnum;
use sdl2::render::Canvas as SdlCanvas;
use sdl2::video::Window;
use std::fs::File;
use std::time::{SystemTime, UNIX_EPOCH};

/// Save the current canvas content to a PNG file in /tmp/
/// Returns the path to the saved file, or None on failure.
pub fn save_screenshot(
    sdl_canvas: &SdlCanvas<Window>,
    canvas_w: u32,
    canvas_h: u32,
) -> Option<String> {
    // Create PNG encoder
    let mut file = File::create("/tmp/annotator_screenshot.png").ok()?;

    // Get pixels from the canvas (area below menu bar)
    let menu_h = crate::ui::MENU_HEIGHT as u32;
    let capture_h = canvas_h.saturating_sub(menu_h);

    // Read pixels
    let rect = sdl2::rect::Rect::new(0, menu_h as i32, canvas_w, capture_h);
    let rgb_data = sdl_canvas
        .read_pixels(Some(rect), PixelFormatEnum::RGB24)
        .ok()?;

    // Encode as PNG
    let mut encoder = png::Encoder::new(&mut file, canvas_w, capture_h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);

    if let Ok(mut writer) = encoder.write_header()
        && writer.write_image_data(&rgb_data).is_ok()
    {
        // Rename with timestamp
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let path = format!("/tmp/annotate_screenshot_{}.png", timestamp);
        if std::fs::rename("/tmp/annotator_screenshot.png", &path).is_ok() {
            return Some(path);
        }
        return Some("/tmp/annotator_screenshot.png".to_string());
    }
    None
}
