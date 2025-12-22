//! Toolchain management for vx
//!
//! Downloads and manages toolchains for hermetic builds:
//! - Rust toolchains from static.rust-lang.org (without rustup)
//! - Zig toolchains for C/C++ compilation (via `zig cc`)
//!
//! Toolchains are stored in CAS for deduplication and remote execution.

pub mod rust;
pub mod zig;

use thiserror::Error;

// Re-export protocol types from vx-cas-proto
pub use vx_cas_proto::{Blake3Hash, RustChannel, RustComponent, RustToolchainSpec};

// Re-export Rust toolchain functions
pub use rust::{
    AcquiredRustToolchain, ChannelManifest, RustToolchainId, acquire_rust_toolchain,
    download_component, fetch_channel_manifest, parse_rust_channel, rust_channel_manifest_url,
    rust_toolchain_spec_cross, rust_toolchain_spec_native,
};

// Type alias for backward compatibility
pub type Channel = RustChannel;

/// Errors that can occur during toolchain operations
#[derive(Debug, Error)]
pub enum ToolchainError {
    #[error("failed to fetch manifest from {url}: {source}")]
    FetchError {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to parse channel manifest: {0}")]
    ParseError(String),

    #[error("component {component} not available for target {target}")]
    ComponentNotAvailable { component: String, target: String },

    #[error("invalid channel specification: {0}")]
    InvalidChannel(String),

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

pub fn detect_host_triple() -> Result<String, ToolchainError> {
    use std::process::Command;

    let output =
        Command::new("rustc")
            .arg("-vV")
            .output()
            .map_err(|e| ToolchainError::FetchError {
                url: "rustc -vV".to_string(),
                source: Box::new(e),
            })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }

    Err(ToolchainError::FetchError {
        url: "rustc -vV".to_string(),
        source: "could not determine host triple".into(),
    })
}

#[cfg(test)]
mod tests;
