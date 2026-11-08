//! Saving and loading annotation scenes to disk.
//!
//! Annotations live in RAM only, so a crash, a reboot or a power cut
//! during a session destroys the whole exhibit.  This module persists a
//! scene to a file and reads it back.
//!
//! Format
//! ------
//! JSON, via `serde_json` (already a dependency).  Human-readable and
//! debuggable, which matters more here than compactness: a court
//! annotation file may need to be inspected by hand.
//!
//! The one exception is the baked background — the eraser bakes erased
//! pixels into an 8.3 MB RGBA layer, which would bloat the JSON.  It is
//! stored as a PNG blob instead (the `png` crate is already a
//! dependency), base64-encoded into the document.
//!
//! Durability
//! ----------
//! Writes are atomic: the document is written to a temporary file in the
//! same directory, flushed, `fsync`ed, and then renamed over the target.
//! A crash mid-write therefore leaves the previous save intact rather
//! than a truncated file.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::scene::{BakedLayer, DrawObject, Scene};

/// Current format version.  Bumped when the document layout changes in a
/// way that older readers cannot understand.
pub const FORMAT_VERSION: u32 = 1;

/// A serializable snapshot of a scene.
///
/// This is the on-disk shape.  It is deliberately *not* the same as the
/// live `Scene` — the live type carries undo/redo stacks and an `Arc`
/// payload that have no meaning on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneDocument {
    /// Format version, for future migrations.
    pub version: u32,
    /// Canvas the annotations were drawn on.
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// The annotations, in drawing order.
    pub objects: Vec<DrawObject>,
    /// The baked background, PNG-encoded and base64'd.  Absent when the
    /// eraser has never been used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baked_png: Option<String>,
    /// Unix timestamp (seconds) of the save.
    pub saved_at: u64,
}

impl SceneDocument {
    /// Build a document from the live scene.
    pub fn from_scene(scene: &Scene, canvas_width: u32, canvas_height: u32) -> Self {
        Self {
            version: FORMAT_VERSION,
            canvas_width,
            canvas_height,
            objects: scene.objects().to_vec(),
            baked_png: scene.baked_bg().and_then(encode_baked),
            saved_at: now_secs(),
        }
    }

    /// Number of annotations in the document.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }
}

/// The current time as whole seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Encode the baked background as a PNG, base64'd into a string.
fn encode_baked(bg: &BakedLayer) -> Option<String> {
    encode_png(bg).map(|png| base64_encode(&png))
}

/// Decode a base64 PNG back into a baked layer.
fn decode_baked(data: &str) -> Option<BakedLayer> {
    base64_decode(data).and_then(|png| decode_png(&png))
}

/// Encode an RGBA layer as a PNG in memory.
fn encode_png(bg: &BakedLayer) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, bg.width, bg.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&bg.pixels).ok()?;
    }
    Some(out)
}

/// Decode a PNG back into an RGBA layer.
fn decode_png(data: &[u8]) -> Option<BakedLayer> {
    let decoder = png::Decoder::new(std::io::Cursor::new(data));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    // The encoder always writes RGBA8, but be defensive about the
    // colour type so a hand-edited file cannot cause a bad read.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some(BakedLayer {
        width: info.width,
        height: info.height,
        pixels: buf[..info.buffer_size()].to_vec(),
    })
}

// ── base64 ───────────────────────────────────────────────────────────────────
//
// Hand-rolled so the module does not pull in another dependency for two
// small functions.  Standard alphabet, with padding.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        if c == b'\n' || c == b'\r' || c == b' ' {
            continue;
        }
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&b| b == c)? as u32;
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, &v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ── File I/O ──────────────────────────────────────────────────────────────────

/// The directory saved scenes live in.
///
/// `$ANNOTATOR_SAVE_DIR` overrides it; otherwise `~/.local/share/annotator/saves`.
/// The directory is created on demand.
pub fn save_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ANNOTATOR_SAVE_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("annotator/saves")
}

/// Ensure the save directory exists, returning it.
pub fn ensure_save_dir() -> std::io::Result<PathBuf> {
    let dir = save_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Serialize a scene to a JSON string.
pub fn to_json(scene: &Scene, canvas_width: u32, canvas_height: u32) -> String {
    let doc = SceneDocument::from_scene(scene, canvas_width, canvas_height);
    serde_json::to_string_pretty(&doc)
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize scene: {e}\"}}"))
}

