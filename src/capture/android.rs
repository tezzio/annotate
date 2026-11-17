//! Android capture backend — Camera2 through a JNI bridge.
//!
//! Android has neither V4L2 nor AVFoundation: the camera is reached
//! through Camera2, which is a Java API.  The native code cannot call
//! it directly, so `CameraBridge` (in the application's Java sources)
//! does the work and hands the frames over through JNI.
//!
//! The frames arrive as YUV_420_888 — the format every Camera2 device
//! is required to support — and are converted to RGB here, so the rest
//! of the application sees the same thing it sees on the desktop.
//!
//! The interface is identical to the other backends, so nothing outside
//! this module has to know that Android's camera works differently.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};

use jni::objects::{JByteArray, JIntArray, JObject, JObjectArray, JString};

use super::{CapMode, CaptureStatus, DeviceInfo, Frame, FrameFormat};

/// The Java class the camera calls back into.
///
/// The name has to match the one in `CameraBridge.java` exactly.
const BRIDGE_CLASS: &str = "io/tezz/annotator/CameraBridge";

/// The status the Java side reports when the camera is streaming.
const STATUS_STREAMING: i32 = 0;
/// The status the Java side reports when the camera could not be
/// opened.
const STATUS_FAILED: i32 = 1;
/// The status the Java side reports when the camera was disconnected.
const STATUS_DISCONNECTED: i32 = 2;

/// The Android image format for YUV_420_888.
const IMAGE_FORMAT_YUV_420_888: i32 = 35;

/// The state the Java side calls back into.
///
/// The Java code reaches these through JNI, so they have to be
/// `extern "C"` and unmangled.
struct CallbackState {
    /// The channel the frames go into.
    frames: Mutex<Option<SyncSender<Option<Frame>>>>,
    /// The channel the status changes go into.
    status: Mutex<Option<SyncSender<CaptureStatus>>>,
    /// The stop flag, so a pending camera can be closed.
    stop: Arc<AtomicBool>,
}

/// The global the Java callbacks find the state through.
///
/// JNI has no way to pass a pointer to a native function, so the
/// state is kept in a global that the callbacks look up.
static CALLBACK_STATE: Mutex<Option<Arc<CallbackState>>> = Mutex::new(None);

/// The camera that is currently open, so that it can be closed when
/// the capture thread stops.
static OPEN_CAMERA: Mutex<Option<String>> = Mutex::new(None);

// ── The JNI callbacks ────────────────────────────────────────────────────────

/// Called by the Java side when a frame arrives.
///
/// # Safety
///
/// Called from Java with a byte array the JVM owns.
#[unsafe(no_mangle)]
pub extern "C" fn Java_io_tezz_annotator_CameraBridge_nativeOnFrame(
    mut env: jni::JNIEnv,
    _class: JObject<'static>,
    luma: JByteArray<'static>,
    chroma: JByteArray<'static>,
    width: i32,
    height: i32,
) {
    let state = match current_state() {
        Some(s) => s,
        None => return,
    };
    if state.stop.load(Ordering::Relaxed) {
        return;
    }

    // The Java side packs the planes: the luma plane on its own, and
    // the chroma plane as interleaved U,V pairs.
    let Some(luma) = read_bytes(&mut env, &luma) else {
        return;
    };
    let Some(chroma) = read_bytes(&mut env, &chroma) else {
        return;
    };

    let Some(rgb) = yuv420_to_rgb(&luma, &chroma, width as u32, height as u32) else {
        return;
    };

    let frame = Frame::from_owned(rgb, width as u32, height as u32, FrameFormat::Rgb24);
    if let Ok(guard) = state.frames.lock()
        && let Some(tx) = guard.as_ref()
    {
        // The channel holds one frame; the oldest one is dropped so
        // that a slow consumer does not stall the camera.
        let _ = tx.try_send(Some(frame));
    }
}

/// Called by the Java side when the camera's state changes.
///
/// # Safety
///
/// Called from Java with an integer.
#[unsafe(no_mangle)]
pub extern "C" fn Java_io_tezz_annotator_CameraBridge_nativeOnStatus(
    _env: jni::JNIEnv,
    _class: JObject,
    status: i32,
) {
    let Some(state) = current_state() else {
        return;
    };
    let mapped = match status {
        STATUS_STREAMING => CaptureStatus::Streaming,
        STATUS_FAILED => CaptureStatus::DeviceLost,
        STATUS_DISCONNECTED => CaptureStatus::DeviceLost,
        _ => return,
    };
    if let Ok(guard) = state.status.lock()
        && let Some(tx) = guard.as_ref()
    {
        let _ = tx.try_send(mapped);
    }
}

