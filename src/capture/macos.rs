//! macOS capture backend, built on AVFoundation.
//!
//! Linux uses V4L2 (see [`super::linux`]); macOS has no V4L2, so the
//! camera is driven through AVFoundation instead.  The two backends
//! present exactly the same interface to the rest of the application:
//!
//! * [`enumerate_devices`] lists the cameras.
//! * [`start_capture`] spawns a thread that pushes frames into a channel.
//!
//! AVFoundation hands out frames as `CMSampleBuffer`s wrapping a
//! `CVPixelBuffer`.  We ask for 32-bit BGRA (the format CoreVideo
//! produces natively and the one SDL can upload without a conversion
//! pass) and copy the pixels out into an RGB24 buffer, because the
//! pixel buffer's memory is recycled by the capture stack as soon as
//! the delegate returns.
//!
//! The pixel format delivered to the application is therefore always
//! [`FrameFormat::Rgb24`].

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::Duration,
};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVReturnSuccess,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

use super::{CapMode, CaptureStatus, DeviceInfo, Frame, FrameFormat};

/// The pixel format requested from AVFoundation.
///
/// 32-bit BGRA is CoreVideo's native output format, so no conversion
/// happens inside the framework and the copy out of the pixel buffer is
/// a straight memcpy.
const PIXEL_FORMAT_BGRA: u32 = 0x42475241; // kCVPixelFormatType_32BGRA

/// The resolutions the picker offers.
///
/// AVFoundation's presets are what a camera reliably supports; anything
/// finer depends on the individual device, so the list mirrors the
/// Linux backend's allowed heights.
const SUPPORTED_RESOLUTIONS: &[(u32, u32)] = &[
    (1920, 1080),
    (1280, 720),
    (1024, 768),
    (960, 540),
    (640, 480),
];

/// How long the camera may stay silent before it is treated as gone.
///
/// AVFoundation does not report a camera that was unplugged — it simply
/// stops delivering frames.  A camera that has not produced a frame for
/// this long is treated as lost, so the application can tell the user
/// and reconnect when the camera comes back.
///
/// The value is generous: a camera that is merely pointed at a dark
/// scene still delivers frames, so a long silence really does mean the
/// device is gone.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

// ── Device enumeration ────────────────────────────────────────────────────────

/// List the cameras AVFoundation can see.
///
/// The device "path" is the camera's unique ID — a stable string that
/// survives reboots and USB re-plugs, which is what the rest of the
/// application uses to identify a device across restarts.
pub fn enumerate_devices() -> Vec<DeviceInfo> {
    let Some(media) = (unsafe { AVMediaTypeVideo }) else {
        return Vec::new();
    };

    // `devicesWithMediaType:` is deprecated in favour of
    // `AVCaptureDeviceDiscoverySession`, but the discovery session needs
    // an extra delegate type to report the devices it finds, and the
    // deprecated call returns exactly the same list without any of
    // that ceremony.
    #[allow(deprecated)]
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media) };

    let mut out = Vec::new();
    for device in devices.iter() {
        let unique_id = unsafe { device.uniqueID() }.to_string();
        let name = unsafe { device.localizedName() }.to_string();
        if name.trim().is_empty() {
            continue;
        }
        let modes = query_device_modes(&device);
        if modes.is_empty() {
            continue; // no usable formats — hide this device
        }
        let caps_summary = modes
            .iter()
            .map(|m: &CapMode| m.label())
            .collect::<Vec<_>>()
            .join("  ·  ");
        out.push(DeviceInfo {
            path: PathBuf::from(unique_id),
            name,
            modes,
            caps_summary,
        });
    }
    out
}

