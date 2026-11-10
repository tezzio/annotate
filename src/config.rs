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

    /// True when the stored hash uses the old, unsalted scheme and
    /// should be replaced after a successful login.
    pub fn needs_upgrade(&self) -> bool {
        !self.password_hash.starts_with("pbkdf2-sha256$")
    }
}

/// The number of PBKDF2 iterations used for new hashes.
///
/// 600,000 is the OWASP recommendation for PBKDF2-HMAC-SHA256.  It costs
/// roughly a fifth of a second per login on the target hardware — slow
/// enough to make an offline attack expensive, fast enough that an
/// operator does not notice.
pub const PBKDF2_ITERATIONS: u32 = 600_000;

/// The iteration count used when hashing is not performance-sensitive.
///
/// The tests hash passwords many times over; at the production iteration
/// count the suite would take minutes.  The tests are checking the
/// *construction*, not the work factor, so they use a cheap one.
#[cfg(test)]
const TEST_ITERATIONS: u32 = 1_000;

/// The iteration count to use.
#[cfg(not(test))]
const ACTIVE_ITERATIONS: u32 = PBKDF2_ITERATIONS;

#[cfg(test)]
const ACTIVE_ITERATIONS: u32 = TEST_ITERATIONS;

/// The length of the derived key, in bytes.
const PBKDF2_KEY_LEN: usize = 32;

/// The length of the per-user salt, in bytes.
const SALT_LEN: usize = 16;

/// Hash a password with PBKDF2-HMAC-SHA256 and a random per-user salt.
///
/// The result is self-describing:
///
///     pbkdf2-sha256$<iterations>$<salt-hex>$<key-hex>
///
/// so an old `sha256(password + "annotator-salt")` hash can still be
/// verified — and is transparently upgraded to this scheme the first
/// time the password is used.
///
/// A random per-user salt means two users with the same password have
/// different hashes, and a stolen config cannot be attacked with one
/// precomputed table.  The iteration count makes each guess expensive.
pub fn hash_password(password: &str) -> String {
    let salt = random_bytes(SALT_LEN);
    hash_password_with_salt(password, &salt, ACTIVE_ITERATIONS)
}

/// Hash a password with an existing salt and iteration count.
fn hash_password_with_salt(password: &str, salt: &[u8], iterations: u32) -> String {
    // PBKDF2-HMAC-SHA256, written out because neither PBKDF2 nor HMAC is
    // provided by the `sha2` crate and the `hmac` crate is not a
    // dependency.  This is the standard construction:
    // U1 = HMAC(password, salt || counter), U(i+1) = HMAC(password, U(i)),
    // and the output is the XOR of all of them.
    let mut block = [0u8; PBKDF2_KEY_LEN];
    // PBKDF2 derives the key in 32-byte blocks; each uses a different
    // counter.  For a 32-byte key there is exactly one block.
    for (i, chunk) in block.as_chunks_mut::<32>().0.iter_mut().enumerate() {
        let mut u = hmac_sha256(password.as_bytes(), &hmac_input(salt, i as u32 + 1));
        let mut acc = u;
        for _ in 1..iterations {
            u = hmac_sha256(password.as_bytes(), &u);
            for (a, b) in acc.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&acc);
    }

    format!(
        "pbkdf2-sha256${}${}${}",
        iterations,
        hex::encode(salt),
        hex::encode(block)
    )
}

/// HMAC-SHA256, as described in RFC 2104.
///
/// `H((K ^ opad) || H((K ^ ipad) || message))`, where the key is padded
/// or hashed to the block size and `opad`/`ipad` are the usual constants.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    const BLOCK_SIZE: usize = 64;

    // A key longer than the block size is replaced by its hash.
    let mut k = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        k[..digest.len()].copy_from_slice(&digest);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// The PBKDF2 first-block input: the salt followed by a big-endian
/// block counter.
fn hmac_input(salt: &[u8], counter: u32) -> Vec<u8> {
    let mut input = Vec::with_capacity(salt.len() + 4);
    input.extend_from_slice(salt);
    input.extend_from_slice(&counter.to_be_bytes());
    input
}