/// The callback state, if there is one.
fn current_state() -> Option<Arc<CallbackState>> {
    CALLBACK_STATE.lock().ok().and_then(|guard| guard.clone())
}

/// Read a byte array into a vector.
///
/// The JNI API uses a signed type for the bytes, so they are widened
/// on the way out.
fn read_bytes(env: &mut jni::JNIEnv, array: &JByteArray) -> Option<Vec<u8>> {
    let len = env.get_array_length(array).ok()? as usize;
    let mut buffer = vec![0i8; len];
    env.get_byte_array_region(array, 0, &mut buffer).ok()?;
    Some(buffer.into_iter().map(|b| b as u8).collect())
}

// ── The JNI helpers ──────────────────────────────────────────────────────────

// SDL's JNI environment accessor.
//
// It is not in the `sdl2` crate's bindings, so it is declared here.
unsafe extern "C" {
    /// The JNI environment of the calling thread, or null.
    fn SDL_AndroidGetJNIEnv() -> *mut std::ffi::c_void;

    /// The activity, or null.
    fn SDL_AndroidGetActivity() -> *mut std::ffi::c_void;
}

/// The JNI environment for the current thread.
///
/// SDL's accessor attaches the thread to the virtual machine when it
/// is not attached already, which is what the capture thread needs.
fn java_env() -> Option<jni::JNIEnv<'static>> {
    // SAFETY: the function returns the environment for the calling
    // thread, or null when there is none.  The environment belongs to
    // the thread and is released when it exits.
    let raw = unsafe { SDL_AndroidGetJNIEnv() };
    if raw.is_null() {
        return None;
    }
    // The environment is already attached, so it is taken over rather
    // than created.
    unsafe { jni::JNIEnv::from_raw(raw as *mut jni::sys::JNIEnv) }.ok()
}

/// The application's context, which the camera manager needs.
fn app_context() -> Option<jni::JNIEnv<'static>> {
    java_env()
}

/// The camera bridge class, kept for the life of the process.
///
/// `FindClass` from a native thread uses the *boot* class loader,
/// which does not have the application's classes — it fails with
/// "ClassNotFoundException: Didn't find class
/// io.tezz.annotator.CameraBridge".  The class is therefore looked up
/// once, on the thread that can find it, and kept as a global
/// reference: a global reference is valid on every thread.
fn bridge_class(env: &mut jni::JNIEnv) -> Option<jni::objects::GlobalRef> {
    static CLASS: std::sync::OnceLock<Option<jni::objects::GlobalRef>> = std::sync::OnceLock::new();

    if let Some(class) = CLASS.get() {
        return class.clone();
    }

    // The class is found through the application's class loader, which
    // is the one the activity was loaded with.
    //
    // The activity is reached through SDL, which hands it out without
    // a class lookup of its own — a `FindClass` would use the boot
    // class loader, which does not have the application's classes.
    let context = activity_context(env)?;
    let loader = env
        .call_method(&context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
        .ok()
        .and_then(|r| r.l().ok())?;

    let name = env.new_string(BRIDGE_CLASS).ok()?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&name).into()],
        )
        .ok()
        .and_then(|r| r.l().ok())?;

    let global = env.new_global_ref(class).ok()?;
    let _ = CLASS.set(Some(global.clone()));
    Some(global)
}

