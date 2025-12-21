//! Build report types and persistence for vx
//!
//! This crate provides:
//! - Versioned report schema (BuildReport, NodeReport, etc.)
//! - ULID-based run ID generation
//! - Persistence to `.vx/runs/` directory
//!
//! Reports capture everything needed to answer "why did it rebuild?":
//! - What nodes ran vs hit cache
//! - What the cache key was
//! - What inputs contributed to the key
//! - Timing and diagnostic output (as CAS blob references)

use camino::{Utf8Path, Utf8PathBuf};
use facet::Facet;

/// Current report schema version.
/// Bump when schema changes in backwards-incompatible ways.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

// =============================================================================
// RUN ID
// =============================================================================

/// A time-ordered unique run identifier.
///
/// Uses ULID format: 26 characters, lexicographically sortable by time.
/// Example: "01ARZ3NDEKTSV4RRFFQ69G5FAV"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct RunId(pub String);

impl RunId {
    /// Generate a new RunId based on current time.
    ///
    /// ULIDs are 128-bit identifiers that:
    /// - Are lexicographically sortable
    /// - Encode timestamp in the first 48 bits
    /// - Have 80 bits of randomness to avoid collisions
    pub fn new() -> Self {
        // ULID format: TTTTTTTTTTRRRRRRRRRRRRRRR (10 timestamp chars + 16 random chars)
        // We use a simpler approach: timestamp_ms + random suffix
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Crockford's base32 alphabet (excludes I, L, O, U to avoid confusion)
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

        // Encode timestamp in 10 characters (48 bits)
        let mut chars = [0u8; 26];
        let mut t = timestamp as u64;
        for i in (0..10).rev() {
            chars[i] = ALPHABET[(t & 0x1F) as usize];
            t >>= 5;
        }

        // Fill remaining 16 characters with random data
        // Use nanoseconds and process ID for entropy (no unstable APIs)
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let random: u128 = nanos ^ (std::process::id() as u128).rotate_left(32);

        let mut r = random;
        for i in 10..26 {
            chars[i] = ALPHABET[(r & 0x1F) as usize];
            r >>= 5;
        }

        Self(String::from_utf8_lossy(&chars).to_string())
    }

    /// Parse a RunId from a string.
    pub fn from_str(s: &str) -> Option<Self> {
        // Basic validation: 26 characters, all valid Crockford base32
        if s.len() != 26 {
            return None;
        }
        for c in s.chars() {
            if !matches!(c, '0'..='9' | 'A'..='H' | 'J' | 'K' | 'M' | 'N' | 'P'..='T' | 'V'..='Z') {
                return None;
            }
        }
        Some(Self(s.to_string()))
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// =============================================================================
// BUILD REPORT
// =============================================================================

/// Complete report for a single build invocation.
#[derive(Debug, Clone, Facet)]
pub struct BuildReport {
    /// Schema version for forward compatibility
    pub schema: u32,
    /// Unique run identifier
    pub run_id: RunId,
    /// When the build started (unix ms)
    pub started_at_unix_ms: u64,
    /// When the build finished (unix ms)
    pub ended_at_unix_ms: u64,

    /// Workspace root (relative paths are relative to this)
    pub workspace_root: String,
    /// Build profile ("debug" or "release")
    pub profile: String,
    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target_triple: String,

    /// Toolchains used in this build
    pub toolchains: ToolchainsUsed,
    /// Per-node reports
    pub nodes: Vec<NodeReport>,

    /// Overall build success
    pub success: bool,
    /// Error message if build failed
    pub error: Option<String>,
}

impl BuildReport {
    /// Create a new BuildReport for a build that's starting.
    pub fn new(workspace_root: String, profile: String, target_triple: String) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            schema: REPORT_SCHEMA_VERSION,
            run_id: RunId::new(),
            started_at_unix_ms: now,
            ended_at_unix_ms: 0,
            workspace_root,
            profile,
            target_triple,
            toolchains: ToolchainsUsed::default(),
            nodes: Vec::new(),
            success: false,
            error: None,
        }
    }

    /// Finalize the report with success/failure status.
    pub fn finalize(&mut self, success: bool, error: Option<String>) {
        use std::time::{SystemTime, UNIX_EPOCH};

        self.ended_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.success = success;
        self.error = error;
    }

    /// Add a node report.
    pub fn add_node(&mut self, node: NodeReport) {
        self.nodes.push(node);
    }

