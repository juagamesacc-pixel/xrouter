//! xrouter-ffi: JNI + C FFI bridge for xrouter HTTP proxy server
//!
//! Exposes xrouter's HTTP proxy functionality as a loadable Android shared library.
//! The server runs on localhost and is accessible via HTTP from the Android app.

mod config;
mod server;
mod state;
mod ffi;
mod jni_bridge;

pub use ffi::*;
