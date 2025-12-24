//! vx-aether protocol definitions
//!
//! The aether is the brain of vx. It:
//! - Owns the picante incremental computation runtime
//! - Parses manifests and tracks source files
//! - Computes cache keys
//! - Orchestrates builds via CAS and Exec services
//!
//! The CLI is a thin client that just asks the ather to do things.

use camino::Utf8PathBuf;
use facet::Facet;

/// Aether protocol version.
/// Bump this when making breaking changes to the Aether RPC interface.
pub const AETHER_PROTOCOL_VERSION: u32 = 1;

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

/// Action type being tracked
#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
#[repr(C)]
pub enum ActionType {
    /// Compiling a Rust crate
    CompileRust(String),
    /// Acquiring toolchain
    AcquireToolchain,
    /// Acquiring registry crate
    AcquireRegistryCrate(String, String), // name, version
    /// Ingesting source tree to CAS
    IngestSource(String),
}

impl ActionType {
    /// Get a display name for this action
    pub fn display_name(&self) -> String {
        match self {
            ActionType::CompileRust(name) => format!("compile {}", name),
            ActionType::AcquireToolchain => "acquire toolchain".to_string(),
            ActionType::AcquireRegistryCrate(name, version) => {
                format!("acquire {}@{}", name, version)
            }
            ActionType::IngestSource(name) => format!("ingest {}", name),
        }
    }
}

/// The daemon service trait
#[rapace::service]
pub trait Aether {
    /// Get service version information (for health checks and compatibility)
    async fn version(&self) -> vx_oort_proto::ServiceVersion;

    /// Build a project
    async fn build(&self, request: BuildRequest) -> BuildResult;

    /// Shutdown the daemon gracefully
    async fn shutdown(&self);
}

/// Progress reporting service (implemented by vx CLI, called by aether)
#[rapace::service]
pub trait ProgressListener {
    /// Set the total number of actions
    async fn set_total(&self, total: u64);

    /// Start tracking an action, returning its ID
    async fn start_action(&self, action_type: ActionType) -> u64;

    /// Mark an action as completed
    async fn complete_action(&self, action_id: u64);

    /// Add a log message
    async fn log_message(&self, message: String);
}