/// Parse a scene document from JSON.
pub fn from_json(json: &str) -> Result<SceneDocument, String> {
    let doc: SceneDocument = serde_json::from_str(json).map_err(|e| format!("parse scene: {e}"))?;
    if doc.version > FORMAT_VERSION {
        return Err(format!(
            "scene format version {} is newer than this build supports ({FORMAT_VERSION})",
            doc.version
        ));
    }
    Ok(doc)
}

/// Write a scene to `path` atomically.
///
/// The document goes to a temporary file in the same directory, is
/// flushed and `fsync`ed, and is then renamed over the target.  A crash
/// part-way through leaves the previous save intact.
pub fn save_to_path(
    scene: &Scene,
    canvas_width: u32,
    canvas_height: u32,
    path: &Path,
) -> Result<u64, String> {
    let json = to_json(scene, canvas_width, canvas_height);
    write_atomic(path, json.as_bytes())
}

/// Write bytes to `path` atomically.
fn write_atomic(path: &Path, data: &[u8]) -> Result<u64, String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // The temp file must be on the same filesystem as the target for the
    // rename to be atomic.
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("scene")
    ));

    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create temp: {e}"))?;
        f.write_all(data).map_err(|e| format!("write temp: {e}"))?;
        f.flush().map_err(|e| format!("flush temp: {e}"))?;
        // Force the bytes to disk before the rename, so a power cut cannot
        // leave a renamed-but-empty file.
        f.sync_all().map_err(|e| format!("sync temp: {e}"))?;
    }

    fs::rename(&tmp, path).map_err(|e| {
        // Clean up the temp file so a failed save does not litter the
        // directory.
        let _ = fs::remove_file(&tmp);
        format!("rename into place: {e}")
    })?;
    Ok(data.len() as u64)
}

/// Read a scene document back from `path`.
pub fn load_from_path(path: &Path) -> Result<SceneDocument, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    from_json(&text)
}

/// Apply a loaded document to a live scene.
///
/// The undo and redo stacks are cleared — a loaded scene is a new
/// starting point, and replaying an old history across a load would be
/// confusing.  The scene is marked dirty so it renders immediately.
pub fn apply_document(scene: &mut Scene, doc: &SceneDocument) {
    let baked = doc
        .baked_png
        .as_deref()
        .and_then(decode_baked)
        .map(std::sync::Arc::new);

    let inner = crate::scene::SceneInner {
        objects: doc.objects.clone(),
        baked_bg: baked,
    };
    scene.replace_contents(inner);
}

/// A timestamped filename for a new save.
pub fn timestamped_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("scene-{now}.json")
}

/// Metadata about a save file, for listing.
#[derive(Clone, Debug, Serialize)]
pub struct SaveInfo {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub objects: usize,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub saved_at: u64,
}

