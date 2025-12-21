//! Exec action definitions for remote Rust compilation
//!
//! This module defines the RustDepInfo action that runs rustc remotely
//! via execd to produce both compilation outputs and dep-info.

use crate::input_set::InputSet;
use facet::Facet;

/// Maximum number of snapshot expansion retries before failing
///
/// Prevents infinite loops when dep-info repeatedly references new files
/// (e.g., proc macros with unbounded file reading)
pub const MAX_SNAPSHOT_RETRIES: u32 = 3;

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
/// - RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS are REJECTED (never in env)
/// - All cfg-affecting flags MUST be in rustc_args (target, edition, --cfg, etc.)
/// - env allowlist is minimal and controlled
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
    /// Also used as sandbox directory name: /work/<workspace_root_id>/...
    pub workspace_root_id: String,

    /// Snapshot manifest hash (Blake3)
    /// References the SnapshotManifest in CAS describing files to materialize
    /// Execd fetches via get_manifest() then fetches blobs as needed
    /// Do NOT send manifest inline - can be thousands of entries
    pub snapshot_manifest_hash: String,

    /// Arguments to pass to rustc
    /// MUST include --emit=dep-info and a deterministic --dep-info output path
    /// MUST include all flags affecting cfg/codegen (target, edition, --cfg, --extern, -C, --cap-lints)
    pub rustc_args: Vec<String>,

    /// Working directory (workspace-relative)
    /// Usually the crate directory or workspace root
    pub cwd_rel: String,

    /// Explicit environment variables (strict allowlist only)
    /// Ambient environment is NOT inherited. Should be almost empty.
    /// FORBIDDEN: RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS, CARGO_* (except allowed below)
    /// ALLOWED (examples): CARGO_PKG_VERSION, CARGO_CRATE_NAME (if fully controlled)
    pub env: Vec<(String, String)>,
}

