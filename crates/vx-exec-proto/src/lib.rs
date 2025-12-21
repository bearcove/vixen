//! Exec service protocol definitions
//!
//! Defines the rapace service trait and types for rustc and zig cc execution.

use facet::Facet;
use vx_cas_proto::{BlobHash, ManifestHash};

/// Structured rustc invocation (not a shell string!)
#[derive(Debug, Clone, Facet)]
pub struct RustcInvocation {
    /// Toolchain manifest hash (execd fetches manifest, materializes, runs)
    pub toolchain_manifest: ManifestHash,
    /// Command line arguments
    pub args: Vec<String>,
    /// Environment variables (explicit, minimal)
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: String,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
}

/// Structured zig cc invocation for C/C++ compilation
#[derive(Debug, Clone, Facet)]
pub struct CcInvocation {
    /// Toolchain manifest hash (execd fetches manifest, materializes, runs)
    pub toolchain_manifest: ManifestHash,
    /// Command line arguments (["cc", "-c", "main.c", "-o", "main.o", ...])
    pub args: Vec<String>,
    /// Environment variables (minimal, controlled)
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: String,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Path to depfile (if any) - will be parsed and returned in result
    pub depfile_path: Option<String>,
    /// Workspace root for canonicalizing depfile paths
    pub workspace_root: String,
}

/// An expected output file
#[derive(Debug, Clone, Facet)]
pub struct ExpectedOutput {
    /// Logical name ("bin", "rlib", "obj", "depfile", etc.)
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

/// Result of executing zig cc
#[derive(Debug, Clone, Facet)]
pub struct CcExecuteResult {
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
    /// Discovered dependencies (workspace-relative paths from depfile)
    /// These are canonicalized, sorted, and deduplicated.
    /// System headers (outside workspace) are filtered out.
    pub discovered_deps: Vec<String>,
    /// If exec pushed manifest to CAS, this is set
    pub manifest_hash: Option<ManifestHash>,
}

/// Exec service trait
#[rapace::service]
pub trait Exec {
    /// Execute a rustc invocation
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult;

    /// Execute a zig cc invocation (C/C++ compilation)
    ///
    /// This runs zig cc, captures outputs, and parses the depfile (if any)
    /// to return discovered dependencies for incremental builds.
    async fn execute_cc(&self, invocation: CcInvocation) -> CcExecuteResult;
}