    /// Total build duration in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.ended_at_unix_ms.saturating_sub(self.started_at_unix_ms)
    }

    /// Count of cache hits.
    pub fn cache_hits(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| matches!(n.cache, CacheOutcome::Hit { .. }))
            .count()
    }

    /// Count of cache misses.
    pub fn cache_misses(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| matches!(n.cache, CacheOutcome::Miss { .. }))
            .count()
    }
}

// =============================================================================
// TOOLCHAINS
// =============================================================================

/// Toolchains used in a build.
#[derive(Debug, Clone, Default, Facet)]
pub struct ToolchainsUsed {
    /// Rust toolchain (rustc)
    pub rust: Option<ToolchainRef>,
    /// Zig toolchain (zig cc)
    pub zig: Option<ToolchainRef>,
}

/// Reference to a toolchain.
#[derive(Debug, Clone, Facet)]
pub struct ToolchainRef {
    /// Toolchain identifier (e.g., content hash for hermetic toolchains)
    pub id: String,
    /// Human-readable version string
    pub version: Option<String>,
}

// =============================================================================
// NODE REPORT
// =============================================================================

/// Report for a single build node.
#[derive(Debug, Clone, Facet)]
pub struct NodeReport {
    /// Human-readable node identifier (e.g., "compile-bin:hello:aarch64-apple-darwin:release")
    pub node_id: String,
    /// Node kind for categorization
    pub kind: String,
    /// Cache key (hex)
    pub cache_key: String,
    /// Cache outcome
    pub cache: CacheOutcome,
    /// Execution timing
    pub timing: NodeTiming,

    /// Inputs that contributed to the cache key
    pub inputs: Vec<InputRecord>,
    /// Dependencies used (for nodes with dependencies)
    pub deps: Vec<DependencyRecord>,
    /// Outputs produced (with hashes)
    pub outputs: Vec<OutputRecord>,
    /// Invocation details (if executed)
    pub invocation: Option<InvocationRecord>,

    /// Diagnostic output
    pub diagnostics: DiagnosticsRecord,
}

/// Cache outcome for a node.
#[derive(Debug, Clone, Facet)]
#[repr(u8)]
pub enum CacheOutcome {
    /// Cache hit - output was retrieved from cache
    Hit {
        /// Manifest hash that was retrieved
        manifest: String,
    },
    /// Cache miss - node was executed
    Miss {
        /// Reason for the miss
        reason: MissReason,
    },
}

/// Reason for a cache miss.
#[derive(Debug, Clone, Facet)]
#[repr(u8)]
pub enum MissReason {
    /// First time building this node
    FirstBuild,
    /// Cache key not found in CAS
    KeyNotFound,
    /// A dependency changed
    DependencyChanged,
    /// Unknown reason (try not to use this)
    Unknown,
}

impl std::fmt::Display for MissReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MissReason::FirstBuild => write!(f, "first build"),
            MissReason::KeyNotFound => write!(f, "key not found"),
            MissReason::DependencyChanged => write!(f, "dependency changed"),
            MissReason::Unknown => write!(f, "unknown"),
        }
    }
}

/// Timing information for a node.
#[derive(Debug, Clone, Default, Facet)]
pub struct NodeTiming {
    /// Time spent in queue (if applicable)
    pub queued_ms: u64,
    /// Time spent executing
    pub execute_ms: u64,
}

// =============================================================================
// INPUTS AND OUTPUTS
// =============================================================================

/// Record of an input that contributed to a cache key.
#[derive(Debug, Clone, Facet)]
pub struct InputRecord {
    /// Label describing the input (e.g., "src/main.rs", "rustc", "Cargo.toml")
    pub label: String,
    /// Value of the input (hash, version string, etc.)
    pub value: String,
}

/// Record of a dependency used by a node.
///
/// This captures the full provenance of a dependency: what it's called,
/// what crate it is, and the exact rlib + manifest hashes that were used.
#[derive(Debug, Clone, Facet)]
pub struct DependencyRecord {
    /// Extern crate name (as used in --extern)
    pub extern_name: String,
    /// Unique crate identifier (hex)
    pub crate_id: String,
    /// Blake3 hash of the rlib file (hex)
    pub rlib_hash: String,
    /// Manifest hash from CAS (hex)
    pub manifest_hash: String,
}

/// Record of an output produced by a node.
#[derive(Debug, Clone, Facet)]
pub struct OutputRecord {
    /// Logical name ("bin", "rlib", "o", "exe")
    pub logical: String,
    /// Manifest hash (if stored in CAS)
    pub manifest: Option<String>,
    /// Blob hash (if stored in CAS)
    pub blob: Option<String>,
    /// Materialized path (workspace-relative)
    pub path: Option<String>,
}

