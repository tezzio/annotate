//! Video capture.
//!
//! The application talks to one interface, but the operating systems
//! underneath it are different: Linux exposes cameras through V4L2 and
//! macOS through AVFoundation.  The shared types live here, and the
//! platform-specific code sits in [`linux`] and [`macos`].
//!
//! Both backends implement the same three entry points:
//!
//! * [`enumerate_devices`] — the cameras the machine can see.
//! * [`start_capture`] — a thread that pushes frames into a channel.
//! * [`null_receiver`] / [`null_status_receiver`] — the "no input" case.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{enumerate_devices, null_receiver, null_status_receiver, start_capture};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{enumerate_devices, null_receiver, null_status_receiver, start_capture};

/// The pixel layout of a captured frame.
#[derive(Clone, PartialEq, Debug)]
pub enum FrameFormat {
    /// Three bytes per pixel, red first.
    Rgb24,
    /// Packed YUYV 4:2:2 — two bytes per pixel.
    ///
    /// Only the V4L2 backend produces this format; AVFoundation always
    /// delivers uncompressed RGB.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Yuyv,
}

/// A video frame — either RGB24 (software path) or YUYV (V4L2 hardware
/// passthrough).
#[derive(Clone)]
pub struct Frame {
    pub data: std::sync::Arc<Vec<u8>>,
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
            data: std::sync::Arc::new(data),
            width,
            height,
            format,
        }
    }
}

/// A user-selectable capture mode (format + resolution + estimated fps).
#[derive(Clone, Debug)]
pub struct CapMode {
    /// True when the stream is MJPEG-compressed, false for raw YUYV.
    ///
    /// macOS has no MJPEG mode — AVFoundation always delivers
    /// uncompressed frames — so the flag is false there.
    pub is_mjpeg: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CapMode {
    /// A human-readable description of the mode.
    pub fn label(&self) -> String {
        if self.is_mjpeg {
            format!("MJPEG {}×{} @{}fps", self.width, self.height, self.fps)
        } else {
            format!("YUYV {}×{} @{}fps", self.width, self.height, self.fps)
        }
    }
}

/// Describes a detected capture device.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// The device's identifier: a `/dev/videoN` node on Linux, the
    /// camera's unique ID on macOS.
    pub path: PathBuf,
    pub name: String,
    pub modes: Vec<CapMode>,
    /// Pre-formatted one-liner derived from modes.
    pub caps_summary: String,
}

/// What the capture thread reports back to the main loop.
///
/// The capture thread runs on its own, so the main loop cannot see a
/// device being unplugged until the thread says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    /// The device is streaming normally.
    Streaming,
    /// The device went away — unplugged, or the driver released it.
    /// The capture thread keeps retrying and will report `Streaming`
    /// again once the device comes back.
    DeviceLost,
}

// ── Screenshot capture ────────────────────────────────────────────────────────

use sdl2::pixels::PixelFormatEnum;
use sdl2::render::Canvas as SdlCanvas;
use sdl2::video::Window;
use std::fs::File;
use std::time::{SystemTime, UNIX_EPOCH};

/// Save the current canvas content to a PNG file in /tmp
///
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame carries the pixels it was built with.
    #[test]
    fn a_frame_keeps_its_pixels() {
        let f = Frame::from_owned(vec![1, 2, 3], 1, 1, FrameFormat::Rgb24);
        assert_eq!(f.width, 1);
        assert_eq!(f.height, 1);
        assert_eq!(f.format, FrameFormat::Rgb24);
        assert_eq!(f.data.as_ref(), &[1, 2, 3]);
    }

    /// A mode's label names the format, the size and the frame rate.
    #[test]
    fn a_mode_label_describes_the_mode() {
        let m = CapMode {
            is_mjpeg: false,
            width: 1920,
            height: 1080,
            fps: 60,
        };
        assert!(m.label().contains("1920"));
        assert!(m.label().contains("1080"));
        assert!(m.label().contains("60"));
    }
}
