//! Android system log.
//!
//! On Android an application's standard error does not go anywhere the
//! user can see — it is closed, or goes to a buffer that `logcat` only
//! shows to the platform.  A crash on a tablet would therefore leave no
//! diagnostics at all.
//!
//! The `liblog` library that the NDK ships provides `__android_log_print`,
//! which writes to the system log, where `logcat` shows it.  This module
//! wraps it, and the panic hook in `main.rs` uses it to report what went
//! wrong.
//!
//! The level constants are the ones `android_LogPriority` defines:
//! `ANDROID_LOG_VERBOSE = 2`, `DEBUG = 3`, `INFO = 4`, `WARN = 5`,
//! `ERROR = 6`, `FATAL = 7`.

use std::ffi::CString;
use std::os::raw::c_char;

/// The system log's "error" priority.
const PRIORITY_ERROR: i32 = 6;

/// The system log's "info" priority.
const PRIORITY_INFO: i32 = 4;

unsafe extern "C" {
    fn __android_log_print(prio: i32, tag: *const c_char, text: *const c_char) -> i32;
}

/// Write a message to the Android system log.
///
/// The tag and the text are passed to `liblog` as C strings.  An
/// interior NUL byte would truncate the string, so it is replaced rather
/// than rejected — a truncated log line is better than none.
pub fn log(prio: i32, tag: &str, message: &str) {
    let tag = CString::new(tag).unwrap_or_default();
    let text = CString::new(message).unwrap_or_default();
    unsafe {
        __android_log_print(prio, tag.as_ptr(), text.as_ptr());
    }
}

/// Write an error message to the Android system log.
pub fn error(tag: &str, message: &str) {
    log(PRIORITY_ERROR, tag, message);
}

/// Write an informational message to the Android system log.
pub fn info(tag: &str, message: &str) {
    log(PRIORITY_INFO, tag, message);
}

/// The system log's "debug" priority.
const PRIORITY_DEBUG: i32 = 3;

/// Write a debug message to the Android system log.
pub fn debug(tag: &str, message: &str) {
    log(PRIORITY_DEBUG, tag, message);
}
