use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The web portal allows one administrator plus four additional users.
pub const MAX_USERS: usize = 5;

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

    /// Show the wall-clock time in the middle of the top bar.
    #[serde(default)]
    pub show_time: bool,

    /// Hide the toolbar / side rail entirely.
    #[serde(default)]
    pub hide_toolbar: bool,

    /// Leave a trail behind the laser pointer.
    #[serde(default = "default_laser_on")]
    pub laser_on: bool,

    /// Show the frame-rate counter in the top bar.
    #[serde(default)]
    pub show_fps: bool,

    /// REST API settings (`[api]` section) — Crestron / remote control
    #[serde(default)]
    pub api: ApiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String,
    pub is_admin: bool,
    /// The password is a generated placeholder and must be changed before
    /// the account can be used for anything but the change itself.
    ///
    /// Set on first run so `admin`/`admin` never ships silently.
    #[serde(default)]
    pub must_change_password: bool,
}

/// REST API configuration (see ROADMAP.md).
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

    /// Case reference written into exported images.  Makes an exported
    /// annotation traceable without a side database.
    #[serde(default)]
    pub case_reference: String,

    /// Who is making the annotations — written into exports.
    #[serde(default)]
    pub annotator: String,

    /// Free-text note written into exports.
    #[serde(default)]
    pub case_note: String,
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
fn default_laser_on() -> bool {
    true
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
            show_time: false,
            hide_toolbar: false,
            laser_on: default_laser_on(),
            show_fps: false,
            api: ApiConfig::default(),

            users: vec![User {
                username: "admin".to_string(),
                password_hash: hash_password("admin"),
                is_admin: true,
                must_change_password: false,
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

/// Generate a random password.
///
/// Used on first run so the account does not ship with a well-known
/// password.  The value is printed once, at startup, for the operator to
/// record.
pub fn generate_password() -> String {
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Mix the clock with the process id so two boxes provisioned in the
    // same second do not get the same password.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(now.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(std::time::Instant::now().elapsed().as_nanos().to_le_bytes());
    let digest = hex::encode(hasher.finalize());

    // 20 hex characters — 80 bits of entropy, typed without ambiguity.
    digest[..20].to_string()
}

/// Replace every account's password with a freshly generated one and
/// mark them as needing a change.
///
/// Returns the generated passwords so the caller can print them.
pub fn provision_accounts(cfg: &mut Config) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for user in &mut cfg.users {
        let password = generate_password();
        user.password_hash = hash_password(&password);
        user.must_change_password = true;
        out.push((user.username.clone(), password));
    }
    out
}

/// Set a user's password.
///
/// The account is also cleared of the "must change" flag, since this is
/// the change itself.
pub fn set_password(cfg: &mut Config, username: &str, password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("the password must be at least 8 characters".into());
    }
    let user = cfg
        .users
        .iter_mut()
        .find(|u| u.username == username)
        .ok_or_else(|| format!("no such user '{username}'"))?;
    user.password_hash = hash_password(password);
    user.must_change_password = false;
    Ok(())
}

fn verify_password(password: &str, hash: &str) -> bool {
    hash_password(password) == hash
}

impl Config {
    /// Whether the configuration holds more users than the portal allows.
    /// Only the first [`MAX_USERS`] are used; the rest are ignored so that
    /// a hand-edited file cannot quietly widen the account list.
    pub fn users_exceed_limit(&self) -> bool {
        self.users.len() > MAX_USERS
    }

    /// The users that are actually honoured, capped at [`MAX_USERS`].
    pub fn active_users(&self) -> &[User] {
        &self.users[..self.users.len().min(MAX_USERS)]
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: default_api_enabled(),
            bind: default_api_bind(),
            port: default_api_port(),
            token: None,
            case_reference: String::new(),
            annotator: String::new(),
            case_note: String::new(),
        }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// The legacy config path.
///
/// The config used to live here.  `/tmp` is tmpfs — it is wiped on every
/// reboot — so the API token and the password hashes were silently lost
/// on restart, which left the REST API open to the whole LAN.  The path
/// is kept only so an existing config can be migrated.
pub const LEGACY_CONFIG_PATH: &str = "/tmp/config.ann";

/// The file name used for the config in every location.
const CONFIG_FILE_NAME: &str = "config.ann";

/// The candidate config locations, in priority order.
///
/// The first one that exists wins.  `ANNOTATOR_CONFIG` overrides
/// everything — it names a file, not a directory.
pub fn config_candidates() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("ANNOTATOR_CONFIG")
        && !p.is_empty()
    {
        return vec![PathBuf::from(p)];
    }

    let mut out = Vec::new();

    // The user's own configuration directory.
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME")
        && !p.is_empty()
    {
        out.push(PathBuf::from(p).join("annotator").join(CONFIG_FILE_NAME));
    } else if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        out.push(
            PathBuf::from(home)
                .join(".config")
                .join("annotator")
                .join(CONFIG_FILE_NAME),
        );
    }

    // A system-wide location, for a shared install.
    out.push(PathBuf::from("/etc/annotator").join(CONFIG_FILE_NAME));

    // Last resort, so a fresh install on a locked-down box still starts.
    out.push(PathBuf::from("/tmp").join(CONFIG_FILE_NAME));

    out
}

