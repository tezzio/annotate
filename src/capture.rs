use std::{
    path::PathBuf,
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread,
    time::Duration,
};

use v4l::{
    buffer::Type,
    format::fourcc::FourCC,
    io::traits::CaptureStream,
    prelude::*,
    video::Capture,
};

// ── Public types ──────────────────────────────────────────────────────────────

/// A decoded video frame in RGB24 format (width × height × 3 bytes, row-major).
#[derive(Clone)]
pub struct Frame {
    pub data:   Vec<u8>,
    pub width:  u32,
    pub height: u32,
}

/// Describes a detected V4L2 device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
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
                    devices.push(DeviceInfo { path: path.clone(), name: card });
                }
            }
        }
    }
    devices
}

// ── Capture thread ────────────────────────────────────────────────────────────

/// Spawn a background thread that continuously captures frames from `device_path`.
///
/// Returns a `Receiver` that always holds the **latest** frame.  Old frames are
/// discarded automatically (channel capacity = 1).
pub fn start_capture(
    device_path: PathBuf,
    target_width: u32,
    target_height: u32,
) -> Receiver<Option<Frame>> {
    let (tx, rx): (SyncSender<Option<Frame>>, Receiver<Option<Frame>>) = sync_channel(1);
    thread::spawn(move || capture_loop(device_path, target_width, target_height, tx));
    rx
}

/// Returns a null receiver that always yields `None` (black screen mode).
pub fn null_receiver() -> Receiver<Option<Frame>> {
    let (tx, rx) = sync_channel(1);
    // Send one None so the first recv() doesn't block
    let _ = tx.send(None);
    rx
}

// ── Internal capture loop ─────────────────────────────────────────────────────

fn capture_loop(
    path: PathBuf,
    target_width: u32,
    target_height: u32,
    tx: SyncSender<Option<Frame>>,
) {
    let dev = match Device::with_path(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("capture: failed to open {}: {e}", path.display());
            let _ = tx.send(None);
            return;
        }
    };

    // Negotiate format: prefer MJPEG at the requested resolution, fall back to YUYV
    let fourcc_mjpeg = FourCC::new(b"MJPG");
    let fourcc_yuyv  = FourCC::new(b"YUYV");

    let fmt_to_try = [
        (fourcc_mjpeg, target_width, target_height),
        (fourcc_yuyv,  target_width, target_height),
        (fourcc_yuyv,  640u32, 480u32),
    ];

    let mut chosen_fmt = None;
    for (cc, w, h) in fmt_to_try {
        // Get the current format as a base to modify, skip if unavailable
        let Ok(mut fmt) = dev.format() else { continue; };
        fmt.width  = w;
        fmt.height = h;
        fmt.fourcc = cc;
        if dev.set_format(&fmt).is_ok() {
            if let Ok(actual) = dev.format() {
                if actual.fourcc == cc {
                    chosen_fmt = Some(actual);
                    break;
                }
            }
        }
    }

    let fmt = match chosen_fmt {
        Some(f) => f,
        None => {
            eprintln!("capture: could not negotiate a supported format");
            let _ = tx.send(None);
            return;
        }
    };

    let is_mjpeg = fmt.fourcc == fourcc_mjpeg;
    let cap_w    = fmt.width;
    let cap_h    = fmt.height;

    eprintln!(
        "capture: {} {}x{} ({})",
        path.display(),
        cap_w, cap_h,
        if is_mjpeg { "MJPEG" } else { "YUYV" }
    );

    let mut stream = match MmapStream::with_buffers(&dev, Type::VideoCapture, 4) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("capture: stream error: {e}");
            let _ = tx.send(None);
            return;
        }
    };

    loop {
        let (buf, _meta) = match stream.next() {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("capture: stream.next() error: {e}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        let rgb = if is_mjpeg {
            decode_mjpeg(buf, cap_w, cap_h)
        } else {
            decode_yuyv(buf, cap_w, cap_h)
        };

        match rgb {
            Some(data) => {
                let frame = Frame { data, width: cap_w, height: cap_h };
                // send_to: if channel full, just drop the old frame (non-blocking)
                let _ = tx.try_send(Some(frame));
            }
            None => {
                let _ = tx.try_send(None);
            }
        }
    }
}

// ── MJPEG decode (pure Rust, no libjpeg) ─────────────────────────────────────

fn decode_mjpeg(data: &[u8], _w: u32, _h: u32) -> Option<Vec<u8>> {
    use jpeg_decoder::Decoder;
    let mut decoder = Decoder::new(data);
    match decoder.decode() {
        Ok(pixels) => {
            let info = decoder.info()?;
            // jpeg-decoder may return RGB or Luma; normalise to RGB24
            let rgb = match info.pixel_format {
                jpeg_decoder::PixelFormat::RGB24 => pixels,
                jpeg_decoder::PixelFormat::L8 => {
                    // greyscale → replicate to RGB
                    pixels.iter().flat_map(|&v| [v, v, v]).collect()
                }
                _ => {
                    eprintln!("capture: unsupported JPEG pixel format");
                    return None;
                }
            };
            Some(rgb)
        }
        Err(e) => {
            eprintln!("capture: MJPEG decode error: {e}");
            None
        }
    }
}

// ── YUYV → RGB24 ──────────────────────────────────────────────────────────────

fn decode_yuyv(data: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let pixels = (w * h) as usize;
    if data.len() < pixels * 2 {
        return None;
    }
    let mut rgb = Vec::with_capacity(pixels * 3);
    let mut i = 0usize;
    while i + 3 < data.len() {
        let y0 = data[i]     as i32;
        let u  = data[i + 1] as i32 - 128;
        let y1 = data[i + 2] as i32;
        let v  = data[i + 3] as i32 - 128;

        for y in [y0, y1] {
            let r = clamp_u8(y + 1402 * v / 1000);
            let g = clamp_u8(y - 344  * u / 1000 - 714 * v / 1000);
            let b = clamp_u8(y + 1772 * u / 1000);
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
        i += 4;
    }
    Some(rgb)
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