/// Invocation details for an executed node.
#[derive(Debug, Clone, Facet)]
pub struct InvocationRecord {
    /// Program that was executed
    pub program: String,
    /// Arguments passed
    pub args: Vec<String>,
    /// Working directory (workspace-relative if possible)
    pub cwd: String,
    /// Exit code
    pub exit_code: i32,
}

/// Diagnostic output from a node (stored as CAS blob references).
#[derive(Debug, Clone, Default, Facet)]
pub struct DiagnosticsRecord {
    /// Blob hash of stdout (if captured)
    pub stdout_blob: Option<String>,
    /// Blob hash of stderr (if captured)
    pub stderr_blob: Option<String>,
}

// =============================================================================
// PERSISTENCE
// =============================================================================

/// Persistence layer for build reports.
pub struct ReportStore {
    /// Path to .vx/runs/ directory
    runs_dir: Utf8PathBuf,
}

impl ReportStore {
    /// Create a new ReportStore for a project.
    pub fn new(project_root: &Utf8Path) -> Self {
        Self {
            runs_dir: project_root.join(".vx/runs"),
        }
    }

    /// Initialize the runs directory.
    pub fn init(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.runs_dir)
    }

    /// Save a build report.
    pub fn save(&self, report: &BuildReport) -> std::io::Result<()> {
        self.init()?;

        let json = facet_json::to_string(report);
        let path = self.runs_dir.join(format!("{}.json", report.run_id));

        // Atomic write via temp file
        let tmp_path = self.runs_dir.join(format!("{}.json.tmp", report.run_id));
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &path)?;

        // Update "latest" symlink
        let latest_path = self.runs_dir.join("latest");
        let _ = std::fs::remove_file(&latest_path);
        std::fs::write(&latest_path, &report.run_id.0)?;

        Ok(())
    }

    /// Load a build report by run ID.
    pub fn load(&self, run_id: &RunId) -> std::io::Result<Option<BuildReport>> {
        let path = self.runs_dir.join(format!("{}.json", run_id));
        if !path.exists() {
            return Ok(None);
        }

        let json = std::fs::read_to_string(&path)?;
        let report: BuildReport = facet_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Some(report))
    }

    /// Get the run ID of the latest build.
    pub fn latest_run_id(&self) -> std::io::Result<Option<RunId>> {
        let latest_path = self.runs_dir.join("latest");
        if !latest_path.exists() {
            return Ok(None);
        }

        let id_str = std::fs::read_to_string(&latest_path)?;
        Ok(RunId::from_str(id_str.trim()))
    }

    /// Load the latest build report.
    pub fn load_latest(&self) -> std::io::Result<Option<BuildReport>> {
        if let Some(run_id) = self.latest_run_id()? {
            self.load(&run_id)
        } else {
            Ok(None)
        }
    }

    /// List all run IDs in chronological order (oldest first).
    pub fn list_runs(&self) -> std::io::Result<Vec<RunId>> {
        if !self.runs_dir.exists() {
            return Ok(Vec::new());
        }

        let mut runs = Vec::new();
        for entry in std::fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "json" {
                    if let Some(stem) = path.file_stem() {
                        if let Some(run_id) = RunId::from_str(&stem.to_string_lossy()) {
                            runs.push(run_id);
                        }
                    }
                }
            }
        }

        // Sort lexicographically (ULIDs are time-ordered, so this gives chronological order)
        runs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(runs)
    }

    /// Get the previous run (second most recent).
    pub fn previous_run_id(&self) -> std::io::Result<Option<RunId>> {
        let runs = self.list_runs()?;
        if runs.len() >= 2 {
            Ok(Some(runs[runs.len() - 2].clone()))
        } else {
            Ok(None)
        }
    }

    /// Load the previous build report.
    pub fn load_previous(&self) -> std::io::Result<Option<BuildReport>> {
        if let Some(run_id) = self.previous_run_id()? {
            self.load(&run_id)
        } else {
            Ok(None)
        }
    }
}

// =============================================================================
// DIFF
// =============================================================================

/// Difference between two build runs.
#[derive(Debug, Clone)]
pub struct RunDiff {
    /// Run ID of the "before" run
    pub before: RunId,
    /// Run ID of the "after" run
    pub after: RunId,
    /// Nodes that changed from hit to miss
    pub flipped_to_miss: Vec<NodeDiff>,
    /// Nodes that changed from miss to hit
    pub flipped_to_hit: Vec<String>,
    /// Toolchain changes
    pub toolchain_changes: Vec<ToolchainChange>,
}

/// Difference in a single node between runs.
#[derive(Debug, Clone)]
pub struct NodeDiff {
    /// Node ID
    pub node_id: String,
    /// Changed inputs (label → (old_value, new_value))
    pub changed_inputs: Vec<(String, String, String)>,
}

