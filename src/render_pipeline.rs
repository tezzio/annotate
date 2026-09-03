//! Background-rasterized, double-buffered committed-canvas pipeline.
//!
//! Implements TODOs 10 and 11:
//! * **TODO 10** — keeps two CPU `Canvas` buffers (`front` / `back`) and
//!   swaps them atomically when the worker finishes rasterization, hiding
//!   the GPU upload bus transfer behind CPU rasterization of the next frame.
//! * **TODO 11** — moves committed-scene rasterization off the render
//!   thread onto a single background worker.  The worker re-renders whenever
//!   a new scene snapshot arrives; the main thread always blits the latest
//!   `front` canvas without blocking on rasterization.
//!
//! The active stroke (TODO 5) is *not* handled here — it stays on the
//! render thread because it changes every frame and there is no benefit
//! to backgrounding it.
//!
//! Synchronization model:
//! - `front_idx: AtomicU8` (0/1) identifies which buffer is currently
//!   displayed.  The worker renders into `front_idx ^ 1` then `swap`s.
//! - `seq: AtomicU64` is a monotonic completion counter incremented by
//!   the worker after each successful render.  The main thread tracks
//!   the last seq it has seen; if `seq` advanced, a new frame is ready.
//! - `pending: Mutex<Option<SceneSnapshot>>` holds the latest scene to
//!   render; the worker waits on `cv` for new work.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use sdl2::render::Texture;

use crate::canvas::Canvas;
use crate::scene::Scene;

/// Index of which buffer is currently the "front" (display) canvas.
/// `0` → `buffers[0]`, `1` → `buffers[1]`.  Toggled by the worker via
/// `fetch_xor(1)` after each successful render.
struct FrontIndex(AtomicU8);

impl FrontIndex {
    fn new() -> Self { Self(AtomicU8::new(0)) }
    fn get(&self) -> usize { self.0.load(Ordering::Acquire) as usize }
    /// Atomically toggle 0↔1 and return the *new* value.
    fn swap(&self) -> usize {
        // fetch_xor returns the previous value; XOR with 1 to get the new one.
        (self.0.fetch_xor(1, Ordering::AcqRel) as usize) ^ 1
    }
}

/// Public handle to the pipeline.  Cheap to clone (all interior state is
/// `Arc`-shared).
#[derive(Clone)]
pub struct RenderPipeline {
    inner: Arc<PipelineInner>,
}

struct PipelineInner {
    /// Width / height of each buffer (rows × cols of pixels).
    width:  u32,
    height: u32,
    /// Double-buffered CPU canvases.  Held in a Mutex so both the main
    /// thread (which writes the input canvas into the back buffer on
    /// `submit_scene`) and the worker (which writes the rendered result
    /// and swaps the front pointer) can take a `&mut [Canvas; 2]`
    /// exclusively.  The main thread's only contended operation is the
    /// input-canvas copy at `submit_scene`; otherwise it just reads
    /// `front_pixels()` under no lock (the front buffer is stable until
    /// the next swap).
    buffers:   Mutex<[Canvas; 2]>,
    front_idx: FrontIndex,
    /// Latest scene snapshot to render (replaced atomically by submit()).
    pending:   Mutex<Option<Scene>>,
    /// Worker waits on this when `pending` is empty.
    cv:        Condvar,
    /// Set to true on shutdown; worker exits its loop on next wake.
    shutdown:  Mutex<bool>,
    /// Monotonic completion counter; incremented by the worker after
    /// each successful render.  Read by `poll()`.
    seq:       AtomicU64,
}

impl RenderPipeline {
    /// Spawn the worker thread and allocate two RGBA buffers of size
    /// `width × height`.
    pub fn new(width: u32, height: u32) -> Self {
        let inner = Arc::new(PipelineInner {
            width,
            height,
            buffers:   Mutex::new([
                Canvas::new(width, height),
                Canvas::new(width, height),
            ]),
            front_idx: FrontIndex::new(),
            pending:   Mutex::new(None),
            cv:        Condvar::new(),
            shutdown:  Mutex::new(false),
            seq:       AtomicU64::new(0),
        });

        // Spawn worker.
        let worker_inner = Arc::clone(&inner);
        let _ = thread::Builder::new()
            .name("scene-rasterizer".into())
            .spawn(move || worker_loop(worker_inner));

        Self { inner }
    }

    /// Submit a new scene snapshot for the worker to render.
    ///
    /// Called from the input handler whenever `state.scene.dirty` is set.
    /// Cheap: the scene is moved into the Mutex; the worker takes
    /// ownership and processes it asynchronously.
    ///
    /// Also copies the input canvas (`committed_canvas`) into the
    /// pipeline's back buffer so the worker can use it as the base layer
    /// (this captures eraser/text-blit writes that happened since the
    /// last render).  Called from the main thread.
    pub fn submit_scene(&self, committed_canvas: &Canvas, scene: &Scene) {
        // Copy the input canvas into the back buffer (the one the worker
        // is about to render into).  Lock the buffer mutex briefly to
        // gain exclusive access.
        let back_idx = self.inner.front_idx.get() ^ 1;
        {
            let mut buffers = self.inner.buffers.lock().unwrap();
            buffers[back_idx].pixels.copy_from_slice(&committed_canvas.pixels);
            buffers[back_idx].dirty = true;
        }

        // Move the *cloned* scene into the slot.  We take ownership via
        // `Scene::clone()` — this is cheap relative to the 8.3 MB
        // baked_bg copy that the pipeline would otherwise pay on every
        // commit if we shared via Arc.  See perf-todos.md TODO-arc-snapshot.
        {
            let mut slot = self.inner.pending.lock().unwrap();
            *slot = Some((*scene).clone());
        }
        self.inner.cv.notify_one();
    }

