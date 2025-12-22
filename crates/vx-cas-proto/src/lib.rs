//! CAS service protocol definitions
//!
//! CAS stores immutable content. Clients produce working directories.
//!
//! The CAS is purely about:
//! - put/get blobs (content-addressed by blake3)
//! - put/get manifests (structured build outputs)
//! - lookup/publish cache keys (mapping inputs → outputs)

use facet::Facet;

/// Current cache key schema version.
/// Bump this when the cache key canonicalization changes.
pub const CACHE_KEY_SCHEMA_VERSION: u32 = 1;

/// A blake3 hash, used for blobs, manifests, and cache keys.
/// Internally stored as raw bytes; hex formatting is for display only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Facet)]
pub struct Blake3Hash(#[facet(sensitive)] pub [u8; 32]);

impl Blake3Hash {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Get first 16 hex chars (8 bytes) for display
    pub fn short_hex(&self) -> String {
        self.0[..8].iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut arr = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex_str = std::str::from_utf8(chunk).ok()?;
            arr[i] = u8::from_str_radix(hex_str, 16).ok()?;
        }
        Some(Self(arr))
    }
}

impl std::fmt::Display for Blake3Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Hash of a blob (raw bytes)
pub type BlobHash = Blake3Hash;

/// Hash of a manifest (JSON document)
pub type ManifestHash = Blake3Hash;

/// Hash of all inputs (cache lookup key)
pub type CacheKey = Blake3Hash;

/// A stable, human-readable node identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Output entry in a manifest
#[derive(Debug, Clone, Facet)]
pub struct OutputEntry {
    /// Logical name ("bin", "rlib", etc.)
    pub logical: String,
    /// Filename for materialization ("hello", "libfoo.rlib")
    pub filename: String,
    /// Hash of the blob
    pub blob: BlobHash,
    /// Whether the file should be executable
    pub executable: bool,
}

/// A node manifest (what a node produced)
#[derive(Debug, Clone, Facet)]
pub struct NodeManifest {
    pub node_id: NodeId,
    pub cache_key: CacheKey,
    pub produced_at: String,
    pub outputs: Vec<OutputEntry>,
}

/// Result of publishing a cache entry
#[derive(Debug, Clone, Facet)]
pub struct PublishResult {
    pub success: bool,
    pub error: Option<String>,
}

/// CAS service trait
///
/// CAS stores immutable content. Clients produce working directories.
#[rapace::service]
#[allow(async_fn_in_trait)]
pub trait Cas {
    // =========================================================================
    // Cache key operations
    // =========================================================================

    /// Look up a cache key, returns manifest hash if found
    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash>;

    /// Publish a cache entry atomically.
    /// Validates that the manifest exists (and optionally its blobs) before
    /// writing the cache key → manifest hash mapping.
    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult;

    // =========================================================================
    // Manifest operations
    // =========================================================================

    /// Store a manifest, returns its hash
    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash;

    /// Get a manifest by hash
    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest>;

    // =========================================================================
    // Blob operations (small blobs, recommended < 10MB)
    // =========================================================================

    /// Store a blob, returns its hash.
    /// For small blobs. Use chunked upload for large files.
    // NOTE: Vec<u8> is not zerocopy-friendly. For v0 this is acceptable.
    async fn put_blob(&self, data: Vec<u8>) -> BlobHash;

    /// Get a blob by hash.
    /// Returns None if not found.
    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>>;

    /// Check if a blob exists
    async fn has_blob(&self, hash: BlobHash) -> bool;

    // =========================================================================
    // Streaming blob operations (for large files like toolchains)
    // =========================================================================

    /// Stream blob chunks for extraction.
    ///
    /// NOTE: This is chunked delivery (Vec<u8> per chunk), not zero-copy.
    /// Each chunk involves allocation. Fine for v1 but don't assume this is
    /// free for multi-GB transfers.
    async fn stream_blob(&self, blob: Blake3Hash) -> rapace::Streaming<Vec<u8>>;

    // =========================================================================
    // Toolchain operations (called by execd/daemon)
    // =========================================================================

    /// Get toolchain manifest by its hash
    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest>;

