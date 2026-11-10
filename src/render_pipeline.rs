//! Background-rasterized, double-buffered committed-canvas pipeline.
//!
//! Keeps two CPU `Canvas` buffers (`front` / `back`) and swaps them
//! atomically when the worker finishes rasterization, hiding the GPU upload
//! bus transfer behind CPU rasterization of the next frame.
//!
//! Committed-scene rasterization runs off the render thread on a single
//! background worker.  The worker re-renders whenever a new scene snapshot
//! arrives; the main thread always blits the latest `front` canvas without
//! blocking on rasterization.
//!
//! The active stroke is *not* handled here — it stays on the
//! render thread because it changes every frame and there is no benefit
//! to backgrounding it.
//!
//! Synchronization model:
//! - `front_idx: AtomicU8` (0/1) identifies which buffer is currently
//!   displayed.  The worker renders into `front_idx ^ 1` then `swap`s.
//!   Only the worker swaps, and only while holding the `buffers` mutex.
//! - `seq: AtomicU64` is a monotonic completion counter incremented by
//!   the worker after each successful render.  The main thread tracks
//!   the last seq it has seen; if `seq` advanced, a new frame is ready.
//! - `pending: Mutex<Option<PendingFrame>>` holds the latest scene to
//!   render *plus* the base-layer pixels to render on top of; the worker
//!   waits on `cv` for new work.  Carrying the base in the slot means
//!   `submit_scene` never touches the `buffers` mutex — so the main
//!   thread can never stall behind a worker rasterization, and can never
//!   overwrite the displayed front buffer by racing a swap.
//! - `submit_seq` / `rendered_submit`: monotonic submit counter and the
//!   id of the most recently published submit.  The main loop compares
//!   them to learn when a given commit has actually reached the screen.

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
    /// Double-buffered CPU canvases.  Held in a Mutex so the worker
    /// (which copies the rendered result into the back buffer and swaps
    /// the front pointer), the main thread (which clones the front
    /// buffer inside `upload_to()`), and `resize()` (which replaces the
    /// buffers) can each take a `&mut [Canvas; 2]` exclusively.  The
    /// lock is only ever held for a buffer copy/swap — crucially, NOT
    /// for the duration of scene rasterization — so the main thread's
    /// worst-case wait is one 8.3 MB memcpy, not a whole render.
    buffers: Mutex<[Canvas; 2]>,
    front_idx: FrontIndex,
    /// Latest frame to render (replaced atomically by submit()).
    pending: Mutex<Option<PendingFrame>>,
    /// Worker waits on this when `pending` is empty.
    cv: Condvar,
    /// Set to true on shutdown; worker exits its loop on next wake.
    shutdown: Mutex<bool>,
    /// Monotonic completion counter; incremented by the worker after
    /// each successful render.  Read by `poll()`.
    seq: AtomicU64,
    /// Monotonic submit counter; bumped by `submit_scene` and returned
    /// to the caller so it can track which submit carried which commit.
    submit_seq: AtomicU64,
    /// Id of the most recently *published* submit.  Stored by the worker
    /// BEFORE `seq` is bumped, so a `poll()` that observes the new `seq`
    /// also observes this value (Release/Acquire pairing).
    rendered_submit: AtomicU64,
}

/// One submitted frame: the scene snapshot to rasterize plus the
/// base-layer pixels (the input canvas at submit time) underneath it.
struct PendingFrame {
    id: u64,
    scene: Scene,
    width: u32,
    height: u32,
    base: Vec<u8>,
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
            submit_seq: AtomicU64::new(0),
            rendered_submit: AtomicU64::new(0),
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