/// List the cameras.
///
/// The cameras are asked for through the Java bridge, because
/// Camera2 cannot be called from Rust.
pub fn enumerate_devices() -> Vec<DeviceInfo> {
    let Some(mut env) = app_context() else {
        return Vec::new();
    };
    let Some(global) = bridge_class(&mut env) else {
        return Vec::new();
    };
    let class = &global;
    let Some(context) = activity_context(&mut env) else {
        return Vec::new();
    };

    // The context is passed as the first argument of every method.
    let Ok(result) = env.call_static_method(
        class,
        "enumerate",
        "(Landroid/content/Context;)[Ljava/lang/Object;",
        &[(&context).into()],
    ) else {
        return Vec::new();
    };
    let Ok(array) = result.l() else {
        return Vec::new();
    };
    let array: JObjectArray = JObjectArray::from(array);
    let Ok(len) = env.get_array_length(&array) else {
        return Vec::new();
    };

    let mut devices = Vec::new();
    for i in 0..len {
        // Four values per camera: the id, the facing direction, the
        // width and the height.
        let Ok(id_obj) = env.get_object_array_element(&array, i) else {
            continue;
        };
        let id_string = JString::from(id_obj);
        let Ok(id) = env.get_string(&id_string) else {
            continue;
        };
        let Ok(facing) = env.get_object_array_element(&array, i + 1) else {
            continue;
        };
        let Ok(facing) = env.call_method(&facing, "intValue", "()I", &[]) else {
            continue;
        };
        let facing = facing.i().unwrap_or(2);
        let Ok(width) = env.get_object_array_element(&array, i + 2) else {
            continue;
        };
        let Ok(width) = env.call_method(&width, "intValue", "()I", &[]) else {
            continue;
        };
        let Ok(height) = env.get_object_array_element(&array, i + 3) else {
            continue;
        };
        let Ok(height) = env.call_method(&height, "intValue", "()I", &[]) else {
            continue;
        };

        let width = width.i().unwrap_or(0) as u32;
        let height = height.i().unwrap_or(0) as u32;
        if width == 0 || height == 0 {
            continue;
        }

        let id = id.to_string_lossy().into_owned();
        let name = match facing {
            0 => format!("Front camera ({id})"),
            1 => format!("Back camera ({id})"),
            _ => format!("Camera ({id})"),
        };

        // The camera can deliver several sizes; the largest one that
        // is not bigger than 1080p is the useful one for a court
        // display, and the smaller ones are offered as alternatives.
        let modes = sizes_for(&mut env, &class, &context, &id)
            .into_iter()
            .map(|(w, h)| CapMode {
                is_mjpeg: false,
                width: w,
                height: h,
                fps: 30,
            })
            .collect::<Vec<_>>();

        let modes = if modes.is_empty() {
            vec![CapMode {
                is_mjpeg: false,
                width,
                height,
                fps: 30,
            }]
        } else {
            modes
        };

        let caps_summary = modes
            .iter()
            .map(|m| m.label())
            .collect::<Vec<_>>()
            .join(", ");

        devices.push(DeviceInfo {
            path: PathBuf::from(id),
            name,
            modes,
            caps_summary,
        });
    }

    devices
}

/// The sizes one camera can deliver.
fn sizes_for(
    env: &mut jni::JNIEnv,
    class: &jni::objects::GlobalRef,
    context: &JObject,
    camera_id: &str,
) -> Vec<(u32, u32)> {
    let Ok(id) = env.new_string(camera_id) else {
        return Vec::new();
    };
    let Ok(result) = env.call_static_method(
        class,
        "sizes",
        "(Landroid/content/Context;Ljava/lang/String;I)[I",
        &[
            (&context).into(),
            (&id).into(),
            IMAGE_FORMAT_YUV_420_888.into(),
        ],
    ) else {
        return Vec::new();
    };
    let Ok(array) = result.l() else {
        return Vec::new();
    };
    let array: JIntArray = JIntArray::from(array);
    let Ok(len) = env.get_array_length(&array) else {
        return Vec::new();
    };
    let mut values = vec![0i32; len as usize];
    if env.get_int_array_region(&array, 0, &mut values).is_err() {
        return Vec::new();
    }
    values
        .chunks_exact(2)
        .map(|pair| (pair[0] as u32, pair[1] as u32))
        .collect()
}

/// Start a capture thread.
///
/// The camera is opened through the Java bridge; the frames arrive
/// through the JNI callback and are put on the channel.
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

    let state = Arc::new(CallbackState {
        frames: Mutex::new(Some(tx.clone())),
        status: Mutex::new(Some(status_tx.clone())),
        stop: Arc::clone(&stop),
    });
    if let Ok(mut guard) = CALLBACK_STATE.lock() {
        *guard = Some(Arc::clone(&state));
    }

    let camera_id = device_path.to_string_lossy().into_owned();
    let (width, height) = match &mode {
        Some(m) => (m.width as i32, m.height as i32),
        None => (1280, 720),
    };

    // The camera is opened from the thread that owns the JNI
    // environment, which is the thread SDL runs the application on.
    // The capture thread below only consumes what arrives.
    open_camera(&camera_id, width, height);

    let _ = std::thread::Builder::new()
        .name("android-capture".into())
        .spawn(move || {
            let mut last_count = 0u32;
            let mut last_time = std::time::Instant::now();
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // The fps counter is updated once a second, in the
                // same units as the other backends: fps × 10.
                let elapsed = last_time.elapsed();
                if elapsed >= std::time::Duration::from_secs(1) {
                    let count = fps_counter_thread.load(Ordering::Relaxed);
                    let frames = count.wrapping_sub(last_count);
                    let fps = frames as f64 / elapsed.as_secs_f64();
                    fps_counter_thread.store((fps * 10.0) as u32, Ordering::Relaxed);
                    last_count = count;
                    last_time = std::time::Instant::now();
                }
            }
            close_camera();
        });

    (rx, fps_counter, stop, status_rx)
}

