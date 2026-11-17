//! Android stub for the TLS acceptor.
//!
//! The REST API serves HTTPS, and the OpenSSL crate has no Android
//! support — so neither the API nor its TLS is built on Android.  This
//! module stands in for `tls.rs` so the main loop does not need
//! conditional code: the acceptor is simply never available, which is
//! the same state the API is in when its certificate cannot be read.
//!
//! See `api_android.rs` for why the API is left out.

use std::path::Path;

/// The error type the real module uses.
pub type Error = String;

/// The TLS acceptor the API server would be given.
///
/// There is none on Android, so the type is opaque and uninhabited in
/// practice.
pub struct SslAcceptor;

impl SslAcceptor {
    /// There is no acceptor to construct.
    pub fn unavailable() -> Self {
        SslAcceptor
    }
}

/// Load (or generate) the API's TLS certificate.
///
/// Always fails on Android: the API is not built there, so there is
/// nothing to load the certificate for.
pub fn acceptor_from_config_dir() -> Result<SslAcceptor, Error> {
    Err("the REST API is not available on Android".into())
}

/// Load (or generate) a TLS certificate from explicit paths.
///
/// Always fails on Android, for the same reason.
#[allow(dead_code)]
pub fn acceptor_from_paths(_cert: &Path, _key: &Path) -> Result<SslAcceptor, Error> {
    Err("the REST API is not available on Android".into())
}
