//! Android stub for the REST API.
//!
//! The REST API is a control-processor feature: a Crestron talks to
//! the annotator over HTTPS.  On Android the API is not built at all.
//!
//! The reason is the dependency, not the code.  The API serves HTTPS,
//! and the OpenSSL crate has no Android support — `openssl-sys` cannot
//! find a system OpenSSL on Android, and its `vendored` feature does
//! not build one either.  Rather than drag an OpenSSL cross-build into
//! the Android toolchain, the API is left out and the annotation
//! application — the drawing, the capture and the UI — stands on its
//! own.
//!
//! This module provides the same public surface as `api.rs`, so
//! `main.rs` does not need conditional code: the server simply never
//! starts, and the settings panel shows the API as unavailable.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, sync_channel},
};

use crate::state::{ApiCommand, StateSnapshot};

/// Set when the API changes the config, so the main loop knows to
/// write it to disk.
///
/// The API is not built on Android, so nothing ever sets it.
pub type ConfigDirtyFlag = Arc<AtomicBool>;

use std::sync::Arc;

/// Shared, read-only snapshot of the application state.
///
/// The main loop still publishes it every frame — it costs nothing and
/// keeps the two builds' main loops identical.
pub type SharedSnapshot = Arc<std::sync::RwLock<StateSnapshot>>;

/// Receiving end of the mutation channel, owned by the main loop.
///
/// The channel is never fed, so `try_iter` always yields nothing.
pub type ApiCommandRx = Receiver<ApiCommand>;

/// A running API server.
///
/// There is never one on Android: [`serve`] returns a server that is
/// already stopped.
pub struct ApiServer {
    /// The command channel the main loop drains.  Holding the sender
    /// keeps the channel open for the server's lifetime.
    #[allow(dead_code)]
    cmd_tx: SyncSender<ApiCommand>,
    /// Set to false to ask the accept loop to stop.
    running: Arc<AtomicBool>,
    /// The address the listener is bound to.
    #[allow(dead_code)]
    addr: String,
}

impl ApiServer {
    /// Ask the server to stop and wait for it.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Whether the server is currently accepting connections.
    ///
    /// Always false — the API is not built on Android.
    pub fn is_running(&self) -> bool {
        false
    }

    /// The address the server is listening on.
    #[allow(dead_code)]
    pub fn addr(&self) -> &str {
        &self.addr
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the API server.
///
/// On Android this does nothing but hand back a stopped server and an
/// empty command channel.  The signature matches `api::serve` so the
/// caller does not have to care which build it is in.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    bind: &str,
    port: u16,
    _tls: &crate::tls::SslAcceptor,
    _snapshot: SharedSnapshot,
    _cfg: std::sync::Arc<std::sync::RwLock<crate::config::Config>>,
    _config_dirty: ConfigDirtyFlag,
) -> (ApiServer, ApiCommandRx) {
    eprintln!("[api] the REST API is not available on Android");
    let (tx, rx) = sync_channel(0);
    (
        ApiServer {
            cmd_tx: tx,
            running: Arc::new(AtomicBool::new(false)),
            addr: format!("{bind}:{port}"),
        },
        rx,
    )
}
