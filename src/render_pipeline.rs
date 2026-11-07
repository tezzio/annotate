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

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use sdl2::render::Texture;

use crate::canvas::Canvas;
use crate::scene::Scene;

/// Shared `ANNOTATOR_DEBUG` gate.  `main.rs` flips this on once at startup
/// after reading the env var; `input.rs` and `worker_loop` poll it before
/// each `eprintln!` so the debug cost is one atomic load per site.
pub static DEBUG: AtomicBool = AtomicBool::new(false);

/// Index of which buffer is currently the "front" (display) canvas.
/// `0` → `buffers[0]`, `1` → `buffers[1]`.  Toggled by the worker via
/// `fetch_xor(1)` after each successful render.
struct FrontIndex(AtomicU8);

impl FrontIndex {
    fn new() -> Self {
        Self(AtomicU8::new(0))
    }
    fn get(&self) -> usize {
        self.0.load(Ordering::Acquire) as usize
    }
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
    width: u32,
    height: u32,
    /// Double-buffered CPU canvases.  Held in a Mutex so both the main
    /// thread (which writes the input canvas into the back buffer on
    /// `submit_scene`) and the worker (which writes the rendered result
    /// and swaps the front pointer) can take a `&mut [Canvas; 2]`
    /// exclusively.  The main thread's only contended operations are
    /// the input-canvas copy at `submit_scene` and the snapshot copy
    /// inside `upload_to()`; otherwise the front buffer is stable until
    /// the next swap.
    buffers: Mutex<[Canvas; 2]>,
    front_idx: FrontIndex,
    /// Latest scene snapshot to render (replaced atomically by submit()).
    pending: Mutex<Option<Scene>>,
    /// Worker waits on this when `pending` is empty.
    cv: Condvar,
    /// Set to true on shutdown; worker exits its loop on next wake.
    shutdown: Mutex<bool>,
    /// Monotonic completion counter; incremented by the worker after
    /// each successful render.  Read by `poll()`.
    seq: AtomicU64,
}

impl RenderPipeline {
    /// Spawn the worker thread and allocate two RGBA buffers of size
    /// `width × height`.
    pub fn new(width: u32, height: u32) -> Self {
        let inner = Arc::new(PipelineInner {
            width,
            height,
            buffers: Mutex::new([Canvas::new(width, height), Canvas::new(width, height)]),
            front_idx: FrontIndex::new(),
            pending: Mutex::new(None),
            cv: Condvar::new(),
            shutdown: Mutex::new(false),
            seq: AtomicU64::new(0),
        });

        // Spawn worker.  IMPORTANT: do NOT discard the spawn result with `let _` —
        // on resource-constrained platforms (RPi-class SoCs, Radxa Q6A) the
        // worker can fail to spawn (thread limit, RLIMIT_NPROC) and silently
        // no scene ever gets rasterized, which manifests as "strokes don't
        // commit".  When `ANNOTATOR_DEBUG` is set we surface the error so
        // the operator sees it immediately.
        let worker_inner = Arc::clone(&inner);
        match thread::Builder::new()
            .name("scene-rasterizer".into())
            .spawn(move || worker_loop(worker_inner))
        {
            Ok(_) => {
                if DEBUG.load(Ordering::Relaxed) {
                    eprintln!("[dbg] pipeline worker spawned ok");
                }
            }
            Err(e) => {
                eprintln!("[dbg] FATAL: pipeline worker failed to spawn: {e}");
                eprintln!(
                    "[dbg]   scene rendering will never happen — strokes will appear to not commit."
                );
            }
        }

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
            buffers[back_idx]
                .pixels
                .copy_from_slice(&committed_canvas.pixels);
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
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!(
                "[dbg] submit_scene: objects={} baked_bg={} seq_before={}",
                scene.objects().len(),
                scene.baked_bg().is_some(),
                self.inner.seq.load(Ordering::Relaxed),
            );
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

    /// Pipeline canvas width.  Reserved for callers that need to query
    /// the dimensions without going through the Scene/Canvas handle.
    #[allow(dead_code)]
    pub fn width(&self) -> u32 {
        self.inner.width
    }
    /// Pipeline canvas height.  Reserved for callers that need to query
    /// the dimensions without going through the Scene/Canvas handle.
    #[allow(dead_code)]
    pub fn height(&self) -> u32 {
        self.inner.height
    }

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
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!(
                "[dbg] render_into (sync): objects={} rendered={} seq={}",
                scene.objects().len(),
                rendered,
                self.inner.seq.load(Ordering::Relaxed),
            );
        }
    }

    /// Resize both buffers.  Called from the window-resize handler.
    /// Allocates fresh RGBA storage for both `front` and `back`; the
    /// worker will rasterize them on the next scene submit.
    ///
    /// IMPORTANT: do NOT terminate the worker here.  The previous
    /// implementation set `shutdown = true`, waited 5 ms, replaced the
    /// buffers, then set `shutdown = false` — but the worker had
    /// already exited its loop on the `shutdown = true` check, and
    /// nothing ever respawned it.  Result: after the first resize, the
    /// worker was dead, no further scenes were ever rendered, and the
    /// texture stayed frozen on whatever the last successful upload
    /// contained.  This was the root cause of "strokes don't commit"
    /// on the Radxa Q6A (whose display is 1920×1043, not 1920×1080,
    /// so a Resized event fires immediately after init).
    ///
    /// The correct approach is to replace buffers and update
    /// dimensions atomically w.r.t. the worker, all under the buffers
    /// mutex.  The worker holds that mutex for the duration of its
    /// render+copy, so resize() serializes with it; once resize
    /// releases, the worker's next iteration reads the new dimensions
    /// (Acquire-load, synchronized via the lock release) and picks up
    /// the new buffer sizes on its next render.
    pub fn resize(&self, width: u32, height: u32) {
        let old_w = self.inner.width;
        let old_h = self.inner.height;
        if width == old_w && height == old_h {
            return;
        }

        if DEBUG.load(Ordering::Relaxed) {
            eprintln!("[dbg] pipeline.resize: {old_w}x{old_h} -> {width}x{height}");
        }

        let mut buffers = self.inner.buffers.lock().unwrap();
        *buffers = [Canvas::new(width, height), Canvas::new(width, height)];
        // SAFETY: we hold the buffers mutex; the worker holds the same
        // mutex during its render+copy, so there is no concurrent
        // access to width/height/seq from this point until the lock
        // is released below.
        let inner_mut = Arc::as_ptr(&self.inner) as *mut PipelineInner;
        unsafe {
            (*inner_mut).width = width;
            (*inner_mut).height = height;
            (*inner_mut).seq.store(0, Ordering::Release);
        }
        // Lock released here.  The worker's next iteration will see
        // the new width/height when it acquires the buffers mutex.
    }

    /// Upload the front canvas to a GPU texture.  Used by the main
    /// thread after a successful `poll()`.  Returns `true` if the
    /// texture was updated.
    ///
    /// `width` is read under the buffers mutex (via the buffer's own
    /// `width` field) so it's consistent with the pixels being
    /// uploaded.  Otherwise a concurrent `resize()` could update
    /// `width` after we read it but before the GPU upload completes,
    /// giving the texture a wrong pitch (and on aarch64, possibly a
    /// stale value).
    pub fn upload_to(&self, tex: &mut Texture) -> bool {
        let (pixels, width) = {
            let buffers = self.inner.buffers.lock().unwrap();
            let idx = self.inner.front_idx.get();
            (buffers[idx].pixels.clone(), buffers[idx].width)
        };
        let pitch = (width as usize) * 4;
        tex.update(None, &pixels, pitch).is_ok()
    }
}

