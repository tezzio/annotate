//! Exporting annotations to image files.
//!
//! The on-screen screenshot (`capture::save_screenshot`) writes a PNG of
//! whatever is currently on the canvas.  That is fine for a quick look
//! but it is not a usable case artifact:
//!
//! * it goes to a hardcoded path in `/tmp`
//! * it cannot be triggered remotely — only from the on-screen button
//! * it carries no metadata, so the file cannot be traced back to a case
//! * it is a snapshot of the *display*, which includes the menu bar and
//!   the tool rail
//!
//! This module produces a clean export instead: the annotation layer
//! rendered over the video frame, with case metadata written into the
//! PNG's text chunks so the file is self-describing.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::canvas::Canvas;
use crate::scene::Scene;

/// Metadata written into the exported PNG and reported by the API.
///
/// This is what makes an exported image usable as a case artifact — the
/// file says what it is without needing a side database.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExportMeta {
    /// Case reference or exhibit number.
    pub case_reference: String,
    /// Who made the annotations.
    pub annotator: String,
    /// Free-text note.
    pub note: String,
    /// The video input that was in use, if any.
    pub input: String,
    /// The tool that was active.
    pub tool: String,
    /// The active colour, as `rrggbb`.
    pub color: String,
    /// Brush size in px.
    pub brush_size: u32,
}

/// The directory exports are written to.
///
/// `$ANNOTATOR_EXPORT_DIR` overrides it; otherwise
/// `~/.local/share/annotator/exports`.
pub fn export_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ANNOTATOR_EXPORT_DIR")
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
    base.join("annotator/exports")
}

/// Ensure the export directory exists, returning it.
pub fn ensure_export_dir() -> std::io::Result<PathBuf> {
    let dir = export_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A timestamped filename for a new export.
pub fn timestamped_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("export-{now}.png")
}

/// Render the annotation scene onto a canvas and write it as a PNG.
///
/// The scene is rendered fresh rather than read back from the screen, so
/// the export contains only the annotations — no menu bar, no tool rail,
/// no cursor.  The caller supplies the canvas to composite the video
/// frame onto.
pub fn write_png(canvas: &Canvas, meta: &ExportMeta, path: &Path) -> Result<u64, String> {
    let rgb = to_rgb(&canvas.pixels);
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, canvas.width, canvas.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;

        // Metadata as PNG text chunks, so the file describes itself.
        for (keyword, value) in meta_chunks(meta) {
            let chunk = png::text_metadata::TEXtChunk {
                keyword: keyword.to_string(),
                text: value,
            };
            // A metadata failure must not lose the image itself.
            let _ = writer.write_text_chunk(&chunk);
        }

        writer
            .write_image_data(&rgb)
            .map_err(|e| format!("png data: {e}"))?;
    }

    fs::write(path, &out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(out.len() as u64)
}

/// The metadata as (keyword, value) pairs for the PNG text chunks.
fn meta_chunks(meta: &ExportMeta) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if !meta.case_reference.is_empty() {
        out.push(("CaseReference", meta.case_reference.clone()));
    }
    if !meta.annotator.is_empty() {
        out.push(("Annotator", meta.annotator.clone()));
    }
    if !meta.note.is_empty() {
        out.push(("Note", meta.note.clone()));
    }
    if !meta.input.is_empty() {
        out.push(("VideoInput", meta.input.clone()));
    }
    if !meta.tool.is_empty() {
        out.push(("Tool", meta.tool.clone()));
    }
    if !meta.color.is_empty() {
        out.push(("Color", meta.color.clone()));
    }
    out.push(("BrushSize", meta.brush_size.to_string()));
    out.push((
        "Created",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string(),
    ));
    out
}

/// Convert RGBA canvas pixels to RGB, compositing over black.
///
/// The annotation layer is transparent where nothing has been drawn, so
/// the video frame has to be underneath it.  Callers that have a video
/// frame should composite it in first; this function is the fallback for
/// an export with no video.
fn to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.as_chunks::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0]);
        } else {
            // Source-over onto black.
            out.push((px[0] as u32 * a / 255) as u8);
            out.push((px[1] as u32 * a / 255) as u8);
            out.push((px[2] as u32 * a / 255) as u8);
        }
    }
    out
}

/// Composite an RGBA annotation layer over an RGB video frame.
///
/// Returns the composited RGB pixels.
pub fn composite_over_video(rgba: &[u8], video_rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(video_rgb.len());
    for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
        let a = px[3] as u32;
        let v = i * 3;
        let (vr, vg, vb) = (
            *video_rgb.get(v).unwrap_or(&0) as u32,
            *video_rgb.get(v + 1).unwrap_or(&0) as u32,
            *video_rgb.get(v + 2).unwrap_or(&0) as u32,
        );
        if a == 0 {
            out.push(vr as u8);
            out.push(vg as u8);
            out.push(vb as u8);
        } else if a == 255 {
            out.push(px[0]);
            out.push(px[1]);
            out.push(px[2]);
        } else {
            // Source-over the video.
            let ia = 255 - a;
            out.push(((px[0] as u32 * a + vr * ia) / 255) as u8);
            out.push(((px[1] as u32 * a + vg * ia) / 255) as u8);
            out.push(((px[2] as u32 * a + vb * ia) / 255) as u8);
        }
    }
    out
}

