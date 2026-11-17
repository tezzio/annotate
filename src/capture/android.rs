//! Android capture backend — a placeholder.
//!
//! Android has neither V4L2 nor AVFoundation.  The camera is reached
//! through Camera2, which needs a JNI shim against the NDK's
//! `ACamera` API; that is a piece of work in its own right.
//!
//! Until it exists, the application starts with a black screen and no
//! inputs in the picker, which is enough to exercise everything else:
//! the drawing, the tools, the export and the whole UI.
//!
//! The interface is identical to the other backends, so nothing outside
//! this module has to know that there is no camera yet.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};

use super::{CapMode, CaptureStatus, DeviceInfo, Frame, FrameFormat};

/// List the cameras.
///
/// There are none yet — see the module documentation.
pub fn enumerate_devices() -> Vec<DeviceInfo> {
    Vec::new()
}

/// Start a capture thread.
///
/// There is no camera to capture from, so the thread reports the device
/// as lost and keeps retrying, exactly as the other backends do when a
/// camera is unplugged.  The UI can therefore show the same "device
/// lost" state it would show on a desktop.
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
    let (status_tx, status_rx) = sync_channel(1);

    let _ = std::thread::Builder::new()
        .name("android-capture".into())
        .spawn(move || {
            // Report the loss once, then wait for the thread to be
            // stopped.  The status channel is lossy on purpose.
            let _ = status_tx.try_send(CaptureStatus::DeviceLost);
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
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

/// The frame formats the Android backend can produce.
///
/// Only RGB24 is listed: there is no capture yet, so no other format
/// is ever produced.  The variant exists so the renderer can be
/// written against the same set of formats as the other backends.
#[allow(dead_code)]
const SUPPORTED_FORMATS: [FrameFormat; 1] = [FrameFormat::Rgb24];