    /// Submit a new scene snapshot for the worker to render.  Returns
    /// the submit's monotonic id; compare it against
    /// [`Self::rendered_submit`] to learn when this submit has actually
    /// been published to the front buffer.
    ///
    /// Called from the render loop whenever `state.scene.dirty` or the
    /// input canvas is dirty.  Cheap: `Scene::clone()` is a few `Arc`
    /// bumps (see scene.rs), and the base-layer pixel copy happens
    /// outside any lock.
    ///
    /// CRITICAL: this function deliberately does NOT touch the `buffers`
    /// mutex.  The previous implementation copied the input canvas into
    /// the back buffer here, which (a) read `front_idx` outside the
    /// lock, so a concurrent worker swap could redirect the copy into
    /// the *displayed* buffer — the whole annotation layer blinked away
    /// until the next render — and (b) blocked the main thread for the
    /// entire duration of any in-flight worker rasterization, freezing
    /// input processing mid-stroke.  Carrying the base pixels inside the
    /// pending slot removes both hazards by construction.
    pub fn submit_scene(&self, committed_canvas: &Canvas, scene: &Scene) -> u64 {
        let id = self.inner.submit_seq.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut slot = self.inner.pending.lock().unwrap();
            *slot = Some(PendingFrame {
                id,
                scene: (*scene).clone(),
                width: committed_canvas.width,
                height: committed_canvas.height,
                base: committed_canvas.pixels.clone(),
            });
        }
        if DEBUG.load(Ordering::Relaxed) {
            eprintln!(
                "[dbg] submit_scene: id={} objects={} baked_bg={} seq_before={}",
                id,
                scene.objects().len(),
                scene.baked_bg().is_some(),
                self.inner.seq.load(Ordering::Relaxed),
            );
        }
        self.inner.cv.notify_one();
        id
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
        // Everything submitted so far is visible after this synchronous
        // render, so advance the rendered marker alongside `seq`.
        self.inner.rendered_submit.store(
            self.inner.submit_seq.load(Ordering::Acquire),
            Ordering::Release,
        );
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