impl RustDepInfoAction {
    /// Create a new RustDepInfo action
    pub fn new(
        toolchain_manifest: String,
        crate_id: String,
        workspace_root_id: String,
        snapshot_manifest_hash: String,
    ) -> Self {
        Self {
            toolchain_manifest,
            crate_id,
            workspace_root_id,
            snapshot_manifest_hash,
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
///
/// These are **observed side-effects** - files written that weren't declared outputs.
///
/// Examples:
/// - OUT_DIR files from build.rs
/// - Proc macro generated code
/// - Debug files, incremental cache
///
/// ## Policy Gates (V1 vs V2)
///
/// **V1 (Current)**: Track + report
/// - Observe writes, record in result
/// - Log warning if producer_id is None ("generated-but-untracked")
/// - Don't block builds
///
/// **V2 (Future)**: Enforce contracts
/// - Require explicit declaration: "this node may produce generated files in OUT_DIR"
/// - Require provenance: all writes must have producer_id
/// - Reject builds with undeclared writes
/// - Matches "sandbox/deny FS access for proc macros" direction
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct GeneratedArtifact {
    /// Workspace-relative path (or generated-root-relative)
    pub path: String,

    /// Blake3 hash if the artifact was captured
    pub blob_hash: Option<String>,

    /// Provenance: which node/action produced this
    /// None = "generated-but-untracked" (V1: warn, V2: reject)
    pub producer_id: Option<String>,
}

/// Result from a RustDepInfo execution
///
/// ## outputs vs writes Separation
///
/// **outputs**: Declared compilation artifacts
/// - What we asked rustc to produce (via --emit, --crate-type)
/// - Expected and required
/// - Examples: libfoo.rlib, libfoo.rmeta
///
/// **writes**: Observed side-effect files
/// - Files written but not declared as outputs
/// - May or may not have provenance
/// - Examples: OUT_DIR/* from build.rs, incremental cache
/// - V1: track + report, V2: require declaration
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

    /// Declared compilation outputs (rlib, rmeta, bin, etc.)
    /// These are the expected artifacts from rustc based on --emit
    pub outputs: Vec<CompilationOutput>,

    /// Observed side-effect writes (generated files with optional provenance)
    /// If dep-info references generated files, track them here
    /// V1: informational, V2: enforce declaration contract
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
///
/// These are the **declared outputs** for the node - the artifacts we expect
/// rustc to produce based on --emit flags and --crate-type.
///
/// Examples: libfoo.rlib, libfoo.rmeta, foo (bin), libfoo.d (dep-info)
///
/// Contrast with `writes` in RustDepInfoResult which are **observed side-effects**.
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
///    - Create SnapshotManifest → store in CAS with hash
///    - Store snapshot_manifest_hash for RustDepInfoAction
///
/// 2. **Execute Rustc** (execd, remote):
///    - Fetch SnapshotManifest from CAS using snapshot_manifest_hash
///    - Materialize toolchain from toolchain_manifest
///    - Materialize workspace to /work/<workspace_root_id>/...
///    - Run: `rustc --emit=dep-info,rlib,rmeta <args>` in sandbox
///    - Upload all outputs (including dep-info) to CAS
///    - Return RustDepInfoResult with observed_inputs
///
/// 3. **Parse Dep-Info** (execd, remote):
///    - Parse Makefile-style dep-info format
///    - Normalize paths: strip /work/<workspace_root_id>/ prefix
///    - Paths that don't map to workspace become ExternalDep
///    - Classify into InputSet categories (workspace/toolchain/generated/external)
///    - Return observed_inputs in RustDepInfoResult
///
/// 4. **Compute Cache Key** (daemon, local):
///    - Key = hash(toolchain_manifest, rustc_args, observed_inputs, workspace_file_hashes)
///    - Store outputs under this key
///    - Store NodeReport with observed_inputs for future lookups
///
/// ## Cache Lookup Strategy
///
/// **Bootstrap Optimization**: Use last known observed_inputs
/// - On cache lookup, load previous NodeReport for this node
/// - If found, use stored observed_inputs to compute cache key
/// - Try cache lookup with this key (bootstrap hit)
/// - If hit → skip execution, return cached outputs
/// - If miss → execute RustDepInfo action, get new observed_inputs
///
/// **First Build After Clean**:
/// - No previous NodeReport exists
/// - Must execute to get observed_inputs
/// - Store result + observed_inputs for future bootstrap
///
/// This avoids recompilation when nothing changed, even after clean builds.
///
/// ## Path Normalization Contract
///
/// **Execd Sandbox Layout**:
/// - Fixed root: `/work/<workspace_root_id>/`
/// - Workspace files materialized at: `/work/<workspace_root_id>/<rel_path>`
/// - Rustc runs with cwd: `/work/<workspace_root_id>/<cwd_rel>`
///
/// **Path Mapping Rules** (bijection required for stable cache keys):
/// 1. If path starts with `/work/<workspace_root_id>/` → WorkspaceFile (strip prefix)
/// 2. If path matches toolchain roots → ToolchainFile (keep absolute)
/// 3. If path matches generated roots → GeneratedFile (extract ID)
/// 4. Otherwise → ExternalDep (reject unless explicitly allowed)
///
/// **Invariant**: Same source + config MUST produce identical workspace-relative paths
/// across all workers, regardless of worker sandbox location.
///
/// ## Snapshot Incomplete Detection & Retry
///
/// **When dep-info references workspace file not in snapshot**:
/// - Execd returns status: SnapshotIncomplete
/// - Result includes missing_workspace_files list
///
/// **Daemon Retry Policy**:
/// - Auto-add eligible: workspace files only (*.rs, assets if declared)
/// - Hard reject: outside-workspace paths (never auto-added)
/// - Retry cap: MAX_SNAPSHOT_RETRIES = 3 (prevent infinite loops)
/// - After cap: fail with "snapshot expansion limit exceeded"
///
/// **Common Triggers**:
/// - `include_str!("../assets/template.html")` → add to extra_inputs
/// - `#[path = "../../other_crate/mod.rs"]` → likely error (cross-crate path)
/// - Proc macro reading files → require explicit declaration
///
/// With maximalist *.rs snapshotting, retries should be rare (non-.rs assets only).
pub struct OrchestrationDoc;