/// The capture modes a camera supports.
///
/// The device reports the formats it can produce together with the frame
/// rate ranges valid for each.  Those are turned into one mode per
/// (resolution, frame rate) pair.
fn query_device_modes(device: &AVCaptureDevice) -> Vec<CapMode> {
    let mut modes: Vec<CapMode> = Vec::new();

    for format in unsafe { device.formats() }.iter() {
        let dims = unsafe { format.formatDescription() };
        let (w, h) = unsafe {
            let d = objc2_core_media::CMVideoFormatDescriptionGetDimensions(&dims);
            (d.width as u32, d.height as u32)
        };
        if w == 0 || h == 0 {
            continue;
        }
        if !SUPPORTED_RESOLUTIONS.contains(&(w, h)) {
            continue;
        }

        // Every frame rate in every range that is valid for this format.
        let mut fps_set = std::collections::BTreeSet::new();
        for range in unsafe { format.videoSupportedFrameRateRanges() }.iter() {
            let min = unsafe { range.minFrameRate() };
            let max = unsafe { range.maxFrameRate() };
            for fps in [min, max, 15.0, 30.0, 60.0] {
                if fps >= min && fps <= max {
                    fps_set.insert(fps.round() as u32);
                }
            }
        }
        if fps_set.is_empty() {
            fps_set.insert(30);
        }

        for fps in fps_set {
            if fps == 0 {
                continue;
            }
            modes.push(CapMode {
                is_mjpeg: false,
                width: w,
                height: h,
                fps,
            });
        }
    }

    // Best first: the largest picture, then the highest frame rate.
    modes.sort_by(|a, b| {
        (b.width * b.height)
            .cmp(&(a.width * a.height))
            .then(b.fps.cmp(&a.fps))
    });
    modes.dedup_by(|a, b| {
        a.is_mjpeg == b.is_mjpeg && a.width == b.width && a.height == b.height && a.fps == b.fps
    });
    modes
}

// ── The capture delegate ──────────────────────────────────────────────────────

/// The state the delegate hands to the capture thread.
///
/// The delegate is called on AVFoundation's own dispatch queue, so it
/// cannot touch the application's channels directly without
/// synchronisation.  Instead it drops the frame into a lock-protected
/// slot, and the capture thread picks it up.
#[derive(Default)]
struct DelegateIvars {
    /// The most recent frame, waiting to be picked up.
    pending: std::sync::Mutex<Option<Frame>>,
    /// Set when the camera went away mid-stream.
    lost: std::sync::atomic::AtomicBool,
    /// The time the last frame arrived, so the capture thread can tell
    /// a stalled camera from one that is simply idle.
    last_frame: std::sync::Mutex<Option<std::time::Instant>>,
}

define_class!(
    /// The `AVCaptureVideoDataOutputSampleBufferDelegate` that receives
    /// the frames.
    ///
    /// SAFETY:
    /// - The superclass `NSObject` has no subclassing requirements.
    /// - The class does not implement `Drop`.
    #[unsafe(super = NSObject)]
    // The delegate is created on the capture thread and its callbacks
    // arrive on an AVFoundation queue, so it must not be pinned to the
    // main thread.
    #[thread_kind = AnyThread]
    #[ivars = DelegateIvars]
    struct FrameDelegate;

    unsafe impl NSObjectProtocol for FrameDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output_sample_buffer(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let Some(image_buffer) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let pixel_buffer: &CVPixelBuffer = &image_buffer;

            let width = CVPixelBufferGetWidth(pixel_buffer) as u32;
            let height = CVPixelBufferGetHeight(pixel_buffer) as u32;
            let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
            if width == 0 || height == 0 {
                return;
            }

            // The pixel buffer's memory is not directly accessible until
            // it is locked — CoreVideo may keep it on the GPU.  The
            // lock is released again as soon as the copy is done.
            if unsafe {
                CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly)
            } != kCVReturnSuccess
            {
                return;
            }

            let base = CVPixelBufferGetBaseAddress(pixel_buffer);
            let mut rgb = vec![0u8; width as usize * height as usize * 3];
            if !base.is_null() {
                // Copy the pixels out: the buffer belongs to the capture
                // stack and is recycled as soon as this method returns.
                let row_bytes = width as usize * 4;
                let src = base.cast::<u8>();
                for y in 0..height as usize {
                    let src_row = unsafe { src.add(y * bytes_per_row) };
                    let src_row = unsafe { std::slice::from_raw_parts(src_row, row_bytes) };
                    let dst_start = y * width as usize * 3;
                    let dst = &mut rgb[dst_start..dst_start + width as usize * 3];
                    convert_bgra_row(src_row, dst);
                }
            }
            unsafe {
                CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
            }

            if base.is_null() {
                return;
            }

            let frame = Frame::from_owned(rgb, width, height, FrameFormat::Rgb24);
            if let Ok(mut slot) = self.ivars().pending.lock() {
                // The previous frame has not been consumed yet — replace
                // it.  A camera that outruns the renderer drops frames
                // rather than piling up.
                *slot = Some(frame);
            }
            if let Ok(mut stamp) = self.ivars().last_frame.lock() {
                *stamp = Some(std::time::Instant::now());
            }
        }
    }
);

