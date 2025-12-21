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

/// Request to materialize a registry crate into workspace-local staging
#[derive(Debug, Clone, Facet)]
pub struct RegistryMaterializeRequest {
    /// RegistryCrateManifest hash stored in CAS
    pub manifest_hash: ManifestHash,

    /// Absolute workspace root, used to place .vx/registry/<name>/<version>
    pub workspace_root: String,
}

/// Result of materializing a registry crate
#[derive(Debug, Clone, Facet)]
pub struct RegistryMaterializeResult {
    /// Workspace-relative path to staged crate, e.g. ".vx/registry/serde/1.0.197"
    pub workspace_rel_path: String,

    /// Whether the crate was already cached (no extraction needed)
    pub was_cached: bool,

    /// Status of the materialization
    pub status: MaterializeStatus,

    /// Error message if status is Failed
    pub error: Option<String>,
}

/// Status of registry crate materialization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum MaterializeStatus {
    /// Successfully materialized (extracted or copied from cache)
    Ok = 0,
    /// Failed to materialize
    Failed = 1,
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

    /// Materialize a registry crate into workspace-local staging directory.
    ///
    /// This fetches the RegistryCrateManifest from CAS, extracts the tarball
    /// to global cache if needed, then clones into `.vx/registry/<name>/<version>`.
    async fn materialize_registry_crate(
        &self,
        request: RegistryMaterializeRequest,
    ) -> RegistryMaterializeResult;
}