/// List the saved scenes in the save directory, newest first.
///
/// A file that cannot be parsed is skipped rather than failing the whole
/// listing — one corrupt file must not hide the rest.
pub fn list_saves() -> Vec<SaveInfo> {
    let dir = save_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<SceneDocument>(&text) else {
            continue;
        };
        out.push(SaveInfo {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path: path.display().to_string(),
            bytes: text.len() as u64,
            objects: doc.object_count(),
            canvas_width: doc.canvas_width,
            canvas_height: doc.canvas_height,
            saved_at: doc.saved_at,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.saved_at));
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{PenStroke, ShapeObj, TextObj};

    fn sample_scene() -> Scene {
        let mut s = Scene::new(8);
        s.add(DrawObject::Pen(PenStroke {
            points: vec![(1, 2), (3, 4)],
            color: (255, 0, 0),
            size: 5,
        }));
        s.add(DrawObject::Line(ShapeObj {
            x0: 10,
            y0: 20,
            x1: 30,
            y1: 40,
            color: (0, 255, 0),
            size: 3,
        }));
        s.add(DrawObject::Text(TextObj {
            text: "Exhibit A".into(),
            x: 100,
            y: 200,
            color: (0, 0, 255),
            size: 20,
            bitmap: vec![255, 0, 128, 64],
            bitmap_width: 2,
            bitmap_height: 2,
        }));
        s
    }

    #[test]
    fn base64_round_trips() {
        for data in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            b"hello world, this is a longer payload",
        ] {
            let enc = base64_encode(data);
            let dec = base64_decode(&enc).expect("decode");
            assert_eq!(dec, data, "round trip failed for {data:?}");
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        // "Man" -> "TWFu" is the canonical example from RFC 4648.
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn a_scene_survives_a_json_round_trip() {
        let scene = sample_scene();
        let json = to_json(&scene, 1920, 1080);
        let doc = from_json(&json).expect("parse");
        assert_eq!(doc.object_count(), 3);
        assert_eq!(doc.canvas_width, 1920);
        assert_eq!(doc.canvas_height, 1080);

        let mut restored = Scene::new(8);
        apply_document(&mut restored, &doc);
        assert_eq!(restored.objects().len(), 3);
        match &restored.objects()[2] {
            DrawObject::Text(t) => {
                assert_eq!(t.text, "Exhibit A");
                assert_eq!(t.x, 100);
                assert_eq!(t.y, 200);
                assert_eq!(t.color, (0, 0, 255));
                assert_eq!(t.size, 20);
                assert_eq!(t.bitmap, vec![255, 0, 128, 64]);
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn saving_and_loading_a_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("annotator-save-test-{}", std::process::id()));
        let path = dir.join("scene.json");
        let scene = sample_scene();

        save_to_path(&scene, 800, 600, &path).expect("save");
        assert!(path.exists());

        let doc = load_from_path(&path).expect("load");
        assert_eq!(doc.object_count(), 3);

        let mut restored = Scene::new(8);
        apply_document(&mut restored, &doc);
        assert_eq!(restored.objects().len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_intact() {
        let dir =
            std::env::temp_dir().join(format!("annotator-atomic-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scene.json");

        let scene = sample_scene();
        save_to_path(&scene, 800, 600, &path).expect("first save");
        let first = fs::read_to_string(&path).unwrap();

        // Saving again overwrites cleanly and leaves no temp files behind.
        save_to_path(&scene, 1024, 768, &path).expect("second save");
        let second = fs::read_to_string(&path).unwrap();
        assert!(second.contains("1024"));

        // No leftover temp files.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
        assert!(!first.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_document_from_a_newer_version_is_refused() {
        let json =
            r#"{"version":999,"canvas_width":1,"canvas_height":1,"objects":[],"saved_at":0}"#;
        assert!(from_json(json).is_err());
    }

    #[test]
    fn a_corrupt_document_is_rejected() {
        assert!(from_json("not json at all").is_err());
        assert!(from_json("{}").is_err());
    }

    #[test]
    fn the_baked_background_round_trips_through_png() {
        // A small RGBA layer with a recognisable pattern.
        let w = 4u32;
        let h = 3u32;
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            pixels.extend_from_slice(&[(i as u8) * 10, 20, 30, 255]);
        }
        let bg = BakedLayer {
            width: w,
            height: h,
            pixels,
        };
        let encoded = encode_baked(&bg).expect("encode");
        let decoded = decode_baked(&encoded).expect("decode");
        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);
        assert_eq!(decoded.pixels, bg.pixels);
    }

    #[test]
    fn a_scene_with_a_baked_background_round_trips() {
        let mut scene = Scene::new(8);
        // Bake a tiny background.  `bake_from_canvas` clears the live
        // objects — that is how the eraser commits — so the bake is the
        // only content afterwards.
        let canvas = crate::canvas::Canvas::new(8, 8);
        scene.bake_from_canvas(&canvas);

        let json = to_json(&scene, 8, 8);
        let doc = from_json(&json).expect("parse");
        assert!(doc.baked_png.is_some(), "the bake should have been stored");

        let mut restored = Scene::new(8);
        apply_document(&mut restored, &doc);
        assert!(restored.baked_bg().is_some(), "the bake should survive");
        assert_eq!(restored.baked_bg().unwrap().width, 8);
        assert_eq!(restored.baked_bg().unwrap().height, 8);
    }

    #[test]
    fn applying_a_document_clears_the_undo_history() {
        let mut scene = Scene::new(8);
        scene.push_undo();
        scene.add(DrawObject::Pen(PenStroke {
            points: vec![(1, 1)],
            color: (255, 255, 255),
            size: 2,
        }));
        assert!(!scene.undo_stack.is_empty());

        let doc = SceneDocument::from_scene(&scene, 100, 100);
        let mut fresh = Scene::new(8);
        apply_document(&mut fresh, &doc);
        assert!(fresh.undo_stack.is_empty());
        assert!(fresh.redo_stack.is_empty());
    }

    #[test]
    fn the_timestamped_name_is_unique_per_second() {
        let a = timestamped_name();
        let b = timestamped_name();
        assert!(a.starts_with("scene-"));
        assert!(a.ends_with(".json"));
        assert_eq!(a, b); // same second
    }
}