/// Open a camera through the Java bridge.
fn open_camera(camera_id: &str, width: i32, height: i32) {
    let Some(mut env) = java_env() else {
        return;
    };
    let Some(global) = bridge_class(&mut env) else {
        return;
    };
    let class = &global;
    let Some(context) = activity_context(&mut env) else {
        return;
    };
    let Ok(id) = env.new_string(camera_id) else {
        return;
    };
    let _ = env.call_static_method(
        class,
        "open",
        "(Landroid/content/Context;Ljava/lang/String;II)Z",
        &[(&context).into(), (&id).into(), width.into(), height.into()],
    );
    if let Ok(mut guard) = OPEN_CAMERA.lock() {
        *guard = Some(camera_id.to_string());
    }
}

/// Close the camera that is open.
fn close_camera() {
    if let Some(mut env) = java_env()
        && let Some(global) = bridge_class(&mut env)
    {
        let _ = env.call_static_method(&global, "close", "()V", &[]);
    }
    if let Ok(mut guard) = OPEN_CAMERA.lock() {
        *guard = None;
    }
}

/// The application's context.
///
/// It is the activity, which SDL hands out as a plain object — no
/// class lookup is involved, so it works on any thread.  The camera
/// manager wants a `Context`, and the activity is one.
fn activity_context(env: &mut jni::JNIEnv) -> Option<JObject<'static>> {
    // SAFETY: the function returns a local reference to the activity,
    // or null when there is none.  The reference belongs to the
    // calling thread.
    let raw = unsafe { SDL_AndroidGetActivity() };
    if raw.is_null() {
        return None;
    }
    // SAFETY: the pointer is a JNI object reference that SDL owns.
    Some(unsafe { JObject::from_raw(raw as jni::sys::jobject) })
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

/// Convert a YUV_420_888 frame to RGB24.
///
/// The Y plane is full size; the U and V planes are half as wide and
/// half as tall, and their rows may be padded, so the strides are
/// needed to find the pixels.
///
/// BT.601 studio-swing is used, which is what the camera produces.
fn yuv420_to_rgb(luma: &[u8], chroma: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let w = width as usize;
    let h = height as usize;

    // The luma plane is one byte per pixel, with no padding.
    if luma.len() < w * h {
        return None;
    }

    // The chroma plane is half as wide and half as tall, and holds
    // interleaved U,V pairs.
    let chroma_width = w.div_ceil(2);
    let chroma_height = h.div_ceil(2);
    if chroma.len() < chroma_width * chroma_height * 2 {
        return None;
    }

    let mut out = Vec::with_capacity(w * h * 3);
    let clamp = |x: i32| x.clamp(0, 255) as u8;

    for row in 0..h {
        let y_row = row * w;
        let chroma_row = (row / 2) * chroma_width;
        for col in 0..w {
            let y = luma[y_row + col] as i32;

            let uv = (chroma_row + col / 2) * 2;
            let u = chroma[uv] as i32;
            let v = chroma[uv + 1] as i32;

            // BT.601 studio-swing to full range.
            let c = y - 16;
            let d = u - 128;
            let e = v - 128;
            out.push(clamp((298 * c + 409 * e + 128) >> 8));
            out.push(clamp((298 * c - 100 * d - 208 * e + 128) >> 8));
            out.push(clamp((298 * c + 516 * d + 128) >> 8));
        }
    }

    Some(out)
}

/// The frame formats the Android backend can produce.
///
/// Only RGB24 is listed: Camera2 delivers YUV, which is converted
/// here before the frame reaches the renderer.
#[allow(dead_code)]
const SUPPORTED_FORMATS: [FrameFormat; 1] = [FrameFormat::Rgb24];
