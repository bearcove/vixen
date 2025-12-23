//! Toolchain Manager (Inflight Deduplication)

use jiff::Timestamp;
use std::collections::HashMap;
use std::sync::Arc;
use vx_cas_proto::*;
use vx_cas_proto::{
    EnsureStatus as ToolchainEnsureStatus, EnsureToolchainResult, RustChannel, RustToolchainSpec,
    TOOLCHAIN_MANIFEST_SCHEMA_VERSION, ToolchainComponentTree, ToolchainKind, ToolchainManifest,
    ToolchainSpecKey, ZigToolchainSpec,
};
use vx_tarball::Compression;

use crate::types::CasService;

// =============================================================================
// HTTP Download Functions (Rust Toolchains)
// =============================================================================

/// Fetch a channel manifest from static.rust-lang.org
async fn fetch_channel_manifest(
    channel: &vx_toolchain::Channel,
) -> Result<vx_toolchain::ChannelManifest, vx_toolchain::ToolchainError> {
    let url = vx_toolchain::rust_channel_manifest_url(channel);

    tracing::debug!(url = %url, "fetching channel manifest");

    let body = crate::http::get_text(&url)
        .await
        .map_err(|e| vx_toolchain::ToolchainError::FetchError {
            url: url.clone(),
            source: Box::new(e),
        })?;

    vx_toolchain::ChannelManifest::from_toml(&body)
}

/// Download a component with checksum verification
async fn download_component(url: &str, expected_hash: &str) -> Result<Vec<u8>, vx_toolchain::ToolchainError> {
    tracing::debug!(url = %url, "downloading component");

    let bytes = crate::http::get_bytes(url)
        .await
        .map_err(|e| vx_toolchain::ToolchainError::FetchError {
            url: url.to_string(),
            source: Box::new(e),
        })?;

    // Verify SHA256
    let actual_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    };

    if actual_hash != expected_hash {
        return Err(vx_toolchain::ToolchainError::ChecksumMismatch {
            url: url.to_string(),
            expected: expected_hash.to_string(),
            actual: actual_hash,
        });
    }

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        hash = %actual_hash,
        "component downloaded and verified"
    );

    Ok(bytes)
}

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureToolchainResult>>;

/// Manages in-flight toolchain acquisitions with deduplication.
///
/// Inflight entries are never removed. This is intentional:
/// - Memory cost is negligible (one OnceCell per unique ToolchainSpecKey)
/// - Avoids race between CAS lookup miss and inflight insert
/// - Once CAS has the mapping, lookup_fn fast-paths and OnceCell is never awaited
pub(crate) struct ToolchainManager {
    inflight: tokio::sync::Mutex<HashMap<ToolchainSpecKey, InflightFuture>>,
}

