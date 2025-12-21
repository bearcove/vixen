//! Exec action definitions for remote Rust compilation
//!
//! This module defines the RustDepInfo action that runs rustc remotely
//! via execd to produce both compilation outputs and dep-info.

use crate::input_set::InputSet;
use crate::snapshot::SnapshotManifest;
use facet::Facet;

/// Action for remote Rust compilation with dep-info
///
/// This action runs rustc with `--emit=dep-info,...` to produce both
/// the compilation artifacts and the dependency information in a single
/// remote execution.
///
/// The dep-info output is used to compute `observed_inputs` which become
/// the authoritative record for cache keys.
///
/// ## Invariants
/// - Only execd runs rustc (daemon NEVER shells out to rustc)
/// - Toolchain is hermetic and referenced by toolchain_manifest
/// - Observed inputs are authoritative for cache keys
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RustDepInfoAction {
    /// Toolchain manifest hash (Blake3)
    /// References the hermetic Rust toolchain in CAS
    pub toolchain_manifest: String,

    /// Crate identifier (Blake3 or node ID)
    /// Used for tracking which crate this build is for
    pub crate_id: String,

    /// Workspace root identifier (Blake3)
    /// Opaque identifier for workspace identity (used for path normalization)
    pub workspace_root_id: String,

    /// Snapshot manifest describing all files to materialize
    /// This is the maximalist superset of files (all *.rs + Cargo.toml/lock)
    pub snapshot_manifest: SnapshotManifest,

    /// Arguments to pass to rustc
    /// MUST include --emit=dep-info and a deterministic --dep-info output path
    pub rustc_args: Vec<String>,

    /// Working directory (workspace-relative)
    /// Usually the crate directory or workspace root
    pub cwd_rel: String,

    /// Explicit environment variables (strict allowlist only)
    /// Ambient environment is NOT inherited. Should be almost empty.
    pub env: Vec<(String, String)>,
}

impl RustDepInfoAction {
    /// Create a new RustDepInfo action
    pub fn new(
        toolchain_manifest: String,
        crate_id: String,
        workspace_root_id: String,
        snapshot_manifest: SnapshotManifest,
    ) -> Self {
        Self {
            toolchain_manifest,
            crate_id,
            workspace_root_id,
            snapshot_manifest,
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

/// Execution status for RustDepInfo action
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum ExecutionStatus {
    /// Compilation succeeded
    Success = 0,
    /// Compilation failed (rustc returned non-zero)
    CompilationFailed = 1,
    /// Snapshot was incomplete (missing workspace files)
    SnapshotIncomplete = 2,
    /// Referenced untracked generated files
    UntrackedGeneratedFiles = 3,
    /// Other execution failure
    ExecutionError = 4,
}

impl ExecutionStatus {
    /// Convert status to u8 for serialization
    pub fn to_u8(&self) -> u8 {
        match self {
            Self::Success => 0,
            Self::CompilationFailed => 1,
            Self::SnapshotIncomplete => 2,
            Self::UntrackedGeneratedFiles => 3,
            Self::ExecutionError => 4,
        }
    }

    /// Convert u8 to status for deserialization
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Success),
            1 => Some(Self::CompilationFailed),
            2 => Some(Self::SnapshotIncomplete),
            3 => Some(Self::UntrackedGeneratedFiles),
            4 => Some(Self::ExecutionError),
            _ => None,
        }
    }
}

/// A generated artifact that was written during compilation
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct GeneratedArtifact {
    /// Workspace-relative path (or generated-root-relative)
    pub path: String,

    /// Blake3 hash if the artifact was captured
    pub blob_hash: Option<String>,

    /// Provenance: which node/action produced this
    /// If empty, this is "generated-but-untracked" (should fail hermetic builds)
    pub producer_id: Option<String>,
}

/// Result from a RustDepInfo execution
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RustDepInfoResult {
    /// Execution status (as u8, use ExecutionStatus::from_u8 to decode)
    pub status_code: u8,

    /// Blake3 hash of the dep-info file content (stored in CAS)
    /// Don't inline large strings - always use CAS for dep-info text
    pub dep_info_text_blob: String,

    /// Observed inputs parsed from dep-info, normalized and classified
    /// This is the authoritative record used for cache keys
    pub observed_inputs: InputSet,

    /// Compilation outputs (rlib, rmeta, bin, etc.)
    pub outputs: Vec<CompilationOutput>,

    /// Generated artifacts written during compilation
    /// If dep-info references generated files, we need provenance
    pub writes: Vec<GeneratedArtifact>,

    /// Exit status code from rustc
    pub exit_code: i32,

    /// Blake3 hash of stdout content (bounded, stored in CAS)
    pub stdout_blob: Option<String>,

    /// Blake3 hash of stderr content (bounded, stored in CAS)
    pub stderr_blob: Option<String>,
}

impl RustDepInfoResult {
    /// Get the execution status
    pub fn status(&self) -> Option<ExecutionStatus> {
        ExecutionStatus::from_u8(self.status_code)
    }

    /// Set the execution status
    pub fn set_status(&mut self, status: ExecutionStatus) {
        self.status_code = status.to_u8();
    }
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
