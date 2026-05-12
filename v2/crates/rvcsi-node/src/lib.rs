//! # rvCSI Node.js bindings — napi-rs (skeleton; completed during integration)
//!
//! The safe TypeScript-facing surface over the rvCSI Rust runtime
//! (ADR-095 D3/D4, ADR-096). Nothing here exposes raw pointers; every value
//! that crosses the boundary is a validated/normalized struct or a JSON string.

#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

/// rvCSI runtime version (the workspace crate version).
#[napi]
pub fn rvcsi_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// ABI version of the linked napi-c Nexmon shim (`major << 16 | minor`).
#[napi]
pub fn nexmon_shim_abi_version() -> u32 {
    rvcsi_adapter_nexmon::shim_abi_version()
}