impl ToolchainManager {
    pub(crate) fn new() -> Self {
        Self {
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ensure a toolchain, deduplicating concurrent requests.
    ///
    /// `lookup_fn` is async because CAS is remote in production.
    pub(crate) async fn ensure(
        &self,
        spec_key: ToolchainSpecKey,
        lookup_fn: impl AsyncFn() -> Option<Blake3Hash>,
        acquire_fn: impl AsyncFn() -> EnsureToolchainResult,
    ) -> EnsureToolchainResult {
        // Fast path: check if already in CAS (async RPC)
        if let Some(manifest_hash) = lookup_fn().await {
            return EnsureToolchainResult {
                spec_key: Some(spec_key),
                toolchain_id: None, // Caller should read manifest for ID
                manifest_hash: Some(manifest_hash),
                status: ToolchainEnsureStatus::Hit,
                error: None,
            };
        }

        // Get or create inflight entry
        let cell = {
            let mut inflight = self.inflight.lock().await;
            inflight
                .entry(spec_key)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        // Initialize if we're first, otherwise wait
        #[allow(clippy::redundant_closure)] // it's not
        cell.get_or_init(|| acquire_fn()).await.clone()
    }
}

// =============================================================================
// Toolchain methods (Inherent methods, NOT part of Cas RPC trait)
// =============================================================================

impl CasService {
    /// Ensure a Rust toolchain exists in CAS (internal helper, not an RPC method).
    #[tracing::instrument(skip(self), fields(spec_key = tracing::field::Empty))]
    pub async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Validate and compute spec_key first
        let spec_key = match spec.spec_key() {
            Ok(k) => {
                tracing::Span::current().record("spec_key", k.short_hex());
                k
            }
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: None,
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };

        let this = self.clone();
        let this2 = self.clone();

        self.toolchain_manager
            .ensure(
                spec_key,
                async move || this.lookup_spec(&spec_key).await,
                async move || this2.acquire_rust_toolchain_impl(spec_key, &spec).await,
            )
            .await
    }

    async fn acquire_rust_toolchain_impl(
        &self,
        spec_key: ToolchainSpecKey,
        spec: &RustToolchainSpec,
    ) -> EnsureToolchainResult {
        // Convert proto spec to vx_toolchain types
        let channel = match &spec.channel {
            RustChannel::Stable => vx_toolchain::Channel::Stable,
            RustChannel::Beta => vx_toolchain::Channel::Beta,
            RustChannel::Nightly { date } => vx_toolchain::Channel::Nightly { date: date.clone() },
        };

        // Fetch channel manifest to get download URLs
        let manifest = match fetch_channel_manifest(&channel).await {
            Ok(m) => m,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to fetch channel manifest: {}", e)),
                };
            }
        };

        let rustc_target = match manifest.rustc_for_target(&spec.host) {
            Ok(t) => t,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("{}", e)),
                };
            }
        };

        let rust_std_target = match manifest.rust_std_for_target(&spec.target) {
            Ok(t) => t,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("{}", e)),
                };
            }
        };

        // Compute toolchain ID from manifest SHA256s
        let toolchain_id = vx_toolchain::RustToolchainId::from_manifest_sha256s(
            &spec.host,
            &spec.target,
            &rustc_target.hash,
            &rust_std_target.hash,
        );

        // Download both components in parallel (with semaphore limiting)
        let rustc_url = rustc_target.url.clone();
        let rustc_hash = rustc_target.hash.clone();
        let rust_std_url = rust_std_target.url.clone();
        let rust_std_hash = rust_std_target.hash.clone();
        let sem = self.download_semaphore.clone();

        let (rustc_result, rust_std_result) = tokio::join!(
            async {
                let _permit = sem.acquire().await.unwrap();
                download_component(&rustc_url, &rustc_hash).await
            },
            async {
                let _permit = sem.acquire().await.unwrap();
                download_component(&rust_std_url, &rust_std_hash).await
            }
        );

        let rustc_tarball = match rustc_result {
            Ok(bytes) => bytes,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to download rustc: {}", e)),
                };
            }
        };

        let rust_std_tarball = match rust_std_result {
            Ok(bytes) => bytes,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to download rust-std: {}", e)),
                };
            }
        };

        // Extract rustc to tree
        let this = self.clone();
        let rustc_tree = match vx_tarball::extract_to_tree(
            std::io::Cursor::new(rustc_tarball),
            Compression::Xz,
            1, // strip first component
            async move |data| {
                let hash = this.put_blob(data).await;
                Ok(hash)
            },
        )
        .await
        {
            Ok(tree) => tree,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to extract rustc: {}", e)),
                };
            }
        };

        // Store rustc tree manifest
        let rustc_tree_json = facet_json::to_string(&rustc_tree);
        let rustc_tree_hash = self.put_blob(rustc_tree_json.into_bytes()).await;

        tracing::info!(
            rustc_tree = %rustc_tree_hash.short_hex(),
            files = rustc_tree.entries.len(),
            unique_blobs = rustc_tree.unique_blobs,
            total_bytes = rustc_tree.total_size_bytes,
            "stored rustc tree"
        );

        // Extract rust-std to tree (already downloaded in parallel above)
        let this = self.clone();
        let rust_std_tree = match vx_tarball::extract_to_tree(
            std::io::Cursor::new(rust_std_tarball),
            Compression::Xz,
            1, // strip first component
            async move |data| {
                let hash = this.put_blob(data).await;
                Ok(hash)
            },
        )
        .await
        {
            Ok(tree) => tree,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to extract rust-std: {}", e)),
                };
            }
        };

        // Store rust-std tree manifest
        let rust_std_tree_json = facet_json::to_string(&rust_std_tree);
        let rust_std_tree_hash = self.put_blob(rust_std_tree_json.into_bytes()).await;

        tracing::info!(
            rust_std_tree = %rust_std_tree_hash.short_hex(),
            files = rust_std_tree.entries.len(),
            unique_blobs = rust_std_tree.unique_blobs,
            total_bytes = rust_std_tree.total_size_bytes,
            "stored rust-std tree"
        );

        // Build toolchain manifest
        let toolchain_manifest = ToolchainManifest {
            schema_version: TOOLCHAIN_MANIFEST_SCHEMA_VERSION,
            kind: ToolchainKind::Rust,
            spec_key,
            toolchain_id: toolchain_id.0,
            created_at: Timestamp::now().in_tz("UTC").unwrap().datetime(),
            rust_manifest_date: Some(manifest.date.clone()),
            rust_version: Some(manifest.rustc.version.clone()),
            zig_version: None,
            components: vec![
                ToolchainComponentTree {
                    name: "rustc".to_string(),
                    target: Some(spec.host.clone()),
                    tree_manifest: rustc_tree_hash,
                    sha256: rustc_target.hash.clone(),
                    total_size_bytes: rustc_tree.total_size_bytes,
                    file_count: rustc_tree.entries.len() as u32,
                    unique_blobs: rustc_tree.unique_blobs,
                },
                ToolchainComponentTree {
                    name: "rust-std".to_string(),
                    target: Some(spec.target.clone()),
                    tree_manifest: rust_std_tree_hash,
                    sha256: rust_std_target.hash.clone(),
                    total_size_bytes: rust_std_tree.total_size_bytes,
                    file_count: rust_std_tree.entries.len() as u32,
                    unique_blobs: rust_std_tree.unique_blobs,
                },
            ],
        };

        // Store manifest
        let manifest_hash = self.put_toolchain_manifest(&toolchain_manifest).await;

        // Publish spec → manifest_hash mapping
        let _ = self.publish_spec_mapping(&spec_key, &manifest_hash);

        tracing::info!(
            spec_key = %spec_key.short_hex(),
            toolchain_id = %toolchain_id.short_hex(),
            manifest_hash = %manifest_hash.short_hex(),
            "stored Rust toolchain in CAS"
        );

        EnsureToolchainResult {
            spec_key: Some(spec_key),
            toolchain_id: Some(toolchain_id.0),
            manifest_hash: Some(manifest_hash),
            status: ToolchainEnsureStatus::Downloaded,
            error: None,
        }
    }

    /// Ensure a Zig toolchain exists in CAS (internal helper, not an RPC method).
    pub async fn ensure_zig_toolchain(&self, _spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // TODO: Implement Zig toolchain acquisition
        EnsureToolchainResult {
            spec_key: None,
            toolchain_id: None,
            manifest_hash: None,
            status: ToolchainEnsureStatus::Failed,
            error: Some("Zig toolchain acquisition not yet implemented".to_string()),
        }
    }
}
