//! Self-contained test suite for the PERMAFROST vault.
//!
//! Runs on the wasm32 target only (same flow as the fire root harness):
//!   cargo build --release --target wasm32-unknown-unknown -p permafrost
//!   cp target/wasm32-unknown-unknown/release/permafrost.wasm \
//!      alkanes/permafrost/src/tests/wasm/
//!   cargo test -p permafrost --target wasm32-unknown-unknown
//!
//! `./scripts/build-wasms.sh` refreshes the vendored wasm automatically.

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