// ── Worker thread ────────────────────────────────────────────────────────────

fn worker_loop(inner: Arc<PipelineInner>) {
    use std::sync::atomic::Ordering as Ord;

    if DEBUG.load(Ord::Relaxed) {
        eprintln!(
            "[dbg] worker_loop: started (thread id {:?})",
            thread::current().id()
        );
    }

    // Wrap the loop in catch_unwind so a panic inside `render_to_par`
    // (e.g. rayon misbehaving on a weird CPU topology) doesn't silently
    // kill the worker — leaving the render loop hung on `poll()` and
    // strokes visually never committing.  We log and exit; the main
    // thread's next `pipeline.poll()` will see seq stop advancing and
    // a `[dbg] frame: pipeline_seq=...` summary will show the stall.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            // Wait for a new scene snapshot.
            let scene = {
                let mut slot = inner.pending.lock().unwrap();
                loop {
                    if *inner.shutdown.lock().unwrap() {
                        return;
                    }
                    if let Some(s) = slot.take() {
                        break s;
                    }
                    slot = inner.cv.wait(slot).unwrap();
                }
            };

            if DEBUG.load(Ord::Relaxed) {
                eprintln!(
                    "[dbg] worker: woke with scene (objects={} baked_bg={})",
                    scene.objects().len(),
                    scene.baked_bg().is_some(),
                );
            }

            // Render into the back buffer.  We hold the buffers mutex for
            // the duration of the render so the main thread's submit_scene
            // can't overwrite our working buffer mid-render.  This is a
            // long critical section but contention is rare (the main thread
            // only submits once per scene change, not every frame).
            //
            // CRITICAL: `inner.width` / `inner.height` are read INSIDE the
            // buffers lock scope, not before it.  `resize()` updates these
            // fields under the same lock; reading them under the lock gives
            // us Acquire semantics on the lock-release in resize, so we
            // always see a consistent (buffers, width, height) triple.
            // Reading them outside the lock would race with resize and
            // panic on `copy_from_slice` if our local canvas is the wrong
            // size for the (new) buffers.
            let back_idx = inner.front_idx.get() ^ 1;
            let render_start = std::time::Instant::now();
            let render_ms;
            let new_front;
            {
                let mut buffers = inner.buffers.lock().unwrap();
                let w = inner.width;
                let h = inner.height;
                let mut local = Canvas::new(w, h);
                // Start from whatever's in the back buffer (the main thread
                // may have just copied the input canvas there in submit_scene).
                local.pixels.copy_from_slice(&buffers[back_idx].pixels);
                let rendered = scene.render_to_par(&mut local);
                if rendered {
                    local.dirty = true;
                }
                buffers[back_idx].pixels.copy_from_slice(&local.pixels);
                buffers[back_idx].dirty = true;
                render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
                new_front = inner.front_idx.swap();
            }
            inner.seq.fetch_add(1, Ord::Release);

            // Invoke frame capture callback if one is registered.
            if DEBUG.load(Ord::Relaxed) {
                eprintln!(
                    "[dbg] worker: rendered in {:.1} ms, new_front={} seq={}",
                    render_ms,
                    new_front,
                    inner.seq.load(Ord::Relaxed),
                );
            }
        }
    }));

    if let Err(e) = result {
        eprintln!("[dbg] FATAL: pipeline worker PANICKED: {:?}", e);
        eprintln!("[dbg]   scene rendering has STOPPED.  Strokes will appear to not commit.");
    }
}