/// A toolchain change between runs.
#[derive(Debug, Clone)]
pub struct ToolchainChange {
    /// Toolchain name ("rust", "zig", etc.)
    pub name: String,
    /// Old version/ID
    pub old: Option<String>,
    /// New version/ID
    pub new: Option<String>,
}

impl RunDiff {
    /// Compute the diff between two build reports.
    pub fn compute(before: &BuildReport, after: &BuildReport) -> Self {
        let mut diff = RunDiff {
            before: before.run_id.clone(),
            after: after.run_id.clone(),
            flipped_to_miss: Vec::new(),
            flipped_to_hit: Vec::new(),
            toolchain_changes: Vec::new(),
        };

        // Build maps for quick lookup
        let before_nodes: std::collections::HashMap<&str, &NodeReport> =
            before.nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

        let after_nodes: std::collections::HashMap<&str, &NodeReport> =
            after.nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

        // Find nodes that flipped
        for (node_id, after_node) in &after_nodes {
            if let Some(before_node) = before_nodes.get(node_id) {
                let was_hit = matches!(before_node.cache, CacheOutcome::Hit { .. });
                let is_hit = matches!(after_node.cache, CacheOutcome::Hit { .. });

                if was_hit && !is_hit {
                    // Flipped from hit to miss - find what changed
                    let changed_inputs = Self::diff_inputs(&before_node.inputs, &after_node.inputs);
                    diff.flipped_to_miss.push(NodeDiff {
                        node_id: node_id.to_string(),
                        changed_inputs,
                    });
                } else if !was_hit && is_hit {
                    diff.flipped_to_hit.push(node_id.to_string());
                }
            }
        }

        // Check toolchain changes
        if let (Some(before_rust), Some(after_rust)) =
            (&before.toolchains.rust, &after.toolchains.rust)
        {
            if before_rust.id != after_rust.id {
                diff.toolchain_changes.push(ToolchainChange {
                    name: "rust".to_string(),
                    old: before_rust.version.clone(),
                    new: after_rust.version.clone(),
                });
            }
        }

        if let (Some(before_zig), Some(after_zig)) = (&before.toolchains.zig, &after.toolchains.zig)
        {
            if before_zig.id != after_zig.id {
                diff.toolchain_changes.push(ToolchainChange {
                    name: "zig".to_string(),
                    old: before_zig.version.clone(),
                    new: after_zig.version.clone(),
                });
            }
        }

        diff
    }

    fn diff_inputs(
        before: &[InputRecord],
        after: &[InputRecord],
    ) -> Vec<(String, String, String)> {
        let before_map: std::collections::HashMap<&str, &str> =
            before.iter().map(|i| (i.label.as_str(), i.value.as_str())).collect();

        let mut changes = Vec::new();
        for input in after {
            if let Some(&old_value) = before_map.get(input.label.as_str()) {
                if old_value != input.value {
                    changes.push((
                        input.label.clone(),
                        old_value.to_string(),
                        input.value.clone(),
                    ));
                }
            } else {
                // New input
                changes.push((input.label.clone(), "(none)".to_string(), input.value.clone()));
            }
        }

        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_id_generation() {
        let id1 = RunId::new();
        let id2 = RunId::new();

        // Should be different
        assert_ne!(id1.0, id2.0);

        // Should be 26 characters
        assert_eq!(id1.0.len(), 26);
        assert_eq!(id2.0.len(), 26);

        // Should parse back
        assert!(RunId::from_str(&id1.0).is_some());
    }

    #[test]
    fn test_run_id_ordering() {
        // Generate IDs with a small delay to ensure different timestamps
        let id1 = RunId::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let id2 = RunId::new();

        // id2 should sort after id1 (lexicographic ordering = chronological for ULIDs)
        assert!(id2.0 >= id1.0);
    }

    #[test]
    fn test_build_report_creation() {
        let report = BuildReport::new(
            "/home/user/project".to_string(),
            "release".to_string(),
            "aarch64-apple-darwin".to_string(),
        );

        assert_eq!(report.schema, REPORT_SCHEMA_VERSION);
        assert_eq!(report.profile, "release");
        assert!(!report.success);
        assert!(report.nodes.is_empty());
    }

    #[test]
    fn test_build_report_finalize() {
        let mut report = BuildReport::new(
            "/home/user/project".to_string(),
            "debug".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        );

        report.finalize(true, None);

        assert!(report.success);
        assert!(report.ended_at_unix_ms >= report.started_at_unix_ms);
    }
}
