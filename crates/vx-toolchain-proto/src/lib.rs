//! Toolchain protocol types shared between daemon, CAS, and execd.
//!
//! This crate defines the protocol for toolchain acquisition and materialization.
//! The key types are:
//! - Specs (RustToolchainSpec, ZigToolchainSpec) - what is requested
//! - ToolchainManifest - what was acquired (stored in CAS)
//! - MaterializationPlan - how to materialize on disk
//! - CasToolchain trait - RPC interface for toolchain operations

use facet::Facet;
use vx_cas_proto::Blake3Hash;

pub type SpecKey = Blake3Hash;
pub type ToolchainId = Blake3Hash;

// =============================================================================
// Toolchain Specs (What is Requested)
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum RustChannel {
    Stable,
    Beta,
    Nightly { date: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum RustComponent {
    Rustc,
    RustStd,
    Cargo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct RustToolchainSpec {
    pub channel: RustChannel,
    pub host: String,
    pub target: String,
    pub components: Vec<RustComponent>,
}

impl RustToolchainSpec {
    /// Compute canonical SpecKey.
    /// NOTE: No lowercasing - triples are already canonical. Just trim().
    pub fn spec_key(&self) -> Result<SpecKey, &'static str> {
        // Reject empty components early
        if self.components.is_empty() {
            return Err("components cannot be empty");
        }
        if self.host.trim().is_empty() {
            return Err("host cannot be empty");
        }
        if self.target.trim().is_empty() {
            return Err("target cannot be empty");
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"toolchain-spec-v1\n");
        hasher.update(b"kind:rust\n");

        match &self.channel {
            RustChannel::Stable => {
                hasher.update(b"channel:stable\n");
            }
            RustChannel::Beta => {
                hasher.update(b"channel:beta\n");
            }
            RustChannel::Nightly { date } => {
                let date = date.trim();
                if date.is_empty() {
                    return Err("nightly date cannot be empty");
                }
                hasher.update(b"channel:nightly:");
                hasher.update(date.as_bytes());
                hasher.update(b"\n");
            }
        }

        hasher.update(b"host:");
        hasher.update(self.host.trim().as_bytes());
        hasher.update(b"\n");

        hasher.update(b"target:");
        hasher.update(self.target.trim().as_bytes());
        hasher.update(b"\n");

        // Sort components for determinism
        let mut components: Vec<_> = self.components.iter().collect();
        components.sort_by_key(|c| match c {
            RustComponent::Rustc => 0,
            RustComponent::RustStd => 1,
            RustComponent::Cargo => 2,
        });

        hasher.update(b"components:");
        for (i, c) in components.iter().enumerate() {
            if i > 0 {
                hasher.update(b",");
            }
            let _ = match c {
                RustComponent::Rustc => hasher.update(b"rustc"),
                RustComponent::RustStd => hasher.update(b"rust-std"),
                RustComponent::Cargo => hasher.update(b"cargo"),
            };
        }
        hasher.update(b"\n");

        Ok(Blake3Hash(*hasher.finalize().as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct ZigToolchainSpec {
    pub version: String,
    pub host_platform: String,
}

impl ZigToolchainSpec {
    pub fn spec_key(&self) -> Result<SpecKey, &'static str> {
        if self.version.trim().is_empty() {
            return Err("version cannot be empty");
        }
        if self.host_platform.trim().is_empty() {
            return Err("host_platform cannot be empty");
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"toolchain-spec-v1\n");
        hasher.update(b"kind:zig\n");
        hasher.update(b"version:");
        hasher.update(self.version.trim().as_bytes());
        hasher.update(b"\n");
        hasher.update(b"platform:");
        hasher.update(self.host_platform.trim().as_bytes());
        hasher.update(b"\n");
        Ok(Blake3Hash(*hasher.finalize().as_bytes()))
    }
}

// =============================================================================
// Toolchain Manifest (What Was Acquired)
// =============================================================================

#[derive(Debug, Clone, Facet)]
pub struct ToolchainManifest {
    /// Schema version for format evolution
    pub schema_version: u32,
    /// Kind of toolchain
    pub kind: ToolchainKind,
    /// SpecKey that produced this manifest (for reverse lookup)
    pub spec_key: SpecKey,
    /// Content-derived toolchain identity
    pub toolchain_id: ToolchainId,
    /// When acquired (RFC3339)
    pub created_at: String,

    // === Rust-specific fields ===
    /// Resolved manifest date (for stable/beta this captures the actual version)
    pub rust_manifest_date: Option<String>,
    /// Rustc version string
    pub rust_version: Option<String>,

    // === Zig-specific fields ===
    pub zig_version: Option<String>,

    /// Components with their CAS blob references
    pub components: Vec<ToolchainComponentBlob>,
}

pub const TOOLCHAIN_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Facet)]
pub struct ToolchainComponentBlob {
    /// Component name: "rustc", "rust-std", "zig-exe", "zig-lib"
    pub name: String,
    /// Target triple (for rust-std)
    pub target: Option<String>,
    /// Compression format: "xz", "none", "tar"
    pub compression: String,
    /// CAS blob hash of the tarball/file bytes
    pub blob: Blake3Hash,
    /// Upstream SHA256 hex (for provenance/verification)
    pub sha256: String,
    /// Size in bytes
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum ToolchainKind {
    Rust,
    Zig,
}

// =============================================================================
// Materialization Plan (How to Materialize)
// =============================================================================

/// Materialization plan generated by CAS, executed by execd.
///
/// STABILITY CONTRACT: For a given manifest_hash, this plan is bit-for-bit
/// stable across time and casd versions within the same layout_version.
/// If plan generation changes, bump MATERIALIZATION_LAYOUT_VERSION.
#[derive(Debug, Clone, Facet)]
pub struct MaterializationPlan {
    pub toolchain_id: ToolchainId,
    /// Layout version - execd cache includes this to avoid corruption
    pub layout_version: u32,
    pub steps: Vec<MaterializeStep>,
}

pub const MATERIALIZATION_LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone, Facet)]
#[repr(u8)]
pub enum MaterializeStep {
    /// Ensure a directory exists (for explicit dir creation).
    /// relpath must be non-empty; use "." for toolchain root if needed.
    EnsureDir { relpath: String },
    /// Extract a tar.xz blob
    ExtractTarXz {
        blob: Blake3Hash,
        /// Number of path components to strip (usually 1)
        strip_components: u32,
        /// Destination subdirectory relative to toolchain root
        dest_subdir: String,
    },
    /// Write a single file from a blob
    WriteFile {
        /// Relative path within toolchain root
        relpath: String,
        blob: Blake3Hash,
        /// Unix mode (e.g., 0o755 for executables)
        mode: u32,
    },
    /// Create a symlink (target must not escape toolchain root)
    Symlink { relpath: String, target: String },
}

// =============================================================================
// CAS Toolchain RPCs
// =============================================================================

#[derive(Debug, Clone, Facet)]
pub struct EnsureToolchainResult {
    /// None if spec was invalid and couldn't be hashed
    pub spec_key: Option<SpecKey>,
    /// None on failure
    pub toolchain_id: Option<ToolchainId>,
    /// Hash of the ToolchainManifest in CAS
    pub manifest_hash: Option<Blake3Hash>,
    pub status: EnsureStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum EnsureStatus {
    Hit,        // Already in CAS
    Downloaded, // Just acquired
    Failed,     // Acquisition failed
}

/// CAS Toolchain service trait.
///
/// CAS is the sole component that downloads and stores toolchains.
/// Daemon and execd communicate only with CAS via these RPCs.
#[rapace::service]
pub trait CasToolchain {
    /// Ensure Rust toolchain exists in CAS. CAS downloads if needed.
    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult;

    /// Ensure Zig toolchain exists in CAS.
    async fn ensure_zig_toolchain(&self, spec: ZigToolchainSpec) -> EnsureToolchainResult;

    /// Get toolchain manifest by its hash
    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest>;

    /// Lookup manifest hash by spec key
    async fn lookup_toolchain_spec(&self, spec_key: SpecKey) -> Option<Blake3Hash>;

    /// Get materialization plan for a toolchain
    async fn get_materialization_plan(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<MaterializationPlan>;

    /// Stream blob chunks for extraction.
    ///
    /// NOTE: This is chunked delivery (Vec<u8> per chunk), not zero-copy.
    /// Each chunk involves allocation. Fine for v1 but don't assume this is
    /// free for multi-GB transfers.
    async fn stream_blob(&self, blob: Blake3Hash) -> rapace::Streaming<Vec<u8>>;
}
