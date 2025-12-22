//! Registry crate protocol types shared between daemon, CAS, and execd.
//!
//! This crate defines the protocol for crates.io registry crate acquisition
//! and materialization. The key types are:
//! - RegistrySpec - identifies a specific crate version from a registry
//! - RegistryCrateManifest - what was acquired (stored in CAS)
//! - CasRegistry trait - RPC interface for registry operations

use facet::Facet;
use vx_cas_proto::Blake3Hash;

pub type SpecKey = Blake3Hash;

/// The crates.io registry URL (the only supported registry in v1)
pub const CRATES_IO_REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Current schema version for registry crate manifests.
/// Bump this when the manifest format changes.
pub const REGISTRY_MANIFEST_SCHEMA_VERSION: u32 = 1;

// =============================================================================
// Registry Spec (What is Requested)
// =============================================================================

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
    pub fn spec_key(&self) -> Result<SpecKey, &'static str> {
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

// =============================================================================
// Registry Crate Manifest (What Was Acquired)
// =============================================================================

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

// =============================================================================
// RPC Types
// =============================================================================

/// Result of ensuring a registry crate exists in CAS.
#[derive(Debug, Clone, Facet)]
pub struct EnsureRegistryCrateResult {
    /// The computed spec key (None if spec was invalid)
    pub spec_key: Option<SpecKey>,
    /// Hash of the RegistryCrateManifest in CAS (None on failure)
    pub manifest_hash: Option<Blake3Hash>,
    /// Status of the operation
    pub status: EnsureStatus,
    /// Error message if status is Failed
    pub error: Option<String>,
}

/// Status of an ensure operation.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum EnsureStatus {
    /// Already in CAS
    Hit,
    /// Just downloaded and stored
    Downloaded,
    /// Acquisition failed
    Failed,
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

// =============================================================================
// CAS Registry RPCs
// =============================================================================

/// CAS Registry service trait.
///
/// CAS is the sole component that downloads and stores registry crates.
/// Daemon and execd communicate only with CAS via these RPCs.
#[rapace::service]
#[allow(async_fn_in_trait)]
pub trait CasRegistry {
    /// Ensure a registry crate exists in CAS. CAS downloads if needed.
    ///
    /// This downloads the .crate tarball from crates.io, verifies the SHA256
    /// checksum, validates the tarball structure, stores the bytes as a blob,
    /// and creates a RegistryCrateManifest.
    async fn ensure_registry_crate(&self, spec: RegistrySpec) -> EnsureRegistryCrateResult;

    /// Get a registry crate manifest by its hash.
    async fn get_registry_manifest(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<RegistryCrateManifest>;

    /// Lookup manifest hash by spec key.
    async fn lookup_registry_spec(&self, spec_key: SpecKey) -> Option<Blake3Hash>;
}

// =============================================================================
// Blanket implementation for Arc<T>
// =============================================================================

impl<T: CasRegistry + Send + Sync> CasRegistry for std::sync::Arc<T> {
    async fn ensure_registry_crate(&self, spec: RegistrySpec) -> EnsureRegistryCrateResult {
        (**self).ensure_registry_crate(spec).await
    }

    async fn get_registry_manifest(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<RegistryCrateManifest> {
        (**self).get_registry_manifest(manifest_hash).await
    }

    async fn lookup_registry_spec(&self, spec_key: SpecKey) -> Option<Blake3Hash> {
        (**self).lookup_registry_spec(spec_key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Valid 64-character hex checksums for testing
    const CHECKSUM_A: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const CHECKSUM_B: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn spec_key_is_deterministic() {
        let spec1 = RegistrySpec::crates_io("serde", "1.0.197", CHECKSUM_A);
        let spec2 = RegistrySpec::crates_io("serde", "1.0.197", CHECKSUM_A);

        assert_eq!(spec1.spec_key().unwrap(), spec2.spec_key().unwrap());
    }

    #[test]
    fn spec_key_differs_by_version() {
        let spec1 = RegistrySpec::crates_io("serde", "1.0.197", CHECKSUM_A);
        let spec2 = RegistrySpec::crates_io("serde", "1.0.198", CHECKSUM_B);

        assert_ne!(spec1.spec_key().unwrap(), spec2.spec_key().unwrap());
    }

    #[test]
    fn spec_key_checksum_is_case_insensitive() {
        let spec1 = RegistrySpec::crates_io(
            "serde",
            "1.0.197",
            "ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234",
        );
        let spec2 = RegistrySpec::crates_io(
            "serde",
            "1.0.197",
            "abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234",
        );

        assert_eq!(spec1.spec_key().unwrap(), spec2.spec_key().unwrap());
    }

    #[test]
    fn spec_key_rejects_empty_fields() {
        let spec = RegistrySpec::crates_io("", "1.0.0", CHECKSUM_A);
        assert!(spec.spec_key().is_err());

        let spec = RegistrySpec::crates_io("serde", "", CHECKSUM_A);
        assert!(spec.spec_key().is_err());

        let spec = RegistrySpec::crates_io("serde", "1.0.0", "");
        assert!(spec.spec_key().is_err());
    }

    #[test]
    fn spec_key_rejects_invalid_checksum_length() {
        let spec = RegistrySpec::crates_io("serde", "1.0.0", "abc123");
        assert!(spec.spec_key().is_err());
    }

    #[test]
    fn download_url_is_correct() {
        let spec = RegistrySpec::crates_io("serde", "1.0.197", CHECKSUM_A);
        assert_eq!(
            spec.download_url(),
            "https://crates.io/api/v1/crates/serde/1.0.197/download"
        );
    }
}