/// Convert one BGRA row into RGB24.
///
/// BGRA and RGB24 differ only in the order of the first three bytes, so
/// this is a per-pixel byte shuffle rather than a colour conversion.
fn convert_bgra_row(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len() / 3 * 4);
    // The source advances four bytes per pixel and the destination
    // three, so they are tracked separately.
    let mut si = 0usize;
    let mut di = 0usize;
    while si + 3 < src.len() {
        dst[di] = src[si + 2]; // R
        dst[di + 1] = src[si + 1]; // G
        dst[di + 2] = src[si]; // B
        si += 4;
        di += 3;
    }
}

// ── Capture thread ────────────────────────────────────────────────────────────

/// Spawn a background thread that continuously captures frames from
/// `device_path` (the camera's unique ID).
///
/// Returns a `Receiver` plus an `Arc<AtomicU32>` that the capture
/// thread updates every second with the current capture fps (stored as
/// fps × 10, e.g. 299 = 29.9 fps).
pub fn start_capture(
    device_path: PathBuf,
    mode: Option<CapMode>,
) -> (
    Receiver<Option<Frame>>,
    Arc<AtomicU32>,
    Arc<AtomicBool>,
    Receiver<CaptureStatus>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let (tx, rx): (SyncSender<Option<Frame>>, Receiver<Option<Frame>>) = sync_channel(1);
    let fps_counter = Arc::new(AtomicU32::new(0));
    let fps_counter_thread = Arc::clone(&fps_counter);
    // The status channel is lossy on purpose: only the *latest* status
    // matters, and a full channel must never block the capture thread.
    let (status_tx, status_rx) = sync_channel(1);
    // Named thread for `htop` / `top` / crash diagnostics.
    let _ = thread::Builder::new()
        .name("avf-capture".into())
        .spawn(move || {
            capture_loop(
                device_path,
                mode,
                tx,
                fps_counter_thread,
                stop_thread,
                status_tx,
            )
        });
    (rx, fps_counter, stop, status_rx)
}

/// Returns a null receiver that always yields `None` (black screen mode).
pub fn null_receiver() -> Receiver<Option<Frame>> {
    let (tx, rx) = sync_channel(1);
    // Send one None so the first recv() doesn't block
    let _ = tx.send(None);
    rx
}

/// A status receiver that never reports anything.
pub fn null_status_receiver() -> Receiver<CaptureStatus> {
    let (_tx, rx) = sync_channel(1);
    rx
}