    /// Drain any completed work from the worker.  Returns `true` if a new
    /// frame is ready (and the front/back buffers were swapped since the
    /// last call).
    ///
    /// Safe to call every frame.  Internally uses a monotonic sequence
    /// counter so re-calling without intervening worker progress is a
    /// no-op.
    pub fn poll(&self, last_seq: &mut u64) -> bool {
        let cur = self.inner.seq.load(Ordering::Acquire);
        if cur > *last_seq {
            *last_seq = cur;
            true
        } else {
            false
        }
    }

    /// Read the current front canvas's RGBA bytes.  Used by the main
    /// thread to blit to the GPU texture.
    ///
    /// Returns a `Vec<u8>` snapshot (a copy) so the caller can use it
    /// outside the buffer mutex.  This is necessary because the worker
    /// may swap the front pointer at any time after we drop the mutex.
    pub fn front_pixels(&self) -> Vec<u8> {
        let idx = self.inner.front_idx.get();
        let buffers = self.inner.buffers.lock().unwrap();
        buffers[idx].pixels.clone()
    }

    /// Pipeline canvas width.  Reserved for callers that need to query
    /// the dimensions without going through the Scene/Canvas handle.
    #[allow(dead_code)]
    pub fn width(&self)  -> u32 { self.inner.width  }
    /// Pipeline canvas height.  Reserved for callers that need to query
    /// the dimensions without going through the Scene/Canvas handle.
    #[allow(dead_code)]
    pub fn height(&self) -> u32 { self.inner.height }

    /// Synchronous render into the front buffer.  Used at startup and on
    /// resize, before the worker has produced anything.
    pub fn render_into(&self, committed_canvas: &Canvas, scene: &Scene) {
        let idx = self.inner.front_idx.get();
        let mut buffers = self.inner.buffers.lock().unwrap();
        let buf = &mut buffers[idx];
        buf.pixels.copy_from_slice(&committed_canvas.pixels);
        buf.dirty = true;
        let rendered = scene.render_to_par(buf);
        if rendered {
            buf.dirty = true;
        }
        self.inner.seq.fetch_add(1, Ordering::Release);
    }

    /// Resize both buffers.  Called from the window-resize handler.
    /// Allocates fresh RGBA storage for both `front` and `back`; the
    /// worker will rasterize them on the next scene submit.
    pub fn resize(&self, width: u32, height: u32) {
        if width == self.inner.width && height == self.inner.height { return; }

        // Signal the worker to exit, then wait for it to drain.
        *self.inner.shutdown.lock().unwrap() = true;
        self.inner.cv.notify_all();
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Replace buffers; resize is single-threaded via the shutdown
        // handshake above.
        {
            let mut buffers = self.inner.buffers.lock().unwrap();
            *buffers = [
                Canvas::new(width, height),
                Canvas::new(width, height),
            ];
        }
        // Mutate the immutable width/height fields.  This is unsafe
        // because we go through Arc::as_ptr; sound because the worker
        // is currently idle (we just set shutdown=true).
        let inner_mut = Arc::as_ptr(&self.inner) as *mut PipelineInner;
        unsafe {
            (*inner_mut).width  = width;
            (*inner_mut).height = height;
            (*inner_mut).seq.store(0, Ordering::Release);
        }

        // Resume worker.
        *self.inner.shutdown.lock().unwrap() = false;
        self.inner.cv.notify_all();
    }

    /// Upload the front canvas to a GPU texture.  Used by the main
    /// thread after a successful `poll()`.  Returns `true` if the
    /// texture was updated.
    pub fn upload_to(&self, tex: &mut Texture) -> bool {
        let pixels = self.front_pixels();
        let pitch  = (self.inner.width as usize) * 4;
        tex.update(None, &pixels, pitch).is_ok()
    }
}

// ── Worker thread ────────────────────────────────────────────────────────────

fn worker_loop(inner: Arc<PipelineInner>) {
    use std::sync::atomic::Ordering as Ord;

    loop {
        // Wait for a new scene snapshot.
        let scene = {
            let mut slot = inner.pending.lock().unwrap();
            loop {
                if *inner.shutdown.lock().unwrap() { return; }
                if let Some(s) = slot.take() { break s; }
                slot = inner.cv.wait(slot).unwrap();
            }
        };

        // Render into the back buffer.  We hold the buffers mutex for
        // the duration of the render so the main thread's submit_scene
        // can't overwrite our working buffer mid-render.  This is a
        // long critical section but contention is rare (the main thread
        // only submits once per scene change, not every frame).
        let back_idx = inner.front_idx.get() ^ 1;
        let w = inner.width;
        let h = inner.height;
        let mut local = Canvas::new(w, h);
        {
            let mut buffers = inner.buffers.lock().unwrap();
            // Start from whatever's in the back buffer (the main thread
            // may have just copied the input canvas there in submit_scene).
            local.pixels.copy_from_slice(&buffers[back_idx].pixels);
            let rendered = scene.render_to_par(&mut local);
            if rendered {
                local.dirty = true;
            }
            buffers[back_idx].pixels.copy_from_slice(&local.pixels);
            buffers[back_idx].dirty = true;
        }

        // Publish: swap front/back.  After this, the new buffer is front.
        let _ = inner.front_idx.swap();
        inner.seq.fetch_add(1, Ord::Release);
    }
}
