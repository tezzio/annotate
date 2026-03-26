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
pub enum FrameFormat { Rgb24, Yuyv }

/// A video frame — either RGB24 (from MJPEG decode) or raw YUYV (passed through for GPU conversion).
#[derive(Clone)]
pub struct Frame {
    pub data:   Vec<u8>,
    pub width:  u32,
    pub height: u32,
    pub format: FrameFormat,
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
    thread::spawn(move || capture_loop(device_path, mode, tx, fps_counter_thread, stop_thread));
    (rx, fps_counter, stop)
}

/// Returns a null receiver that always yields `None` (black screen mode).
pub fn null_receiver() -> Receiver<Option<Frame>> {
    let (tx, rx) = sync_channel(1);
    // Send one None so the first recv() doesn't block
    let _ = tx.send(None);
    rx
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

        let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("capture: stream start failed (device busy?): {e}, retrying...");
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };
        first_attempt = false;

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
                passthrough_yuyv(buf, cap_w, cap_h).map(|(d, w, h)| (d, w, h, FrameFormat::Yuyv))
            };

            match decoded {
                Some((data, fw, fh, fmt)) => {
                    consec_decode_errs = 0;
                    let frame = Frame { data, width: fw, height: fh, format: fmt };
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
    use jpeg_decoder::Decoder;
    let mut decoder = Decoder::new(data);
    let pixels = match decoder.decode() {
        Ok(p) => p,
        Err(e) => { eprintln!("capture: MJPEG decode error: {e}"); return None; }
    };
    let info = decoder.info()?;
    let w = info.width  as u32;
    let h = info.height as u32;
    // jpeg-decoder may return RGB24 or Luma; normalise to RGB24
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => pixels.iter().flat_map(|&v| [v, v, v]).collect(),
        _ => { eprintln!("capture: unsupported JPEG pixel format"); return None; }
    };
    Some((rgb, w, h))
}

// ── YUYV → RGB24 ──────────────────────────────────────────────────────────────

/// Pass YUYV bytes through raw — SDL/GPU will do the YUV→RGB conversion.
fn passthrough_yuyv(data: &[u8], w: u32, h: u32) -> Option<(Vec<u8>, u32, u32)> {
    let expected = (w * h) as usize * 2;
    if data.len() < expected {
        return None;
    }
    Some((data[..expected].to_vec(), w, h))
}
