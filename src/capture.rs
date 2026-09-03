use std::{{
    path::PathBuf,
    sync::{{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc,
    }},
    thread,
    time::Duration,
}};

use v4l::{
    buffer::Type,
    format::fourcc::FourCC,
    frameinterval::FrameIntervalEnum,
    framesize::FrameSizeEnum,
    io::traits::CaptureStream,
    prelude::*,
    video::{capture::Parameters, Capture},
};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub enum FrameFormat { Rgb24, Yuyv, Yuv420 }

/// A video frame — either RGB24 (software path), YUYV (V4L2 hardware passthrough),
/// or planar YUV420 (RTSP path, SDL2 IYUV texture).
#[derive(Clone)]
pub struct Frame {
    pub data:   Arc<Vec<u8>>,
    pub width:  u32,
    pub height: u32,
    pub format: FrameFormat,
}

impl Frame {
    /// Convenience: build a Frame that takes ownership of a `Vec<u8>`.
    /// Wraps the buffer in `Arc` so the consumer can hold it cheaply
    /// while the capture thread reuses its ring slot for the next frame.
    pub fn from_owned(data: Vec<u8>, width: u32, height: u32, format: FrameFormat) -> Self {
        Self { data: Arc::new(data), width, height, format }
    }
}

/// A user-selectable capture mode (format + resolution + estimated fps).
#[derive(Clone, Debug)]
pub struct CapMode {
    pub is_mjpeg: bool,
    pub width:    u32,
    pub height:   u32,
    pub fps:      u32,
}

impl CapMode {
    pub fn label(&self) -> String {
        format!("{} {}×{} @{}fps",
            if self.is_mjpeg { "MJPEG" } else { "YUYV" },
            self.width, self.height, self.fps)
    }
    fn fourcc(&self) -> FourCC {
        if self.is_mjpeg { FourCC::new(b"MJPG") } else { FourCC::new(b"YUYV") }
    }
}

/// Describes a detected V4L2 device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path:         PathBuf,
    pub name:         String,
    pub modes:        Vec<CapMode>,
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
        if let Ok(dev) = Device::with_path(&path) {
            if let Ok(caps) = dev.query_caps() {
                let card = caps.card.trim_end_matches('\0').trim().to_string();
                if !card.is_empty() {
                    let modes = query_device_modes(&path);
                    if modes.is_empty() { continue; }  // no usable formats — hide this device
                    let caps_summary = modes.iter().map(|m: &CapMode| m.label()).collect::<Vec<_>>().join("  ·  ");
                    devices.push(DeviceInfo { path: path.clone(), name: card, modes, caps_summary });
                }
            }
        }
    }
    devices
}