    /// Get materialization plan for a toolchain
    async fn get_materialization_plan(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<MaterializationPlan>;

    // =========================================================================
    // Registry operations (called by execd/daemon)
    // =========================================================================

    /// Ensure a registry crate exists in CAS. CAS downloads if needed.
    async fn ensure_registry_crate(&self, spec: RegistrySpec) -> EnsureRegistryCrateResult;

    /// Get a registry crate manifest by its hash
    async fn get_registry_manifest(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<RegistryCrateManifest>;

    /// Lookup manifest hash by spec key
    async fn lookup_registry_spec(&self, spec_key: Blake3Hash) -> Option<Blake3Hash>;
}

// =============================================================================
// Toolchain Types
// =============================================================================

pub type ToolchainSpecKey = Blake3Hash;
pub type ToolchainId = Blake3Hash;

// Toolchain Specs (What is Requested)

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
    pub fn spec_key(&self) -> Result<ToolchainSpecKey, &'static str> {
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
    pub fn spec_key(&self) -> Result<ToolchainSpecKey, &'static str> {
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

// Toolchain Manifest (What Was Acquired)

#[derive(Debug, Clone, Facet)]
pub struct ToolchainManifest {
    /// Schema version for format evolution
    pub schema_version: u32,
    /// Kind of toolchain
    pub kind: ToolchainKind,
    /// SpecKey that produced this manifest (for reverse lookup)
    pub spec_key: ToolchainSpecKey,
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

// Materialization Plan (How to Materialize)

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

// Toolchain RPC Result Types

#[derive(Debug, Clone, Facet)]
pub struct EnsureToolchainResult {
    /// None if spec was invalid and couldn't be hashed
    pub spec_key: Option<ToolchainSpecKey>,
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

// =============================================================================
// Registry Types
// =============================================================================

pub type RegistrySpecKey = Blake3Hash;

/// The crates.io registry URL (the only supported registry in v1)
pub const CRATES_IO_REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Current schema version for registry crate manifests.
/// Bump this when the manifest format changes.
pub const REGISTRY_MANIFEST_SCHEMA_VERSION: u32 = 1;

// Registry Spec (What is Requested)

/// Identifies a specific crate version from a registry.
///
/// In v1, only crates.io is supported. The checksum is the SHA256 from
/// Cargo.lock, which is verified against the downloaded tarball.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct RegistrySpec {
    /// Registry URL (v1 must equal CRATES_IO_REGISTRY)
    pub registry_url: String,
    /// Crate name
    pub name: String,
    /// Exact version string (e.g., "1.0.197")
    pub version: String,
    /// SHA256 checksum from Cargo.lock (hex-encoded)
    pub checksum: String,
}

impl RegistrySpec {
    /// Create a new RegistrySpec for a crates.io crate.
    pub fn crates_io(
        name: impl Into<String>,
        version: impl Into<String>,
        checksum: impl Into<String>,
    ) -> Self {
        Self {
            registry_url: CRATES_IO_REGISTRY.to_string(),
            name: name.into(),
            version: version.into(),
            checksum: checksum.into(),
        }
    }

    /// Compute canonical SpecKey for cache lookups.
    ///
    /// The spec key is a blake3 hash of the normalized spec fields.
    /// This is used to map spec -> manifest in CAS.
    pub fn spec_key(&self) -> Result<RegistrySpecKey, &'static str> {
        if self.name.trim().is_empty() {
            return Err("name cannot be empty");
        }
        if self.version.trim().is_empty() {
            return Err("version cannot be empty");
        }
        if self.checksum.trim().is_empty() {
            return Err("checksum cannot be empty");
        }
        if self.checksum.len() != 64 {
            return Err("checksum must be 64 hex characters (SHA256)");
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"registry-spec-v1\n");
        hasher.update(b"registry:");
        hasher.update(self.registry_url.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"name:");
        hasher.update(self.name.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"version:");
        hasher.update(self.version.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"checksum:");
        hasher.update(self.checksum.to_lowercase().as_bytes());
        hasher.update(b"\n");

        Ok(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    /// Returns the download URL for this crate from crates.io.
    pub fn download_url(&self) -> String {
        format!(
            "https://crates.io/api/v1/crates/{}/{}/download",
            self.name, self.version
        )
    }
}

// Registry Crate Manifest (What Was Acquired)

/// Manifest for an acquired registry crate, stored in CAS.
///
/// This records what was downloaded and where the tarball bytes are stored.
/// The manifest is content-addressed and immutable once written.
#[derive(Debug, Clone, Facet)]
pub struct RegistryCrateManifest {
    /// Schema version for format evolution
    pub schema_version: u32,
    /// The spec that produced this manifest (includes checksum for provenance)
    pub spec: RegistrySpec,
    /// Blake3 hash of the .crate tarball bytes in CAS
    pub crate_tarball_blob: Blake3Hash,
    /// When acquired (RFC3339)
    pub created_at: String,
}

// Registry RPC Types

/// Result of ensuring a registry crate exists in CAS.
#[derive(Debug, Clone, Facet)]
pub struct EnsureRegistryCrateResult {
    /// The computed spec key (None if spec was invalid)
    pub spec_key: Option<RegistrySpecKey>,
    /// Hash of the RegistryCrateManifest in CAS (None on failure)
    pub manifest_hash: Option<Blake3Hash>,
    /// Status of the operation
    pub status: EnsureStatus,
    /// Error message if status is Failed
    pub error: Option<String>,
}

/// Result of materializing a registry crate.
#[derive(Debug, Clone, Facet)]
pub struct RegistryMaterializationResult {
    /// Workspace-relative path to the materialized crate (e.g., ".vx/registry/serde/1.0.197")
    pub workspace_rel_path: String,
    /// Global cache path (for debugging/reporting)
    pub global_cache_path: String,
    /// Whether the workspace copy was already present with matching checksum
    pub was_cached: bool,
}