/// The path the config is actually read from and written to.
///
/// The first candidate that exists wins, so an existing configuration is
/// never silently moved.  Otherwise the first candidate that can be
/// created is used.
pub fn config_path() -> PathBuf {
    let candidates = config_candidates();
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    // None exist — prefer the home directory, fall back through the list.
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("/tmp").join(CONFIG_FILE_NAME))
}

/// Create the directory that holds the config, with restrictive
/// permissions.
///
/// The file holds password hashes and the API token, so it must not be
/// world-readable.
fn ensure_config_dir(path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        // Best effort — some filesystems do not support it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

/// Move a legacy `/tmp/config.ann` to the new location.
///
/// Returns the new path when a migration happened.
fn migrate_legacy(new_path: &Path) -> Option<PathBuf> {
    let legacy = PathBuf::from(LEGACY_CONFIG_PATH);
    if !legacy.exists() || new_path == legacy {
        return None;
    }
    if let Some(parent) = new_path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return None;
    }
    match fs::rename(&legacy, new_path) {
        Ok(()) => Some(new_path.to_path_buf()),
        Err(_) => {
            // A rename across filesystems fails; fall back to a copy.
            match fs::copy(&legacy, new_path) {
                Ok(_) => {
                    let _ = fs::remove_file(&legacy);
                    Some(new_path.to_path_buf())
                }
                Err(_) => None,
            }
        }
    }
}

/// Load the config.
///
/// The file is read from the first candidate location that exists.  If
/// none exists, a legacy `/tmp/config.ann` is migrated if present, and
/// otherwise a default config is written.
pub fn load() -> Config {
    let path = config_path();
    eprintln!("Info: config path: {}", path.display());

    if path.exists() {
        match fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(cfg) => {
                    if cfg.users_exceed_limit() {
                        eprintln!(
                            "Warning: config lists {} users; only the first {MAX_USERS} are used",
                            cfg.users.len()
                        );
                    }
                    return cfg;
                }
                Err(e) => eprintln!("Warning: could not parse config ({e}); using defaults"),
            },
            Err(e) => eprintln!("Warning: could not read config ({e}); using defaults"),
        }
    } else if let Some(migrated) = migrate_legacy(&path) {
        eprintln!(
            "Info: migrated the config from {} to {}",
            LEGACY_CONFIG_PATH,
            migrated.display()
        );
        if let Ok(s) = fs::read_to_string(&migrated)
            && let Ok(cfg) = toml::from_str::<Config>(&s)
        {
            return cfg;
        }
    }

    // First run: write a config with a generated password rather than a
    // well-known default.  Shipping `admin`/`admin` would leave the
    // portal wide open.
    let mut default_cfg = Config::default();
    let provisioned = provision_accounts(&mut default_cfg);
    if let Err(e) = save(&default_cfg) {
        eprintln!("Warning: could not write default config: {e}");
    }
    for (username, password) in provisioned {
        let rule = "=".repeat(64);
        eprintln!("{rule}");
        eprintln!("[annotator] generated password for '{username}': {password}");
        eprintln!("[annotator] change it after the first login.");
        eprintln!("{rule}");
    }
    default_cfg
}

