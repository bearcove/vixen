//! Exec service protocol definitions
//!
//! Defines the rapace service trait and types for rustc execution.

use facet::Facet;
use vx_cas_proto::{BlobHash, ManifestHash};

/// Structured rustc invocation (not a shell string!)
#[derive(Debug, Clone, Facet)]
pub struct RustcInvocation {
    /// Program to run ("rustc")
    pub program: String,
    /// Command line arguments
    pub args: Vec<String>,
    /// Environment variables (explicit, minimal)
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: String,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
}

/// An expected output file
#[derive(Debug, Clone, Facet)]
pub struct ExpectedOutput {
    /// Logical name ("bin", "rlib", etc.)
    pub logical: String,
    /// Relative path where output will be written
    pub path: String,
    /// Whether the file should be executable
    pub executable: bool,
}

/// A produced output file
#[derive(Debug, Clone, Facet)]
pub struct ProducedOutput {
    /// Logical name
    pub logical: String,
    /// Relative path
    pub path: String,
    /// Hash of the blob (exec pushed to CAS)
    pub blob_hash: BlobHash,
    /// Whether the file is executable
    pub executable: bool,
}

/// Result of executing rustc
#[derive(Debug, Clone, Facet)]
pub struct ExecuteResult {
    /// Exit code
    pub exit_code: i32,
    /// Captured stdout
    pub stdout: String,
    /// Captured stderr
    pub stderr: String,
    /// Duration in milliseconds
    pub duration_ms: u64,
    /// Produced outputs (with blob hashes)
    pub outputs: Vec<ProducedOutput>,
    /// If exec pushed manifest to CAS, this is set
    pub manifest_hash: Option<ManifestHash>,
}

/// Exec service trait
#[rapace::service]
pub trait Exec {
    /// Execute a rustc invocation
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult;
}