/// The capture loop.
///
/// The session is (re)opened until it succeeds, the device comes back or
/// the thread is asked to stop.  Frames arrive through the delegate and
/// are handed to the application through the channel.
fn capture_loop(
    path: PathBuf,
    user_mode: Option<CapMode>,
    tx: SyncSender<Option<Frame>>,
    fps_out: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    status_tx: SyncSender<CaptureStatus>,
) {
    let debug = std::env::var("ANNOTATOR_DEBUG").is_ok();

    // Whether the device was streaming the last time we looked.  Used to
    // report each transition exactly once.
    let mut was_streaming = false;
    // Whether the "device is not there" condition has already been
    // reported.
    let mut loss_reported = false;
    // Consecutive open failures — used to throttle the log.
    let mut open_failures: u32 = 0;

    'restart: loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        let session = match open_session(&path, user_mode.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                if !loss_reported {
                    eprintln!("capture: no camera at {}: {e}", path.display());
                    loss_reported = true;
                    let _ = status_tx.try_send(CaptureStatus::DeviceLost);
                }
                if open_failures == 0 || open_failures.is_multiple_of(20) {
                    eprintln!("capture: open {}: {e}, retrying...", path.display());
                }
                open_failures += 1;
                thread::sleep(Duration::from_millis(150));
                continue 'restart;
            }
        };
        open_failures = 0;

        if !was_streaming {
            was_streaming = true;
            loss_reported = false;
            let _ = status_tx.try_send(CaptureStatus::Streaming);
        }

        unsafe {
            session.session.startRunning();
        }
        eprintln!(
            "capture: {} {}x{} @{}fps",
            path.display(),
            user_mode.as_ref().map(|m| m.width).unwrap_or(0),
            user_mode.as_ref().map(|m| m.height).unwrap_or(0),
            user_mode.as_ref().map(|m| m.fps).unwrap_or(0),
        );

        let mut frame_count = 0u64;
        let mut drop_count = 0u64;
        let mut stat_timer = std::time::Instant::now();

        loop {
            if stop.load(Ordering::Relaxed) {
                unsafe { session.session.stopRunning() };
                return;
            }

            // Take the frame the delegate captured.
            let frame = {
                let Ok(mut slot) = session.delegate.ivars().pending.lock() else {
                    break;
                };
                slot.take()
            };

            match frame {
                Some(frame) => {
                    frame_count += 1;
                    match tx.try_send(Some(frame)) {
                        Ok(_) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            // The application has not consumed the previous
                            // frame yet — drop this one.
                            drop_count += 1;
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            return;
                        }
                    }
                }
                None => {
                    // No frame yet.  Check whether the camera vanished.
                    if session.delegate.ivars().lost.swap(false, Ordering::Relaxed) {
                        was_streaming = false;
                        eprintln!("capture: device lost mid-stream: {}", path.display());
                        let _ = status_tx.try_send(CaptureStatus::DeviceLost);
                        unsafe { session.session.stopRunning() };
                        thread::sleep(Duration::from_millis(150));
                        continue 'restart;
                    }

                    // AVFoundation does not tell the application when a
                    // camera is unplugged — it just stops delivering
                    // frames.  A silence longer than this means the
                    // device is gone, so the session is restarted and
                    // the camera comes back when it is plugged in.
                    let stalled = session
                        .delegate
                        .ivars()
                        .last_frame
                        .lock()
                        .ok()
                        .and_then(|s| *s)
                        .map(|t| t.elapsed() > STALL_TIMEOUT)
                        .unwrap_or(false);
                    if stalled {
                        was_streaming = false;
                        eprintln!("capture: device lost mid-stream: {}", path.display());
                        let _ = status_tx.try_send(CaptureStatus::DeviceLost);
                        unsafe { session.session.stopRunning() };
                        thread::sleep(Duration::from_millis(150));
                        continue 'restart;
                    }
                    thread::sleep(Duration::from_millis(4));
                }
            }

            if stat_timer.elapsed().as_secs_f64() >= 1.0 {
                let secs = stat_timer.elapsed().as_secs_f64().max(0.001);
                let fps = frame_count as f64 / secs;
                fps_out.store((fps * 10.0).round() as u32, Ordering::Relaxed);
                if debug {
                    eprintln!("capture: frames={frame_count} drops={drop_count} (~{fps:.1} fps)");
                }
                frame_count = 0;
                drop_count = 0;
                stat_timer = std::time::Instant::now();
            }
        }
    }
}

