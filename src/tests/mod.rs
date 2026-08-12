//! Self-contained test suite for the PERMAFROST vault.
//!
//! Runs on the wasm32 target only: the tests deploy the vendored
//! `wasm/permafrost.wasm` into an in-memory metashrew VM and drive it
//! through real blocks. Build the release WASM and copy it into
//! `src/tests/wasm/` before the first run — the exact commands live in
//! README.md ("Build & test").

#![allow(dead_code)]

pub mod helpers;
pub mod vault_test;

macro_rules! test_log {
    ($($arg:tt)*) => {{
        #[cfg(target_arch = "wasm32")]
        {
            web_sys::console::log_1(&format!($($arg)*).into());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            println!($($arg)*);
        }
    }};
}
pub(crate) use test_log;
