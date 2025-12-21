//! Exec action definitions for remote Rust compilation
//!
//! This module defines the RustDepInfo action that runs rustc remotely
//! via execd to produce both compilation outputs and dep-info.

use facet::Facet;

/// Action for remote Rust compilation with dep-info
///
/// This action runs rustc with `--emit=dep-info,...` to produce both
/// the compilation artifacts and the dependency information in a single
/// remote execution.
///
/// The dep-info output is used to compute `observed_inputs` which become
/// the authoritative record for cache keys.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RustDepInfoAction {
    /// Toolchain manifest hash (Blake3)
    /// References the hermetic Rust toolchain in CAS
    pub toolchain_manifest: String,

    /// Snapshot manifest hash (Blake3)
    /// References the SnapshotManifest in CAS that describes
    /// which workspace files to materialize
    pub snapshot_manifest_hash: String,

    /// Workspace-relative path to the crate root
    /// (e.g., "src/lib.rs" or "src/main.rs")
    pub crate_root_rel: String,

    /// Crate name for identification
    pub crate_name: String,

    /// Arguments to pass to rustc
    /// Must include --emit=dep-info plus other output kinds
    pub rustc_args: Vec<String>,

    /// Working directory (workspace-relative)
    /// Usually the crate directory or workspace root
    pub cwd_rel: String,

    /// Explicit environment variables (allowlist only)
    /// Ambient environment is NOT inherited
    pub env: Vec<(String, String)>,
}

impl RustDepInfoAction {
    /// Create a new RustDepInfo action
    pub fn new(
        toolchain_manifest: String,
        snapshot_manifest_hash: String,
        crate_root_rel: String,
        crate_name: String,
    ) -> Self {
        Self {
            toolchain_manifest,
            snapshot_manifest_hash,
            crate_root_rel,
            crate_name,
            rustc_args: Vec::new(),
            cwd_rel: String::new(),
            env: Vec::new(),
        }
    }

    /// Add a rustc argument
    pub fn add_arg(&mut self, arg: String) {
        self.rustc_args.push(arg);
    }

    /// Set the working directory
    pub fn set_cwd(&mut self, cwd: String) {
        self.cwd_rel = cwd;
    }

    /// Add an environment variable
    pub fn add_env(&mut self, key: String, value: String) {
        self.env.push((key, value));
    }
}

/// Result from a RustDepInfo execution
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RustDepInfoResult {
    /// Blake3 hash of the dep-info file content
    pub depinfo_blob: String,

    /// Compilation outputs (rlib, rmeta, bin, etc.)
    /// Maps output kind to CAS blob hash
    pub outputs: Vec<CompilationOutput>,

    /// Exit status code
    pub exit_code: i32,

    /// Blake3 hash of stdout content (for diagnostics)
    pub stdout_blob: Option<String>,

    /// Blake3 hash of stderr content (for diagnostics)
    pub stderr_blob: Option<String>,
}

/// A single compilation output artifact
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct CompilationOutput {
    /// Output kind (e.g., "rlib", "rmeta", "bin", "dep-info")
    pub kind: String,

    /// Workspace-relative path where this was produced
    pub path: String,

    /// Blake3 hash of the artifact in CAS
    pub blob_hash: String,
}

/// Orchestration flow documentation
///
/// ## Single-Phase Remote Execution
///
/// 1. **Build Snapshot** (daemon, local):
///    - Collect all *.rs files in crate dir (maximalist)
///    - Add Cargo.toml, Cargo.lock
///    - Hash each file → upload missing blobs to CAS
///    - Create SnapshotManifest → store in CAS
///
/// 2. **Execute Rustc** (execd, remote):
///    - Materialize toolchain from toolchain_manifest
///    - Materialize workspace from snapshot_manifest
///    - Run: `rustc --emit=dep-info,rlib,rmeta <args>`
///    - Upload all outputs (including dep-info) to CAS
///    - Return RustDepInfoResult
///
/// 3. **Parse Dep-Info** (daemon, local):
///    - Fetch depinfo_blob from CAS
///    - Parse Makefile-style format
///    - Normalize paths to workspace-relative
///    - Classify into InputSet categories
///    - Store as `observed_inputs`
///
/// 4. **Compute Cache Key** (daemon, local):
///    - Key = hash(toolchain_id, rustc_args, observed_inputs)
///    - Store outputs under this key
///    - Store InputRecords for future lookups
///
/// ## Cache Lookup Strategy
///
/// **V1 (Current)**: No pre-execution lookup
/// - First build: must execute to get dep-info
/// - Subsequent builds: cache hit using observed_inputs from prior run
/// - Simple, correct, no complex bootstrap logic
///
/// **V2 (Future Optimization)**: Bootstrap key
/// - Try lookup with snapshot_hash as bootstrap key
/// - If hit found AND has cached observed_inputs → skip execution
/// - If miss OR no observed_inputs → execute as normal
/// - More complex but avoids re-compilation in some cases
///
/// ## Snapshot Incomplete Detection
///
/// If dep-info references a workspace file not in snapshot:
/// - Classify as `snapshot_incomplete` error
/// - Return structured error with missing file path
/// - Daemon can expand snapshot and retry
/// - This handles include_str!(), #[path="..."], etc.
///
/// With maximalist *.rs snapshotting, this should be rare.
pub struct OrchestrationDoc;