/// The pieces a running capture session needs.
///
/// They are kept together so they stay alive for as long as the session
/// does: dropping the delegate while the session is running would leave
/// the session vending frames to nothing.
struct Session {
    session: Retained<AVCaptureSession>,
    delegate: Retained<FrameDelegate>,
    /// Kept alive so the queue the delegate is called on is not torn
    /// down while the session is still running.
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

/// Open the camera and wire up the capture session.
fn open_session(path: &Path, mode: Option<&CapMode>) -> Result<Session, String> {
    let unique_id = NSString::from_str(&path.to_string_lossy());
    let Some(device) = (unsafe { AVCaptureDevice::deviceWithUniqueID(&unique_id) }) else {
        return Err("the camera is not connected".into());
    };
    if !unsafe { device.isConnected() } {
        return Err("the camera is not connected".into());
    }

    let input =
        unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.map_err(|e| {
            format!(
                "the camera could not be opened: {}",
                e.localizedDescription()
            )
        })?;

    let (delegate, queue) = make_delegate()?;
    let session = unsafe { AVCaptureSession::new() };
    let output = unsafe { AVCaptureVideoDataOutput::new() };

    unsafe {
        session.beginConfiguration();

        session.addInput(&input);
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        // Ask for BGRA so the pixels can be copied straight out.
        set_pixel_format(&output);
        session.addOutput(&output);

        // The device format is set *after* the input has been added:
        // adding an input makes the session take control of the device
        // and apply its own preset, which would override a format
        // chosen earlier.
        if let Some(m) = mode {
            apply_mode(&device, m);
        }

        session.commitConfiguration();
    }

    Ok(Session {
        session,
        delegate,
        _queue: queue,
    })
}

/// Configure the session for the requested mode.
///
/// The device has to be locked for configuration before its format or
/// frame duration can be changed.
fn apply_mode(device: &AVCaptureDevice, mode: &CapMode) {
    unsafe {
        if device.lockForConfiguration().is_err() {
            // Another application holds the camera; the session will
            // still start, just at whatever the device is set to.
            return;
        }

        // Pick the device format that matches the requested resolution.
        for format in device.formats().iter() {
            let dims = format.formatDescription();
            let d = objc2_core_media::CMVideoFormatDescriptionGetDimensions(&dims);
            if d.width as u32 != mode.width || d.height as u32 != mode.height {
                continue;
            }
            device.setActiveFormat(&format);
            break;
        }

        // Limit the frame rate.  The device clamps this to what the
        // format supports, so it is safe to ask for more than it can do.
        let duration = objc2_core_media::CMTime::new(1, mode.fps.max(1) as i32);
        device.setActiveVideoMinFrameDuration(duration);
        device.setActiveVideoMaxFrameDuration(duration);

        device.unlockForConfiguration();
    }
}

/// Ask the output for 32-bit BGRA frames.
fn set_pixel_format(output: &AVCaptureVideoDataOutput) {
    use objc2_core_video::kCVPixelBufferPixelFormatTypeKey;
    use objc2_foundation::{NSDictionary, NSNumber, NSString};

    // The pixel-format key is a CoreFoundation string; it bridges to the
    // Foundation one without a copy.
    let key: &NSString = unsafe { kCVPixelBufferPixelFormatTypeKey }.as_ref();
    let value: Retained<AnyObject> = NSNumber::numberWithUnsignedInt(PIXEL_FORMAT_BGRA).into();
    let dict: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&[key], &[value]);
    unsafe {
        output.setVideoSettings(Some(&dict));
    }
}

/// Create the delegate object and the queue it is called on.
fn make_delegate() -> Result<
    (
        Retained<FrameDelegate>,
        dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
    ),
    String,
