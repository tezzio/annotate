use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Canvas / output width in pixels
    #[serde(default = "default_width")]
    pub width: u32,

    /// Canvas / output height in pixels
    #[serde(default = "default_height")]
    pub height: u32,

    /// Use FULLSCREEN_DESKTOP mode (production / court machine)
    #[serde(default = "default_fullscreen")]
    pub fullscreen: bool,

    /// Open as a normal resizable window (dev / debug on large monitors).
    /// Takes precedence over `fullscreen` when true.
    #[serde(default)]
    pub windowed: bool,

    /// V4L2 device path to open automatically.  None = show picker on launch.
    #[serde(default)]
    pub device_path: Option<String>,

    /// Override the embedded DejaVu Sans font with a path on disk.
    #[serde(default)]
    pub font_path: Option<String>,

    /// Default stroke / tool colour as RRGGBB hex (no leading #)
    #[serde(default = "default_tool_color")]
    pub tool_color: String,

    /// Default brush / stroke size in pixels
    #[serde(default = "default_tool_size")]
    pub tool_size: u32,

    /// Maximum depth of the undo / redo history stacks
    #[serde(default = "default_undo_stack_limit")]
    pub undo_stack_limit: usize,
}

// ── serde defaults ────────────────────────────────────────────────────────────

fn default_width() -> u32 { 1920 }
fn default_height() -> u32 { 1080 }
fn default_fullscreen() -> bool { true }
fn default_tool_color() -> String { "ffffff".to_string() }
fn default_tool_size() -> u32 { 4 }
fn default_undo_stack_limit() -> usize { 20 }

// ── Default impl ──────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            fullscreen: default_fullscreen(),
            windowed: false,
            device_path: None,
            font_path: None,
            tool_color: default_tool_color(),
            tool_size: default_tool_size(),
            undo_stack_limit: default_undo_stack_limit(),
        }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Returns the path to `~/.config/annotator/config.toml`, creating the
/// directory and writing defaults if it does not exist yet.
pub fn config_path() -> PathBuf {
    let base = dirs_home().join(".config").join("annotator");
    fs::create_dir_all(&base).ok();
    base.join("config.toml")
}

/// Load config from `~/.config/annotator/config.toml`.  If the file is absent
/// or unreadable, write a default file and return default values.
pub fn load() -> Config {
    let path = config_path();

    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(cfg) => return cfg,
                Err(e) => eprintln!("Warning: could not parse config ({e}); using defaults"),
            },
            Err(e) => eprintln!("Warning: could not read config ({e}); using defaults"),
        }
    } else {
        // Write the bundled default config.toml next to the binary, or a
        // minimal one if that file is missing.
        let default_text = include_str!("../config.toml");
        fs::write(&path, default_text).ok();
        eprintln!("Info: wrote default config to {}", path.display());
    }

    Config::default()
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Parse a `rrggbb` hex string into `(r, g, b)` bytes, defaulting to white on error.
pub fn parse_color(hex: &str) -> (u8, u8, u8) {
    let s = hex.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(n) = u32::from_str_radix(s, 16) {
            return (
                ((n >> 16) & 0xff) as u8,
                ((n >> 8)  & 0xff) as u8,
                ( n        & 0xff) as u8,
            );
        }
    }
    (255, 255, 255)
}
