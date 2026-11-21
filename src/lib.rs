// SPDX-License-Identifier: MIT

/// Deterministic SHA-256 build ID of the crate sources embedded at compile time by `build.rs`.
pub const BUILD_ID: &str = env!("AGY_GYRO_BUILD_ID");

/// Returns a concise prefix of a build ID (up to 8 characters) for display in logs and CLI output.
pub fn short_build_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

pub mod config;
pub mod models;
pub mod pricing;
pub mod proxy;
pub mod retry;
pub mod runner;
pub mod session;
pub mod tui;
pub mod usage;