/// Query all usable capture modes for a device without starting a stream.
pub fn query_device_modes(path: &PathBuf) -> Vec<CapMode> {
    let Ok(dev) = Device::with_path(path) else { return vec![]; };
    let Ok(formats) = dev.enum_formats() else { return vec![]; };

    let fourcc_mjpeg = FourCC::new(b"MJPG");
    let fourcc_yuyv  = FourCC::new(b"YUYV");
    let mut modes: Vec<CapMode> = Vec::new();

    for &(target_fourcc, is_mjpeg) in &[(fourcc_mjpeg, true), (fourcc_yuyv, false)] {
        if !formats.iter().any(|f| f.fourcc == target_fourcc) { continue; }
        let sizes = dev.enum_framesizes(target_fourcc).unwrap_or_default();
        for fs in &sizes {
            let candidates: Vec<(u32, u32)> = match &fs.size {
                FrameSizeEnum::Discrete(d) => vec![(d.width, d.height)],
                FrameSizeEnum::Stepwise(s) => {
                    [(s.max_width, s.max_height), (1920, 1080), (1280, 720), (640, 480)]
                        .iter()
                        .filter(|&&(w, h)| w >= s.min_width && w <= s.max_width
                                        && h >= s.min_height && h <= s.max_height)
                        .copied().collect()
                }
            };
            const ALLOWED_HEIGHTS: &[u32] = &[480, 720, 800, 1080];
            for (w, h) in candidates {
                if !ALLOWED_HEIGHTS.contains(&h) { continue; }
                // Collect every distinct fps the driver reports for this resolution
                let fps_list: Vec<u32> = match dev.enum_frameintervals(target_fourcc, w, h) {
                    Ok(ivals) if !ivals.is_empty() => {
                        let mut fps_set = std::collections::BTreeSet::new();
                        for fi in &ivals {
                            let fp = match &fi.interval {
                                FrameIntervalEnum::Discrete(frac) if frac.numerator > 0 =>
                                    frac.denominator / frac.numerator,
                                FrameIntervalEnum::Stepwise(s) if s.min.numerator > 0 =>
                                    s.min.denominator / s.min.numerator,
                                _ => 0,
                            };
                            if fp > 0 { fps_set.insert(fp); }
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
                    if !is_mjpeg && fps < 5 { continue; }
                    modes.push(CapMode { is_mjpeg, width: w, height: h, fps });
                }
            }
        }
    }

    modes.sort_by(|a, b| {
        b.is_mjpeg.cmp(&a.is_mjpeg)
            .then((b.width * b.height).cmp(&(a.width * a.height)))
            .then(b.fps.cmp(&a.fps))
    });
    // Dedup only exact duplicates (same format + res + fps); keep different fps as separate modes
    modes.dedup_by(|a, b| a.is_mjpeg == b.is_mjpeg && a.width == b.width && a.height == b.height && a.fps == b.fps);
    modes
}

/// Estimate achievable fps based on format and resolution.
/// MJPEG is compressed so USB bandwidth is rarely a bottleneck — allow up to 60fps.
/// YUYV is raw (2 bytes/pixel) — estimate based on USB 2.0 practical bandwidth (~42 MB/s).
fn estimate_fps(is_mjpeg: bool, w: u32, h: u32) -> u32 {
    if is_mjpeg { return 60; }
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
                if libc::pthread_setaffinity_np(
                    libc::pthread_self(),
                    cpuset_size,
                    &*pin.as_ptr(),
                ) == 0
                {
                    eprintln!("capture: pinned to CPU {cpu}");
                }
            }
        }

        // 2. Try SCHED_FIFO 30 (real-time; needs CAP_SYS_NICE).
        let param = libc::sched_param { sched_priority: 30 };
        if libc::pthread_setschedparam(
            libc::pthread_self(),
            libc::SCHED_FIFO,
            &param,
        ) == 0
        {
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

fn negotiate_format(dev: &Device, prefer_yuyv: bool, target_w: u32, target_h: u32, forced: Option<(FourCC, u32, u32)>) -> Option<v4l::Format> {
    let fourcc_mjpeg = FourCC::new(b"MJPG");
    let fourcc_yuyv  = FourCC::new(b"YUYV");

    // Build candidate list: forced entry first (if any), then the waterfall
    let waterfall: &[(FourCC, u32, u32)] = if prefer_yuyv {
        &[(fourcc_yuyv, 640, 480)]
    } else {
        &[
            (fourcc_yuyv,  640,  480),
            (fourcc_mjpeg, target_w, target_h),
            (fourcc_mjpeg, 1280, 720),
            (fourcc_mjpeg, 640,  480),
        ]
    };

    let forced_slice: [(FourCC, u32, u32); 1] = forced.map(|f| [f]).unwrap_or([(FourCC::new(b"MJPG"), 0, 0)]);
    let iter: Box<dyn Iterator<Item=&(FourCC, u32, u32)>> = if forced.is_some() {
        Box::new(forced_slice.iter().chain(waterfall.iter()))
    } else {
        Box::new(waterfall.iter())
    };

    for &(cc, w, h) in iter {
        if w == 0 { continue; }  // skip placeholder
        let Ok(mut fmt) = dev.format() else { continue; };
        fmt.width  = w;
        fmt.height = h;
        fmt.fourcc = cc;
        if dev.set_format(&fmt).is_ok() {
            if let Ok(actual) = dev.format() {
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
    let target_width  = user_mode.as_ref().map(|m| m.width).unwrap_or(1920);
    let target_height = user_mode.as_ref().map(|m| m.height).unwrap_or(1080);
    let mut prefer_yuyv = user_mode.as_ref().map(|m| !m.is_mjpeg).unwrap_or(false);
    let mut first_attempt = true;

    'restart: loop {
        // Re-open the device on each attempt.  If the previous capture thread is still
        // releasing its V4L2 streaming session, VIDIOC_S_FMT or VIDIOC_STREAMON will
        // return EBUSY — check the stop flag then retry with a short sleep.
        if stop.load(Ordering::Relaxed) { return; }

        let dev = match Device::with_path(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("capture: open {}: {e}, retrying...", path.display());
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };

        let fmt = match negotiate_format(&dev, prefer_yuyv, target_width, target_height,
                                         if first_attempt { forced_mode } else { None }) {
            Some(f) => f,
            None => {
                eprintln!("capture: negotiate_format failed (device busy?), retrying...");
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };

        // Request the user's chosen fps from the driver, up to 60 (best-effort; driver may adjust)
        let target_fps = user_mode.as_ref().map(|m| m.fps).unwrap_or(60).min(60).max(1);
        let params = Parameters::with_fps(target_fps);
        if let Err(e) = dev.set_params(&params) {
            eprintln!("capture: could not set {target_fps}fps: {e}");
        } else if let Ok(p) = dev.params() {
            let i = p.interval;
            eprintln!("capture: frame interval set to {}/{}", i.numerator, i.denominator);
        }

        let is_mjpeg = fmt.fourcc == fourcc_mjpeg;
        let cap_w    = fmt.width;
        let cap_h    = fmt.height;

        eprintln!(
            "capture: {} {}x{} ({})",
            path.display(), cap_w, cap_h,
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
        let slot_bytes = (cap_w as usize) * (cap_h as usize)
            * if is_mjpeg { 3 } else { 2 };
        let mut ring: [Vec<u8>; RING_SIZE] = std::array::from_fn(|_| {
            Vec::with_capacity(slot_bytes.max(1))
        });
        let mut ring_idx: usize = 0;

        let mut frame_count        = 0u64;
        let mut drop_count         = 0u64;
        let mut decode_errors      = 0u64;
        let mut consec_decode_errs = 0u32;
        let mut stat_timer         = std::time::Instant::now();

        loop {
            let (buf, _meta) = match stream.next() {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("capture: stream.next() error: {e}");
                    // If the receiver is gone, stop
                    if tx.try_send(None).is_err() { return; }
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
                        Err(std::sync::mpsc::TrySendError::Full(_)) => { drop_count += 1; }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            // Receiver was dropped — device was switched, exit cleanly
                            return;
                        }
                    }
                }
                None => {
                    decode_errors      += 1;
                    consec_decode_errs += 1;
                    match tx.try_send(None) {
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                        _ => {}
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
                let fps  = frame_count as f64 / secs;
                fps_out.store((fps * 10.0).round() as u32, Ordering::Relaxed);
                if debug {
                    eprintln!(
                        "capture: frames={} drops={} decode_errors={} (~{:.1} fps)",
                        frame_count, drop_count, decode_errors, fps
                    );
                }
                frame_count   = 0;
                drop_count    = 0;
                decode_errors = 0;
                stat_timer    = std::time::Instant::now();
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
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    decoder.set_options(options);
    let pixels = match decoder.decode() {
        Ok(p) => p,
        Err(e) => { eprintln!("capture: MJPEG decode error: {e}"); return None; }
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

use sdl2::render::Canvas as SdlCanvas;
use sdl2::video::Window;
use sdl2::pixels::PixelFormatEnum;
use std::fs::File;
use std::time::{SystemTime, UNIX_EPOCH};

/// Save the current canvas content to a PNG file in /tmp/
/// Returns the path to the saved file, or None on failure.
pub fn save_screenshot(sdl_canvas: &SdlCanvas<Window>, canvas_w: u32, canvas_h: u32) -> Option<String> {
    // Create PNG encoder
    let mut file = File::create("/tmp/annotator_screenshot.png").ok()?;

    // Get pixels from the canvas (area below menu bar)
    let menu_h = crate::ui::MENU_HEIGHT as u32;
    let capture_h = canvas_h.saturating_sub(menu_h);

    // Read pixels
    let rect = sdl2::rect::Rect::new(0, menu_h as i32, canvas_w, capture_h);
    let rgb_data = sdl_canvas.read_pixels(Some(rect), PixelFormatEnum::RGB24).ok()?;

    // Encode as PNG
    let mut encoder = png::Encoder::new(&mut file, canvas_w, capture_h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);

    if let Ok(mut writer) = encoder.write_header() {
        if writer.write_image_data(&rgb_data).is_ok() {
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
    }
    None
}

// ── RTSP Capture via FFmpeg ─────────────────────────────────────────────────────

use std::process::Command;
use std::io::BufReader;

/// RTSP source identifier for the device picker.
#[allow(dead_code)]
pub const RTSP_SOURCE_NAME: &str = "RTSP Stream";

/// Spawn a background thread that continuously captures frames from an RTSP stream
/// using FFmpeg to decode to RGB24.
///
/// Returns a `Receiver` plus an `Arc<AtomicU32>` that the capture thread updates
/// every second with the current capture fps (stored as fps × 10, e.g. 299 = 29.9 fps).
pub fn start_rtsp_capture(
    url: &str,
    width: u32,
    height: u32,
) -> (Receiver<Option<Frame>>, Arc<AtomicU32>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let (tx, rx): (SyncSender<Option<Frame>>, Receiver<Option<Frame>>) = sync_channel(1);
    let fps_counter = Arc::new(AtomicU32::new(0));
    let fps_counter_thread = Arc::clone(&fps_counter);
    let url = url.to_string();

    thread::spawn(move || rtsp_capture_loop(&url, width, height, tx, fps_counter_thread, stop_thread));
    (rx, fps_counter, stop)
}

/// Discover which H.264 hardware decoders this ffmpeg build exposes, in
/// priority order.  Probed once per process by parsing `ffmpeg -decoders`
/// output (cached via `OnceLock`).  Vendor support matrix we search across:
///
/// | Vendor / platform      | Decoder           |
/// |------------------------|-------------------|
/// | Rockchip (Radxa R6A…)  | h264_rkmpp        |
/// | Raspberry Pi VideoCore | h264_mmal         |
/// | NVIDIA                 | h264_cuvid        |
/// | Intel/AMD VA-API       | h264_vaapi        |
/// | Intel QSV              | h264_qsv          |
/// | AMD VDPau              | h264_vdpau        |
/// | Generic V4L2 M2M       | h264_v4l2m2m      |
///
/// `x86` hosts with Intel iGPU/AMD APU typically expose `h264_vaapi` and/or
/// `h264_qsv`; we don't have to know which — we just walk the priority list
/// and pick the first decoder that's actually present.
///
/// Returns the list of viable decoder names in priority order.  The RTSP
/// capture loop tries each in turn and falls back to the next on early
/// failure.  An empty list means "no `-c:v` flag, let ffmpeg auto-pick
/// (software decode)".
fn pick_h264_decoders() -> Vec<String> {
    use std::process::Command;

    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        // Priority order: Rockchip > Pi > NVIDIA > VA-API > QSV > VDPau > V4L2 M2M.
        // Each tuple is (decoder name, vendor tag for logging).
        let candidates: &[(&str, &str)] = &[
            ("h264_rkmpp",    "Rockchip RKMPP"),
            ("h264_mmal",     "Raspberry Pi MMAL"),
            ("h264_cuvid",    "NVIDIA NVDEC"),
            ("h264_vaapi",    "VA-API (Intel/AMD iGPU)"),
            ("h264_qsv",      "Intel Quick Sync"),
            ("h264_vdpau",    "VDPAU"),
            ("h264_v4l2m2m",  "V4L2 mem-to-mem"),
        ];

        // Parse `ffmpeg -decoders` output.  Each line looks like:
        //   " D h264_rkmpp   rkmpp H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 ..."
        //   " D h264_v4l2m2m  V4L2 mem2mem H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 ..."
        // We extract the second whitespace-separated token and compare against
        // each candidate name.
        let decoders_output = Command::new("ffmpeg")
            .arg("-hide_banner")
            .arg("-decoders")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());

        let Some(listing) = decoders_output else {
            eprintln!("[rtsp] could not run `ffmpeg -decoders`; using software H.264 decode");
            return Vec::new();
        };

        // Build a set of available h264* decoder names so we can intersect.
        let mut available: Vec<String> = Vec::new();
        for line in listing.lines() {
            let mut parts = line.split_whitespace();
            // Skip leading flags (e.g. "DEV", "D", "S").
            let Some(first) = parts.next() else { continue };
            if first.starts_with("D") || first.starts_with("V") {
                if let Some(name) = parts.next() {
                    if name.starts_with("h264") {
                        available.push(name.to_string());
                    }
                }
            }
        }

        let mut viable: Vec<String> = Vec::new();
        for (name, vendor) in candidates {
            if available.iter().any(|a| a == name) {
                // Validate the decoder actually works on this host by feeding it
                // a tiny synthetic H.264 stream.  This rules out false positives
                // like `h264_cuvid` listed in -decoders but unusable without an
                // NVIDIA GPU, or `h264_qsv` on a host with no Intel iGPU driver.
                if decoder_works(name) {
                    eprintln!("[rtsp] viable H.264 decoder: {name} ({vendor})");
                    viable.push((*name).to_string());
                } else {
                    eprintln!("[rtsp] {name} ({vendor}) is listed but fails to decode — skipping");
                }
            }
        }

        // Some ffmpeg builds register RKMPP/MMAL via dlopen at runtime and don't
        // show them in `-decoders`.  Probe those at runtime and append any
        // that work (de-duplicated against `viable`).
        let runtime_candidates = ["h264_rkmpp", "h264_mmal"];
        for name in &runtime_candidates {
            if viable.iter().any(|v| v == name) { continue; }
            let out = Command::new("ffmpeg")
                .arg("-hide_banner")
                .arg("-loglevel").arg("error")
                .arg("-f").arg("lavfi")
                .arg("-i").arg("color=c=black:s=64x64:d=0.04")
                .arg("-c:v").arg(name)
                .arg("-f").arg("null")
                .arg("-")
                .output();
            if let Ok(o) = out {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.contains("Unknown decoder") && !stderr.contains("not found") {
                    eprintln!("[rtsp] viable H.264 decoder: {name} (runtime probe)");
                    viable.push((*name).to_string());
                }
            }
        }

        if viable.is_empty() {
            if !available.is_empty() {
                eprintln!(
                    "[rtsp] no priority H.264 hw decoder matched; available: {:?} — using software decode",
                    available
                );
            } else {
                eprintln!("[rtsp] no H.264 hardware decoder found; using software decode");
            }
        }
        viable
    }).clone()
}

/// Verify an H.264 decoder actually works on this host by feeding it a tiny
/// synthetic stream and decoding it to /dev/null.  Used to filter out
/// decoders that are *registered* with ffmpeg but unusable (e.g. cuvid on
/// a non-NVIDIA host, qsv without an Intel iGPU driver loaded).
fn decoder_works(name: &str) -> bool {
    use std::process::Command;
    let out = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel").arg("error")
        .arg("-f").arg("lavfi")
        .arg("-i").arg("color=c=black:s=64x64:d=0.04:r=25")
        .arg("-c:v").arg(name)
        .arg("-f").arg("null")
        .arg("-")
        .output();
    match out {
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("Unknown decoder")
                    || stderr.contains("not found")
                    || stderr.contains("Cannot find")
                    || stderr.contains("could not open")
                    || stderr.contains("Failed to")
                    || stderr.contains("Device creation failed")
                {
                    return false;
                }
                // Unknown failure mode — fall back to software rather than
                // hard-fail.  The watchdog will catch real issues at runtime.
                eprintln!("[rtsp] decoder probe for {name} returned non-zero, stderr: {}",
                    stderr.lines().next().unwrap_or("").trim());
                return false;
            }
            true
        }
        Err(_) => false,
    }
}

/// Internal RTSP capture loop using FFmpeg.  Pipes raw YUV420 frames to
/// stdout — no MJPEG encode, no software JPEG decode on our side.
///
/// Tries each viable H.264 decoder in priority order (from
/// [`pick_h264_decoders`]); on early failure (no frames within
/// `EARLY_FAIL_TIMEOUT`) or immediate decoder error, kills ffmpeg and
/// moves on to the next candidate.  Falls back to software decode
/// (`-c:v` omitted) if no hardware decoder succeeds.
fn rtsp_capture_loop(
    url: &str,
    target_w: u32,
    target_h: u32,
    tx: SyncSender<Option<Frame>>,
    fps_out: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
) {
    let decoders = pick_h264_decoders();

    // Build the candidate list: each hw decoder, then software (None) as
    // final fallback.
    let mut candidates: Vec<Option<String>> = decoders.into_iter().map(Some).collect();
    candidates.push(None);

    for (i, decoder) in candidates.iter().enumerate() {
        if stop.load(std::sync::atomic::Ordering::SeqCst) { return; }

        match decoder {
            Some(name) => eprintln!("[rtsp] attempt {}/{} with {}",
                i + 1, candidates.len(), name),
            None => eprintln!("[rtsp] attempt {}/{} with software decode",
                i + 1, candidates.len()),
        }

        match try_rtsp_attempt(url, target_w, target_h, &tx, &fps_out, &stop,
                               decoder.as_deref()) {
            // The attempt's inner loop runs as long as the user hasn't stopped
            // and frames keep flowing.  It only returns here on stop or
            // early failure; either way we're done with this candidate.
            AttemptResult::EarlyFailure(reason) => {
                eprintln!("[rtsp] decoder failed early: {reason} — trying next");
                continue;
            }
            AttemptResult::Stopped => return,
        }
    }

    eprintln!("[rtsp] all decoder candidates exhausted; giving up");
    let _ = tx.send(None);
}

/// Outcome of a single ffmpeg attempt.  `try_rtsp_attempt` keeps running as
/// long as frames flow and the user hasn't stopped; it only returns on stop
/// or early failure (the success case keeps going forever).
enum AttemptResult {
    /// Decoder didn't produce any frames quickly enough — try a different one.
    EarlyFailure(String),
    /// User requested stop; exit cleanly.
    Stopped,
}

/// Run ffmpeg with the given decoder (None = software fallback) and stream
/// frames until either frames flow successfully, the user stops us, or the
/// decoder fails within the early-failure window.
fn try_rtsp_attempt(
    url: &str,
    target_w: u32,
    target_h: u32,
    tx: &SyncSender<Option<Frame>>,
    fps_out: &Arc<AtomicU32>,
    stop: &Arc<AtomicBool>,
    decoder: Option<&str>,
) -> AttemptResult {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let debug = std::env::var("ANNOTATOR_DEBUG").is_ok();

    // Cap output to MAX_OUT_W on the longest edge.  A 2560x2560 fisheye
    // stream at 25 fps is otherwise unusable on every target CPU we ship to.
    const MAX_OUT_W: u32 = 1920;

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
       .arg("-loglevel").arg("info")
       .arg("-rtsp_transport").arg("tcp");
    if let Some(dec) = decoder {
        cmd.arg("-c:v").arg(dec);
    }
    cmd.arg("-i").arg(url);
    if MAX_OUT_W > 0 {
        cmd.arg("-vf").arg(format!("scale='min({MAX_OUT_W},iw)':-2"));
    }
    cmd.arg("-pix_fmt").arg("yuv420p")
       .arg("-f").arg("rawvideo")
       .arg("pipe:1")
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[rtsp] failed to spawn ffmpeg: {e} (is ffmpeg installed and on PATH?)");
            return AttemptResult::EarlyFailure(format!("spawn failed: {e}"));
        }
    };

    let stdout = child.stdout.take().expect("ffmpeg stdout");
    let stderr = child.stderr.take().expect("ffmpeg stderr");
    let mut stdout = BufReader::with_capacity(1024 * 1024, stdout);

    // Drain ffmpeg's stderr in a background thread.  Without this, the pipe
    // fills (~64KB on Linux) and ffmpeg blocks writing — which looks like
    // "ffmpeg started but never produced frames."
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut err_reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match err_reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        eprintln!("[ffmpeg] {trimmed}");
                    }
                }
                Err(_) => break,
            }
        }
    });

    // For rawvideo YUV420 we know the per-frame size once we know width/height.
    // Wait for ffmpeg to publish its output size via stderr (line containing
    // "Video:") — or use the user-supplied target_w/h as a hint.  Today we
    // trust the caller-supplied target_w/h; the auto-detect path is
    // documented in the comment above but not implemented.
    let frame_w: u32 = target_w;
    let frame_h: u32 = target_h;
    let mut frame_bytes: usize = 0;  // set once frame_w/frame_h are known
    let mut carry: Vec<u8> = Vec::with_capacity(8 * 1024 * 1024); // up to 1920x1080 YUV420

    let mut fps_frame_count: u32 = 0;
    let fps_timer = Instant::now();

    // Early-failure window: if ffmpeg dies within this many seconds and we
    // haven't received a single byte, treat it as a decoder failure and try
    // the next candidate.  Anything beyond this and we trust that the user
    // genuinely has a slow stream / bad network and fall through to the
    // long watchdog.
    let started_at = Instant::now();
    let early_fail_after = Duration::from_secs(5);
    // Long watchdog for legitimate slow streams (e.g. 2560x2560 on RPi3).
    let watchdog_after = Duration::from_secs(15);

    let mut last_frame_at = Instant::now();

    if debug {
        eprintln!("[rtsp] started capture from {} with decoder={:?}",
            url, decoder.unwrap_or("software"));
    }

    let mut chunk = [0u8; 65536];  // larger reads = fewer syscalls

    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        // Early-failure detector.  Only fires before any frames have arrived;
        // once we ship a frame the attempt is considered healthy and we let
        // the inner loop run until the user stops.
        if fps_frame_count == 0 && started_at.elapsed() > early_fail_after {
            if let Ok(Some(_status)) = child.try_wait() {
                let _ = child.kill();
                let _ = child.wait();
                return AttemptResult::EarlyFailure(
                    "ffmpeg exited within 5s without producing a frame".into());
            }
            // ffmpeg is still running but produced nothing in the early
            // window — fall through and let the long watchdog apply.
        }

        // Long watchdog for legitimately slow streams.
        if last_frame_at.elapsed() > watchdog_after && fps_frame_count == 0 {
            eprintln!(
                "[rtsp] no frames received in {}s from {}\n        \
                 check URL, credentials, network reachability, and that the camera supports RTSP",
                watchdog_after.as_secs(), url,
            );
            last_frame_at = Instant::now() - watchdog_after + Duration::from_secs(20);
        }

        // Once we know the frame size, read exactly that many bytes per
        // iteration and ship a frame.  Before that, just accumulate.
        let need = if frame_bytes == 0 {
            usize::MAX  // unknown size yet — drain everything
        } else {
            frame_bytes.saturating_sub(carry.len())
        };

        let to_read = need.min(chunk.len());
        if to_read == 0 && frame_bytes > 0 {
            // We have a complete frame buffered — ship it.
            let data: Vec<u8> = carry.drain(..frame_bytes).collect();
            if tx.try_send(Some(Frame::from_owned(
                data,
                frame_w,
                frame_h,
                FrameFormat::Yuv420,
            ))).is_ok() {
                fps_frame_count += 1;
                let elapsed = fps_timer.elapsed().as_secs_f32();
                if elapsed >= 1.0 {
                    let fps = (fps_frame_count as f32 / elapsed) * 10.0;
                    fps_out.store(fps as u32, std::sync::atomic::Ordering::SeqCst);
                    fps_frame_count = 0;
                }
                last_frame_at = Instant::now();
            }
            continue;
        }

        let bytes_read = match stdout.read(&mut chunk[..to_read]) {
            Ok(0) => {
                // EOF — ffmpeg closed stdout.  If we got frames, this is a
                // normal shutdown; if not, it's a decoder failure.
                let _ = child.kill();
                let _ = child.wait();
                if fps_frame_count == 0 {
                    return AttemptResult::EarlyFailure(
                        "ffmpeg stdout EOF with no frames produced".into());
                }
                let _ = tx.send(None);
                return AttemptResult::Stopped;
            }
            Ok(n) => n,
            Err(e) => {
                eprintln!("[rtsp] read error: {e}");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };

        // Heuristic for auto-detecting frame size: when we see "Output #0"
        // header followed by a "yuv420p" line containing WxH, we know it.
        // For simplicity here, we trust the user-provided target_w/h to be
        // present in the call (start_rtsp_capture passes 0,0 when unknown).
        // When both are 0, wait until ffmpeg's stderr publishes the format.
        if frame_bytes == 0 && frame_w > 0 && frame_h > 0 {
            frame_bytes = (frame_w as usize) * (frame_h as usize) * 3 / 2;
        }
        carry.extend_from_slice(&chunk[..bytes_read]);
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = tx.send(None);
    AttemptResult::Stopped
}
