//! vx-daemon protocol definitions
//!
//! The daemon is the brain of vx. It:
//! - Owns the picante incremental computation runtime
//! - Parses manifests and tracks source files
//! - Computes cache keys
//! - Orchestrates builds via CAS and Exec services
//!
//! The CLI is a thin client that just asks the daemon to do things.

use camino::Utf8PathBuf;
use facet::Facet;

/// Request to build a project
#[derive(Debug, Clone, Facet)]
pub struct BuildRequest {
    /// Path to the project directory (containing Cargo.toml)
    pub project_path: Utf8PathBuf,
    /// Build in release mode
    pub release: bool,
}

/// Result of a build
#[derive(Debug, Clone, Facet)]
pub struct BuildResult {
    /// Whether the build succeeded
    pub success: bool,
    /// Human-readable status message
    pub message: String,
    /// Whether this was a cache hit
    pub cached: bool,
    /// Build duration in milliseconds (0 if cached)
    pub duration_ms: u64,
    /// Path to the output binary (if successful)
    pub output_path: Option<Utf8PathBuf>,
    /// Error message (if failed)
    pub error: Option<String>,
}

/// The daemon service trait
#[rapace::service]
pub trait Daemon {
    /// Build a project
    async fn build(&self, request: BuildRequest) -> BuildResult;
}