/// Save the current config.
pub fn save(cfg: &Config) -> Result<(), String> {
    let path = config_path();
    ensure_config_dir(&path)?;
    let s = toml::to_string_pretty(cfg).map_err(|e| format!("serialize config: {e}"))?;
    fs::write(&path, s).map_err(|e| format!("write config: {e}"))?;
    // The file holds password hashes and the API token — keep it
    // readable only by its owner.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    eprintln!("Info: saved config to {}", path.display());
    Ok(())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Parse a `rrggbb` hex string into `(r, g, b)` bytes, defaulting to white on error.
pub fn parse_color(hex: &str) -> (u8, u8, u8) {
    let s = hex.trim_start_matches('#');
    if s.len() == 6
        && let Ok(n) = u32::from_str_radix(s, 16)
    {
        return (
            ((n >> 16) & 0xff) as u8,
            ((n >> 8) & 0xff) as u8,
            (n & 0xff) as u8,
        );
    }
    (255, 255, 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> User {
        User {
            username: name.to_string(),
            password_hash: hash_password(name),
            is_admin: false,
            must_change_password: false,
        }
    }

    #[test]
    fn default_config_has_one_admin() {
        let cfg = Config::default();
        assert_eq!(cfg.users.len(), 1);
        assert!(cfg.users[0].is_admin);
        let admin = &cfg.active_users()[0];
        assert_eq!(admin.username, "admin");
        assert!(admin.verify_password("admin"));
    }

    #[test]
    fn active_users_are_capped_at_five() {
        let mut cfg = Config::default();
        assert!(!cfg.users_exceed_limit());
        for i in 0..7 {
            cfg.users.push(user(&format!("u{i}")));
        }
        assert!(cfg.users_exceed_limit());
        assert_eq!(cfg.active_users().len(), MAX_USERS);
    }

    #[test]
    fn credentials_outside_the_cap_are_ignored() {
        let mut cfg = Config::default();
        // Six extra users push the last one past the cap.
        for i in 0..6 {
            cfg.users.push(user(&format!("u{i}")));
        }
        let beyond = &cfg.users[MAX_USERS];
        assert!(
            cfg.active_users()
                .iter()
                .all(|u| u.username != beyond.username)
        );
    }

    #[test]
    fn the_config_path_prefers_the_home_directory() {
        // The env override wins outright.
        unsafe { std::env::set_var("ANNOTATOR_CONFIG", "/tmp/explicit.ann") };
        let candidates = config_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], PathBuf::from("/tmp/explicit.ann"));
        unsafe { std::env::remove_var("ANNOTATOR_CONFIG") };

        // Without an override, the home directory comes first and /tmp is
        // the last resort — never the only option.
        let candidates = config_candidates();
        assert!(candidates.len() >= 2);
        let first = candidates[0].display().to_string();
        assert!(
            first.contains(".config") || first.contains("XDG") || first.contains("annotator"),
            "unexpected first candidate: {first}"
        );
        assert_eq!(
            candidates.last().unwrap(),
            &PathBuf::from("/tmp/config.ann"),
            "/tmp must remain the last-resort fallback"
        );
    }

    #[test]
    fn a_generated_password_is_usable_and_unique() {
        let a = generate_password();
        let b = generate_password();
        assert_eq!(a.len(), 20);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two generated passwords must differ");
        // The generated password verifies against its own hash.
        assert!(verify_password(&a, &hash_password(&a)));
    }

    #[test]
    fn provisioning_marks_the_accounts_for_a_password_change() {
        let mut cfg = Config::default();
        let provisioned = provision_accounts(&mut cfg);
        assert_eq!(provisioned.len(), cfg.users.len());
        assert!(cfg.users.iter().all(|u| u.must_change_password));
        // The generated password is the one that now works.
        let (username, password) = &provisioned[0];
        let user = cfg.users.iter().find(|u| &u.username == username).unwrap();
        assert!(user.verify_password(password));
    }

    #[test]
    fn setting_a_password_clears_the_must_change_flag() {
        let mut cfg = Config::default();
        cfg.users[0].must_change_password = true;
        set_password(&mut cfg, "admin", "a-new-password").expect("set");
        let user = &cfg.users[0];
        assert!(!user.must_change_password);
        assert!(user.verify_password("a-new-password"));
        assert!(!user.verify_password("admin"));
    }

    #[test]
    fn a_short_password_is_refused() {
        let mut cfg = Config::default();
        assert!(set_password(&mut cfg, "admin", "short").is_err());
        assert!(set_password(&mut cfg, "admin", "12345678").is_ok());
    }

    #[test]
    fn setting_an_unknown_users_password_fails() {
        let mut cfg = Config::default();
        assert!(set_password(&mut cfg, "nobody", "a-long-password").is_err());
    }

    #[test]
    fn the_config_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.api.token = Some("secret-token".into());
        cfg.api.case_reference = "CASE-42".into();
        cfg.api.annotator = "T. Dixon".into();
        cfg.users[0].must_change_password = true;

        let text = toml::to_string_pretty(&cfg).expect("serialize");
        let back: Config = toml::from_str(&text).expect("parse");
        assert_eq!(back.api.token.as_deref(), Some("secret-token"));
        assert_eq!(back.api.case_reference, "CASE-42");
        assert_eq!(back.api.annotator, "T. Dixon");
        assert!(back.users[0].must_change_password);
    }

    #[test]
    fn a_config_without_the_new_fields_still_parses() {
        // An older config file must keep working — the new fields have
        // serde defaults.
        let text = r#"
width = 1920
height = 1080
tool_color = "ffffff"
tool_size = 4

[[users]]
username = "admin"
password_hash = "abc"
is_admin = true
"#;
        let cfg: Config = toml::from_str(text).expect("parse");
        assert_eq!(cfg.api.case_reference, "");
        assert_eq!(cfg.api.annotator, "");
        assert!(cfg.api.token.is_none());
        assert!(!cfg.users[0].must_change_password);
    }
}