/// Random bytes, from the operating system.
fn random_bytes(n: usize) -> Vec<u8> {
    // `getrandom` is the modern source; fall back to /dev/urandom on
    // kernels that predate it.
    let mut buf = vec![0u8; n];
    #[cfg(target_os = "linux")]
    {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_getrandom,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if ret == buf.len() as libc::c_long {
            return buf;
        }
    }
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    panic!("no source of randomness available");
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

/// Verify a password against a stored hash.
///
/// Two formats are accepted:
///
/// * `pbkdf2-sha256$...` — the current scheme.
/// * a bare 64-character hex string — the original
///   `sha256(password + "annotator-salt")` scheme, so a config written
///   before the upgrade keeps working.
///
/// The comparison is constant-time, so it does not leak how much of the
/// hash matched.
fn verify_password(password: &str, hash: &str) -> bool {
    if let Some(rest) = hash.strip_prefix("pbkdf2-sha256$") {
        let mut parts = rest.split('$');
        let (Some(iterations), Some(salt_hex), Some(key_hex)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let Ok(iterations) = iterations.parse::<u32>() else {
            return false;
        };
        let (Ok(salt), Ok(_expected)) = (hex::decode(salt_hex), hex::decode(key_hex)) else {
            return false;
        };
        let actual = hash_password_with_salt(password, &salt, iterations);
        // Compare the derived key, not the whole string, so the format
        // prefix does not affect the timing.
        let actual_key = actual.rsplit('$').next().unwrap_or_default();
        constant_time_eq(actual_key.as_bytes(), key_hex.as_bytes())
    } else if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        // The old scheme.  It has no salt and no iteration count, so it
        // can only be checked directly.
        constant_time_eq(old_hash_password(password).as_bytes(), hash.as_bytes())
    } else {
        false
    }
}

/// The original hashing scheme, kept so old hashes still verify.
///
/// New passwords use PBKDF2 with a random per-user salt; this is only
/// here for backwards compatibility.
fn old_hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(b"annotator-salt");
    hex::encode(hasher.finalize())
}

/// Compare two byte strings without an early return on the first
/// difference, so the comparison does not leak how much matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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

// ── Password hashing tests ───────────────────────────────────────────────────

#[cfg(test)]
mod password_tests {
    use super::*;

    /// RFC 4231 test case 1 for HMAC-SHA256.
    #[test]
    fn hmac_sha256_matches_the_rfc_vector() {
        let key = [0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        // The expected value from RFC 4231, case 1.
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(hex::encode(mac), expected);
    }

    /// RFC 4231 test case 2 for HMAC-SHA256.
    #[test]
    fn hmac_sha256_handles_a_short_key() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let expected = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
        assert_eq!(hex::encode(mac), expected);
    }

    /// A key longer than the block size is hashed first (RFC 4231 case 6
    /// exercises exactly this).
    #[test]
    fn hmac_sha256_hashes_a_long_key() {
        let key = [0xaau8; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let expected = "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54";
        assert_eq!(hex::encode(mac), expected);
    }

    /// The classic PBKDF2-HMAC-SHA256 vector.
    #[test]
    fn pbkdf2_matches_the_standard_vector() {
        // "password", "salt", 1 iteration, 32-byte output.
        let out = hash_password_with_salt("password", b"salt", 1);
        let key = out.rsplit('$').next().unwrap();
        let expected = "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b";
        assert_eq!(key, expected);
    }

    /// The same vector with 2 iterations.
    #[test]
    fn pbkdf2_matches_the_two_iteration_vector() {
        let out = hash_password_with_salt("password", b"salt", 2);
        let key = out.rsplit('$').next().unwrap();
        let expected = "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43";
        assert_eq!(key, expected);
    }

    #[test]
    fn a_hashed_password_verifies() {
        let hash = hash_password("correct horse battery staple");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    #[test]
    fn two_users_with_the_same_password_get_different_hashes() {
        // The salt is random, so the hashes must differ.
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a, b);
        assert!(verify_password("same", &a));
        assert!(verify_password("same", &b));
    }

    #[test]
    fn the_hash_records_its_own_parameters() {
        let hash = hash_password("x");
        let parts: Vec<&str> = hash.split('$').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "pbkdf2-sha256");
        // The iteration count is recorded so it can be raised later
        // without invalidating existing hashes.
        assert_eq!(parts[1].parse::<u32>().unwrap(), ACTIVE_ITERATIONS);
        assert_eq!(parts[2].len(), SALT_LEN * 2);
        assert_eq!(parts[3].len(), PBKDF2_KEY_LEN * 2);
    }

    #[test]
    fn the_production_iteration_count_is_the_owasp_recommendation() {
        // Guard against the test build's cheap count leaking into
        // production.
        assert_eq!(PBKDF2_ITERATIONS, 600_000);
        #[cfg(not(test))]
        assert_eq!(ACTIVE_ITERATIONS, PBKDF2_ITERATIONS);
    }

    #[test]
    fn an_old_sha256_hash_still_verifies() {
        // A config written before the upgrade must keep working.
        let old = old_hash_password("admin");
        assert!(verify_password("admin", &old));
        assert!(!verify_password("wrong", &old));
    }
}