> {
    let delegate: Retained<FrameDelegate> = unsafe {
        let this = FrameDelegate::alloc().set_ivars(DelegateIvars::default());
        msg_send![super(this), init]
    };
    // A serial queue keeps the frames in order; AVFoundation drops frames
    // itself when the queue cannot keep up.
    let queue =
        dispatch2::DispatchQueue::new("annotator.capture", dispatch2::DispatchQueueAttr::SERIAL);
    Ok((delegate, queue))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The camera list can be read without a camera being present.
    ///
    /// A machine with no camera must not make the application fail — the
    /// list is simply empty.
    #[test]
    fn the_camera_list_is_readable() {
        let devices = enumerate_devices();
        for d in &devices {
            assert!(!d.name.trim().is_empty(), "a device must have a name");
            assert!(!d.modes.is_empty(), "a device must have at least one mode");
        }
    }

    /// Every listed camera has a unique identifier.
    #[test]
    fn the_camera_identifiers_are_unique() {
        let devices = enumerate_devices();
        let mut ids: Vec<String> = devices
            .iter()
            .map(|d| d.path.display().to_string())
            .collect();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "the camera identifiers must be unique");
    }

    /// A camera that does not exist is reported as lost, and the thread
    /// keeps retrying so it comes back when the camera is plugged in.
    #[test]
    fn a_missing_camera_is_reported_once_and_the_thread_keeps_retrying() {
        let missing = PathBuf::from("annotator-no-such-camera-id");

        let (rx, _fps, stop, status) = start_capture(missing.clone(), None);

        let reported = status
            .recv_timeout(Duration::from_secs(5))
            .expect("the capture thread must report the lost camera");
        assert_eq!(reported, CaptureStatus::DeviceLost);

        // It must not then keep spamming the status channel.
        assert!(
            status.recv_timeout(Duration::from_millis(400)).is_err(),
            "the loss must be reported once, not repeatedly"
        );

        // And it must still be running, so a reconnected camera works.
        assert!(!stop.load(Ordering::Relaxed));
        drop(rx);

        stop.store(true, Ordering::Relaxed);
    }

    /// A BGRA row is converted to RGB by shuffling the bytes.
    #[test]
    fn a_bgra_row_becomes_rgb() {
        let src = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let mut dst = [0u8; 6];
        convert_bgra_row(&src, &mut dst);
        // BGRA (10,20,30,40) is R=30, G=20, B=10, and
        // BGRA (50,60,70,80) is R=70, G=60, B=50.
        assert_eq!(dst, [30, 20, 10, 70, 60, 50]);
    }
}

/// A live capture test — it is ignored by default because it needs a
/// camera and takes five seconds.
#[cfg(test)]
mod live_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "needs a camera"]
    fn a_camera_delivers_real_frames() {
        let devices = enumerate_devices();
        if devices.is_empty() {
            eprintln!("no camera on this machine — skipping");
            return;
        }
        let dev = &devices[0];
        eprintln!("camera: {} ({})", dev.name, dev.caps_summary);
        let mode = dev.modes.first().cloned();

        let (rx, _fps, stop, _status) = start_capture(dev.path.clone(), mode);
        let start = Instant::now();
        let mut frames = 0u64;
        let mut first: Option<(u32, u32, usize, u64)> = None;
        while start.elapsed() < Duration::from_secs(5) {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Some(f)) => {
                    frames += 1;
                    if first.is_none() {
                        let mut sum = 0u64;
                        for i in (0..f.data.len()).step_by(997) {
                            sum += f.data[i] as u64;
                        }
                        first = Some((f.width, f.height, f.data.len(), sum));
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        stop.store(true, Ordering::Relaxed);

        let (w, h, len, sum) = first.expect("the camera must deliver frames");
        eprintln!("captured {frames} frames; first frame {w}x{h} bytes={len} checksum={sum}");
        assert!(frames > 0, "the camera must deliver frames");
        assert_eq!(len, (w * h * 3) as usize, "the frame must be RGB24");
        assert!(sum > 0, "the frame must contain real pixel data");
    }
}
