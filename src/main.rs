//! The desktop entry point.
//!
//! Everything lives in the library (`src/lib.rs`), because the Android
//! build needs the same code as a `cdylib` that SDL loads and calls
//! `SDL_main` in.  The desktop build is an ordinary executable that
//! calls the library's `run`.

fn main() {
    annotator::run();
}
