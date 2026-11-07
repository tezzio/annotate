use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Web portal users (admin + up to 4 additional)
    #[serde(default)]
    pub users: Vec<User>,

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

    /// Default stroke / tool colour as RRGGBB hex (no leading #)
    #[serde(default = "default_tool_color")]
    pub tool_color: String,

    /// Default brush / stroke size in pixels
    #[serde(default = "default_tool_size")]
    pub tool_size: u32,

    /// Maximum depth of the undo / redo history stacks
    #[serde(default = "default_undo_stack_limit")]
    pub undo_stack_limit: usize,

    /// REST API settings (`[api]` section) — Crestron / remote control
    #[serde(default)]
    pub api: ApiConfig,

    /// RTSP stream settings for Visionary NextGen AV over IP
    #[serde(default)]
    pub rtsp: RtspConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String,
    pub is_admin: bool,
}

/// REST API configuration (see docs/crestron-rest-api-plan.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Serve the API at all (kiosk deployments can turn it off)
    #[serde(default = "default_api_enabled")]
    pub enabled: bool,

    /// Address to bind, e.g. "0.0.0.0" (LAN) or "127.0.0.1" (local only)
    #[serde(default = "default_api_bind")]
    pub bind: String,

    /// TCP port for the plain-HTTP API
    #[serde(default = "default_api_port")]
    pub port: u16,

    /// Optional shared token (checked via `X-Auth-Token` header or `token=`
    /// query param).  `None` = no authentication.
    #[serde(default)]
    pub token: Option<String>,
}

/// RTSP stream configuration for Visionary NextGen AV over IP sources.
/// Credentials belong in the URL itself (`rtsp://user:pass@host/path`) —
/// that's the standard RTSP convention and is what `ffmpeg -i` accepts
/// directly, so there's no need to store them as separate fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspConfig {
    /// Enable RTSP as a video source
    #[serde(default)]
    pub enabled: bool,

    /// RTSP stream URL (e.g. "rtsp://user:pass@192.168.1.100:554/stream1")
    #[serde(default)]
    pub url: Option<String>,

    /// RTSP stream width (0 = auto detect)
    #[serde(default)]
    pub width: u32,

    /// RTSP stream height (0 = auto detect)
    #[serde(default)]
    pub height: u32,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            width: 0,
            height: 0,
        }
    }
}

// ── serde defaults ────────────────────────────────────────────────────────────

fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_fullscreen() -> bool {
    true
}
fn default_tool_color() -> String {
    "ffffff".to_string()
}
fn default_tool_size() -> u32 {
    4
}
fn default_undo_stack_limit() -> usize {
    20
}
fn default_api_enabled() -> bool {
    true
}
fn default_api_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_api_port() -> u16 {
    8080
}

// ── Default impl ──────────────────────────────────────────────────────────────

impl Default for Config {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            fullscreen: default_fullscreen(),
            windowed: false,
            tool_color: default_tool_color(),
            tool_size: default_tool_size(),
            undo_stack_limit: default_undo_stack_limit(),
            api: ApiConfig::default(),
            rtsp: RtspConfig::default(),
            users: vec![User {
                username: "admin".to_string(),
                password_hash: hash_password("admin"),
                is_admin: true,
            }],
        }
    }
}

impl User {
    pub fn verify_password(&self, password: &str) -> bool {
        verify_password(password, &self.password_hash)
    }
}

pub fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(b"annotator-salt"); // simple static salt
    hex::encode(hasher.finalize())
}

fn verify_password(password: &str, hash: &str) -> bool {
    hash_password(password) == hash
}

impl Config {
    pub fn find_user(&self, username: &str) -> Option<&User> {
        self.users.iter().find(|u| u.username == username)
    }

    pub fn verify_credentials(&self, username: &str, password: &str) -> Option<&User> {
        self.find_user(username)
            .filter(|u| u.verify_password(password))
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: default_api_enabled(),
            bind: default_api_bind(),
            port: default_api_port(),
            token: None,
        }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Path to the config file in /tmp
const CONFIG_PATH: &str = "/tmp/config.ann";

/// Load config from `/tmp/config.ann`.  If the file is absent
/// or unreadable, write a default file and return default values.
pub fn load() -> Config {
    let path = PathBuf::from(CONFIG_PATH);

    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(cfg) => return cfg,
                Err(e) => eprintln!("Warning: could not parse config ({e}); using defaults"),
            },
            Err(e) => eprintln!("Warning: could not read config ({e}); using defaults"),
        }
    }

    // Write default config on first run
    let default_cfg = Config::default();
    if let Err(e) = save(&default_cfg) {
        eprintln!("Warning: could not write default config: {e}");
    }
    default_cfg
}

/// Save the current config to /tmp/config.ann
pub fn save(cfg: &Config) -> Result<(), String> {
    let path = PathBuf::from(CONFIG_PATH);
    let s = toml::to_string_pretty(cfg).map_err(|e| format!("serialize config: {e}"))?;
    fs::write(&path, s).map_err(|e| format!("write config: {e}"))?;
    eprintln!("Info: saved config to {}", path.display());
    Ok(())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Parse a `rrggbb` hex string into `(r, g, b)` bytes, defaulting to white on error.
pub fn parse_color(hex: &str) -> (u8, u8, u8) {
    let s = hex.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(n) = u32::from_str_radix(s, 16) {
            return (
                ((n >> 16) & 0xff) as u8,
                ((n >> 8) & 0xff) as u8,
                (n & 0xff) as u8,
            );
        }
    }
    (255, 255, 255)
}