/// Write an RGB image out as a PNG.
pub fn write_rgb_png(rgb: &[u8], width: u32, height: u32, path: &Path) -> Result<u64, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| format!("png data: {e}"))?;
    }
    fs::write(path, &out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(out.len() as u64)
}

/// Render the scene into a fresh canvas of the given size.
///
/// Used by the export path so the exported image contains the
/// annotations only, without the surrounding UI.
pub fn render_scene_to_canvas(scene: &Scene, width: u32, height: u32) -> Canvas {
    let mut canvas = Canvas::new(width, height);
    // `render_to` is the single-threaded variant; the export happens on
    // the main loop's thread, where blocking briefly is fine.
    let mut copy = scene.clone();
    copy.render_to(&mut canvas);
    canvas
}

/// Metadata about an exported file, for listing.
#[derive(Clone, Debug, Serialize)]
pub struct ExportInfo {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub created_at: u64,
}

/// List the exports in the export directory, newest first.
pub fn list_exports() -> Vec<ExportInfo> {
    let dir = export_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let created_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(ExportInfo {
            name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path: path.display().to_string(),
            bytes: meta.len(),
            created_at,
        });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_with_a_red_pixel() -> Canvas {
        let mut c = Canvas::new(2, 1);
        c.put_pixel(0, 0, 255, 0, 0, 255);
        c
    }

    #[test]
    fn an_export_produces_a_readable_png() {
        let dir =
            std::env::temp_dir().join(format!("annotator-export-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.png");

        let canvas = canvas_with_a_red_pixel();
        let meta = ExportMeta {
            case_reference: "CASE-2026-0042".into(),
            annotator: "T. Dixon".into(),
            ..Default::default()
        };
        let bytes = write_png(&canvas, &meta, &path).expect("export");
        assert!(bytes > 0);
        assert!(path.exists());

        // The PNG decodes back to the expected pixels.
        let data = fs::read(&path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&data));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 1);
        assert_eq!(info.color_type, png::ColorType::Rgb);
        // Pixel 0 is red, pixel 1 is black (transparent over black).
        assert_eq!(&buf[0..3], &[255, 0, 0]);
        assert_eq!(&buf[3..6], &[0, 0, 0]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_metadata_is_written_into_the_png() {
        let dir =
            std::env::temp_dir().join(format!("annotator-export-meta-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.png");

        let canvas = canvas_with_a_red_pixel();
        let meta = ExportMeta {
            case_reference: "CASE-42".into(),
            annotator: "J. Doe".into(),
            note: "Exhibit A".into(),
            input: "USB Camera".into(),
            tool: "pen".into(),
            color: "ff0000".into(),
            brush_size: 7,
        };
        write_png(&canvas, &meta, &path).expect("export");

        // The text chunks are in the raw file.
        let data = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("CaseReference"), "keyword missing");
        assert!(text.contains("CASE-42"), "value missing");
        assert!(text.contains("Annotator"));
        assert!(text.contains("J. Doe"));
        assert!(text.contains("BrushSize"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn annotations_composite_over_the_video_frame() {
        // A canvas with a half-transparent red pixel over a green video.
        let mut rgba = vec![0u8; 4];
        rgba[0] = 255;
        rgba[3] = 128;
        let video = vec![0u8, 255, 0];

        let out = composite_over_video(&rgba, &video);
        assert_eq!(out.len(), 3);
        // Half red over green: the red channel is ~128, green ~127.
        assert!(out[0] > 100 && out[0] < 160, "got {}", out[0]);
        assert!(out[1] > 100 && out[1] < 160, "got {}", out[1]);
        assert_eq!(out[2], 0);
    }

    #[test]
    fn a_transparent_annotation_leaves_the_video_untouched() {
        let rgba = vec![0u8, 0, 0, 0];
        let video = vec![10u8, 20, 30];
        let out = composite_over_video(&rgba, &video);
        assert_eq!(out, vec![10, 20, 30]);
    }

    #[test]
    fn an_opaque_annotation_replaces_the_video() {
        let mut rgba = vec![0u8; 4];
        rgba[0] = 255;
        rgba[3] = 255;
        let video = vec![10u8, 20, 30];
        let out = composite_over_video(&rgba, &video);
        assert_eq!(out, vec![255, 0, 0]);
    }

    #[test]
    fn the_export_directory_honours_the_override() {
        // The override is read at call time, so this test is safe to run
        // alongside others that touch the real directory.
        let dir = std::env::temp_dir().join("annotator-export-dir-test");
        // SAFETY: single-threaded test process for this variable.
        unsafe { std::env::set_var("ANNOTATOR_EXPORT_DIR", &dir) };
        assert_eq!(export_dir(), dir);
        unsafe { std::env::remove_var("ANNOTATOR_EXPORT_DIR") };
    }

    #[test]
    fn the_timestamped_name_is_unique_per_second() {
        let a = timestamped_name();
        assert!(a.starts_with("export-"));
        assert!(a.ends_with(".png"));
        assert_eq!(a, timestamped_name());
    }
}
