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

/// ID for a chunked blob upload session
#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct BlobUploadId(pub u64);

/// Result of finishing a blob upload
#[derive(Debug, Clone, Facet)]
pub struct FinishBlobResult {
    pub success: bool,
    pub hash: Option<BlobHash>,
    pub error: Option<String>,
}

/// CAS service trait
///
/// CAS stores immutable content. Clients produce working directories.
#[rapace::service]
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
    // Chunked blob upload (for large files, avoids big allocations)
    // =========================================================================

    /// Begin a chunked blob upload, returns upload session ID
    async fn begin_blob(&self) -> BlobUploadId;

    /// Append a chunk to an in-progress upload
    async fn blob_chunk(&self, id: BlobUploadId, chunk: Vec<u8>);

    /// Finish the upload, returns the final blob hash
    async fn finish_blob(&self, id: BlobUploadId) -> FinishBlobResult;
}