    /// Id of the most recently published submit.  The main loop uses
    /// this to retire "in-flight" overlays once the frame that contains
    /// them has actually been uploaded to the texture.
    pub fn rendered_submit(&self) -> u64 {
        self.inner.rendered_submit.load(Ordering::Acquire)
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
    /// mutex.  The worker holds that mutex while publishing a frame, so
    /// resize() serializes with it; a frame the worker started before
    /// the resize has the old dimensions and is dropped at publish time
    /// by its buffer-length check, and any not-yet-started frame is
    /// cleared from the pending slot below.
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
        // SAFETY: `resize` is only called from the main thread, and the
        // worker only reads width/height from its `PendingFrame` — these
        // fields exist for the `width()`/`height()` accessors and the
        // resize guard below, so nothing races with this write.
        let inner_mut = Arc::as_ptr(&self.inner) as *mut PipelineInner;
        unsafe {
            (*inner_mut).width = width;
            (*inner_mut).height = height;
            (*inner_mut).seq.store(0, Ordering::Release);
        }
        // Lock released here.  The worker's next publish will see the
        // new width/height when it acquires the buffers mutex.
        drop(buffers);

        // Drop any not-yet-started frame whose base pixels are the old
        // size; the render_into() that follows every resize covers the
        // screen, and the next submit brings a correctly-sized base.
        // (A frame the worker already started is dropped at publish
        // time by its length check against the new buffers.)
        self.inner.pending.lock().unwrap().take();
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
            // Wait for a new frame to render.
            let frame = {
                let mut slot = inner.pending.lock().unwrap();
                loop {
                    if *inner.shutdown.lock().unwrap() {
                        return;
                    }
                    if let Some(f) = slot.take() {
                        break f;
                    }
                    slot = inner.cv.wait(slot).unwrap();
                }
            };

            if DEBUG.load(Ord::Relaxed) {
                eprintln!(
                    "[dbg] worker: woke with frame id={} (objects={} baked_bg={})",
                    frame.id,
                    frame.scene.objects().len(),
                    frame.scene.baked_bg().is_some(),
                );
            }

            // Rasterize WITHOUT holding the buffers mutex.  The previous
            // implementation held that lock for the whole render so the
            // main thread couldn't overwrite the working buffer — but it
            // also meant the main thread's `submit_scene`/`upload_to`
            // could stall for an entire rasterization, freezing input
            // processing mid-stroke.  Rendering into a private canvas
            // shrinks the contended critical section below to a single
            // 8.3 MB copy + pointer swap.
            let render_start = std::time::Instant::now();
            let mut local = Canvas {
                width: frame.width,
                height: frame.height,
                pixels: frame.base,
                dirty: false,
            };
            let rendered = frame.scene.render_to_par(&mut local);
            if rendered {
                local.dirty = true;
            }
            let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;

            // Publish: copy into the back buffer and swap, atomically
            // w.r.t. `upload_to()` and `resize()`.  If a resize swapped
            // in new-sized buffers while we were rendering, this frame
            // is stale — drop it (the resize path's synchronous
            // `render_into` covers the screen until the next submit).
            //
            // A clean-scene submit (`rendered == false`) is dropped too:
            // it carries no new scene content, so publishing its bare
            // base layer would blank the overlay.
            let published = {
                let mut buffers = inner.buffers.lock().unwrap();
                let back_idx = inner.front_idx.get() ^ 1;
                if rendered && local.pixels.len() == buffers[back_idx].pixels.len() {
                    buffers[back_idx].pixels.copy_from_slice(&local.pixels);
                    buffers[back_idx].dirty = true;
                    inner.front_idx.swap();
                    true
                } else {
                    false
                }
            };
            if published {
                // Store the rendered submit id BEFORE bumping `seq` so a
                // `poll()` that observes the new `seq` also observes this
                // value (Release here, Acquire in `poll`/`rendered_submit`).
                inner.rendered_submit.store(frame.id, Ord::Release);
                inner.seq.fetch_add(1, Ord::Release);
            }

            if DEBUG.load(Ord::Relaxed) {
                eprintln!(
                    "[dbg] worker: rendered in {:.1} ms, published={} id={} seq={}",
                    render_ms,
                    published,
                    frame.id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Spin until the worker publishes a frame (bounded so a broken
    /// pipeline fails the test instead of hanging it).
    fn wait_for_frame(p: &RenderPipeline, last: &mut u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if p.poll(last) {
                return true;
            }
            thread::yield_now();
        }
        false
    }

    #[test]
    fn a_submit_is_published_and_marks_its_id() {
        let p = RenderPipeline::new(8, 8);
        let canvas = Canvas::new(8, 8);
        let scene = Scene::new(4);
        let mut seq = 0;

        let id = p.submit_scene(&canvas, &scene);
        assert!(wait_for_frame(&p, &mut seq), "the worker should publish");
        assert!(
            p.rendered_submit() >= id,
            "rendered_submit ({}) should cover the submit id ({id})",
            p.rendered_submit()
        );
    }

    #[test]
    fn submit_ids_are_monotonic() {
        let p = RenderPipeline::new(8, 8);
        let canvas = Canvas::new(8, 8);
        let scene = Scene::new(4);
        let a = p.submit_scene(&canvas, &scene);
        let b = p.submit_scene(&canvas, &scene);
        assert!(b > a);
    }

    #[test]
    fn a_clean_scene_submit_is_not_published() {
        let p = RenderPipeline::new(8, 8);
        let canvas = Canvas::new(8, 8);
        let mut scene = Scene::new(4);
        scene.dirty = false; // nothing new to render
        let mut seq = 0;
        p.submit_scene(&canvas, &scene);
        // Give the worker ample time to (not) publish.
        thread::sleep(Duration::from_millis(200));
        assert!(
            !p.poll(&mut seq),
            "a clean scene carries no new content and must not blank the overlay"
        );
    }

    #[test]
    fn resize_drops_stale_frames_and_the_pipeline_keeps_working() {
        let p = RenderPipeline::new(8, 8);
        let canvas8 = Canvas::new(8, 8);
        let scene = Scene::new(4);
        p.submit_scene(&canvas8, &scene);
        p.resize(16, 16);

        let canvas16 = Canvas::new(16, 16);
        let id = p.submit_scene(&canvas16, &scene);
        let mut seq = 0;
        assert!(
            wait_for_frame(&p, &mut seq),
            "the worker should publish the post-resize frame"
        );
        assert!(p.rendered_submit() >= id);
    }

    #[test]
    fn render_into_marks_everything_submitted_so_far_as_rendered() {
        let p = RenderPipeline::new(8, 8);
        let canvas = Canvas::new(8, 8);
        let scene = Scene::new(4);
        let id = p.submit_scene(&canvas, &scene);
        p.render_into(&canvas, &scene);
        assert!(p.rendered_submit() >= id);
    }
}
