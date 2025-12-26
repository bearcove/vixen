//! Toolchain Manager (Inflight Deduplication)

use futures_util::StreamExt;
use jiff::Timestamp;
use std::collections::HashMap;
use std::sync::Arc;
use vx_cass_proto::*;
use vx_cass_proto::{
    EnsureStatus as ToolchainEnsureStatus, EnsureToolchainResult, RustChannel, RustToolchainSpec,
    TOOLCHAIN_MANIFEST_SCHEMA_VERSION, ToolchainComponentTree, ToolchainKind, ToolchainManifest,
    ToolchainSpecKey, ZigToolchainSpec,
};
use vx_tarball::Compression;

use crate::types::CassService;

// =============================================================================
// HTTP Download Functions (Rust Toolchains)
// =============================================================================

/// Fetch a channel manifest from static.rust-lang.org
async fn fetch_channel_manifest(
    channel: &vx_toolchain::Channel,
) -> Result<vx_toolchain::ChannelManifest, vx_toolchain::ToolchainError> {
    let url = vx_toolchain::rust_channel_manifest_url(channel);

    tracing::debug!(url = %url, "fetching channel manifest");

    let body = crate::http::get_text(&url).await.map_err(|e| {
        vx_toolchain::ToolchainError::FetchError {
            url: url.clone(),
            source: Box::new(e),
        }
    })?;

    vx_toolchain::ChannelManifest::from_toml(&body)
}

/// Download a component with streaming checksum verification
async fn download_component(
    url: &str,
    expected_hash: &str,
) -> Result<Vec<u8>, vx_toolchain::ToolchainError> {
    use http_body_util::BodyExt;
    use tokio::io::AsyncReadExt;

    tracing::debug!(url = %url, "downloading component");

    let response =
        crate::http::get(url)
            .await
            .map_err(|e| vx_toolchain::ToolchainError::FetchError {
                url: url.to_string(),
                source: Box::new(e),
            })?;

    let status = response.status();
    if !status.is_success() {
        return Err(vx_toolchain::ToolchainError::FetchError {
            url: url.to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HTTP {}", status),
            )),
        });
    }

    // Wrap the response body in a hash-verifying reader
    let body_reader = tokio_util::io::StreamReader::new(
        response
            .into_body()
            .into_data_stream()
            .map(|result| result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
    );

    let mut verifying_reader =
        crate::hash_reader::Sha256VerifyingReader::new(body_reader, expected_hash.to_string());

    // Read through the verifying reader (hash verification happens on EOF)
    let mut bytes = Vec::new();
    verifying_reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| vx_toolchain::ToolchainError::ChecksumMismatch {
            url: url.to_string(),
            expected: expected_hash.to_string(),
            actual: e.to_string(),
        })?;

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        hash = %expected_hash,
        "component downloaded and verified"
    );

    Ok(bytes)
}

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureToolchainResult>>;

/// Manages in-flight toolchain acquisitions with deduplication.
///
/// Inflight entries are never removed. This is intentional:
/// - Memory cost is negligible (one OnceCell per unique ToolchainSpecKey)
/// - Avoids race between cass lookup miss and inflight insert
/// - Once cass has the mapping, lookup_fn fast-paths and OnceCell is never awaited
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
    /// `lookup_fn` is async because cass is remote in production.
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
// Toolchain methods (Inherent methods, NOT part of CAS RPC trait)
// =============================================================================

impl CassService {
    /// Ensure a Rust toolchain exists in CAS (internal helper, not an RPC method).
    #[tracing::instrument(skip(self), fields(spec_key = tracing::field::Empty))]
    pub async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Validate and compute spec_key first
        let spec_key = match spec.spec_key() {
            Ok(k) => {
                tracing::Span::current().record("spec_key", k.to_string());
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
            2, // strip rustc-VERSION/ and rustc/ components
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
            rustc_tree = %rustc_tree_hash,
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
            2, // strip rust-std-VERSION/ and rust-std-TARGET/ components
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
            rust_std_tree = %rust_std_tree_hash,
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
            created_at: Timestamp::now().in_tz("UTC").unwrap().to_string(),
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
        if let Err(e) = self.publish_spec_mapping(&spec_key, &manifest_hash).await {
            tracing::warn!(
                "failed to publish spec mapping for {}: {}",
                spec_key,
                e
            );
        }

        tracing::info!(
            spec_key = %spec_key,
            toolchain_id = %toolchain_id,
            manifest_hash = %manifest_hash,
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
    #[tracing::instrument(skip(self), fields(spec_key = tracing::field::Empty))]
    pub async fn ensure_zig_toolchain(&self, spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // Compute spec_key
        let spec_key = match spec.spec_key() {
            Ok(k) => {
                tracing::Span::current().record("spec_key", k.to_string());
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
                async move || this2.acquire_zig_toolchain_impl(spec_key, &spec).await,
            )
            .await
    }

    async fn acquire_zig_toolchain_impl(
        &self,
        spec_key: ToolchainSpecKey,
        spec: &ZigToolchainSpec,
    ) -> EnsureToolchainResult {
        // Convert spec to vx_toolchain types
        let version = vx_toolchain::zig::ZigVersion::new(&spec.version);
        let platform = vx_toolchain::zig::HostPlatform {
            os: if spec.host_platform.contains("macos") {
                "macos"
            } else if spec.host_platform.contains("linux") {
                "linux"
            } else if spec.host_platform.contains("windows") {
                "windows"
            } else {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("unsupported platform: {}", spec.host_platform)),
                };
            }
            .to_string(),
            arch: if spec.host_platform.contains("x86_64") {
                "x86_64"
            } else if spec.host_platform.contains("aarch64") {
                "aarch64"
            } else {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("unsupported platform: {}", spec.host_platform)),
                };
            }
            .to_string(),
        };

        // Download and extract Zig toolchain
        let acquired = match vx_toolchain::zig::acquire_zig_toolchain(version.clone(), platform.clone()).await {
            Ok(a) => a,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: Some(spec_key),
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("failed to acquire Zig toolchain: {}", e)),
                };
            }
        };

        // Store zig executable as tree manifest (single-file tree)
        let zig_exe_blob_hash = self.put_blob(acquired.zig_exe_contents.clone()).await;
        let mut zig_exe_tree = vx_cass_proto::TreeManifest::new();
        zig_exe_tree.add_file(
            "zig".to_string(),
            zig_exe_blob_hash,
            acquired.zig_exe_contents.len() as u64,
            true, // executable
        );
        zig_exe_tree.finalize();
        let zig_exe_tree_json = facet_json::to_string(&zig_exe_tree);
        let zig_exe_hash = self.put_blob(zig_exe_tree_json.into_bytes()).await;

        // Extract lib tarball to tree manually
        // First collect all entries synchronously (tar::Archive is not Send)
        // Note: Paths in the tarball are relative (e.g. "std/std.zig"), but Zig expects them under "lib/"
        let mut lib_files: Vec<(String, Vec<u8>)> = Vec::new();
        {
            let mut archive = tar::Archive::new(std::io::Cursor::new(&acquired.lib_tarball));

            for entry_result in archive.entries().map_err(|e| format!("tar entries: {}", e)).unwrap() {
                let mut entry = entry_result.map_err(|e| format!("tar entry: {}", e)).unwrap();
                let path = entry.path().map_err(|e| format!("entry path: {}", e)).unwrap();
                let path_str = path.to_string_lossy().to_string();

                // Prepend "lib/" to all paths since Zig expects lib directory
                let full_path = format!("lib/{}", path_str);

                // Read entry data
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data)
                    .map_err(|e| format!("read entry: {}", e))
                    .unwrap();

                if !data.is_empty() {
                    lib_files.push((full_path, data));
                }
            }
        } // archive dropped here

        // Now upload blobs asynchronously and build tree
        let mut lib_tree = vx_cass_proto::TreeManifest::new();
        for (path_str, data) in lib_files {
            let blob_hash = self.put_blob(data.clone()).await;
            lib_tree.add_file(
                path_str,
                blob_hash,
                data.len() as u64,
                false, // lib files are not executable
            );
        }

        lib_tree.finalize();

        // Store lib tree manifest
        let lib_tree_json = facet_json::to_string(&lib_tree);
        let lib_hash = self.put_blob(lib_tree_json.into_bytes()).await;

        // Cleanup extraction directory
        if let Err(e) = acquired.cleanup() {
            tracing::warn!("failed to cleanup Zig extraction directory: {}", e);
        }

        let toolchain_id = acquired.id.0;

        tracing::info!(
            zig_exe = %zig_exe_hash,
            lib = %lib_hash,
            toolchain_id = %toolchain_id,
            "stored Zig toolchain blobs in CAS"
        );

        // Create toolchain manifest
        let toolchain_manifest = ToolchainManifest {
            schema_version: TOOLCHAIN_MANIFEST_SCHEMA_VERSION,
            kind: ToolchainKind::Zig,
            spec_key,
            toolchain_id,
            created_at: Timestamp::now().in_tz("UTC").unwrap().to_string(),
            rust_manifest_date: None,
            rust_version: None,
            zig_version: Some(version.version.clone()),
            components: vec![
                ToolchainComponentTree {
                    name: "zig-exe".to_string(),
                    target: Some(platform.to_string()),
                    tree_manifest: zig_exe_hash,
                    sha256: String::new(), // Zig doesn't provide checksums
                    total_size_bytes: acquired.zig_exe_contents.len() as u64,
                    file_count: 1,
                    unique_blobs: 1,
                },
                ToolchainComponentTree {
                    name: "zig-lib".to_string(),
                    target: Some(platform.to_string()),
                    tree_manifest: lib_hash,
                    sha256: String::new(),
                    total_size_bytes: acquired.lib_tarball.len() as u64,
                    file_count: 1, // It's a tarball, many files inside
                    unique_blobs: 1,
                },
            ],
        };

        // Store manifest
        let manifest_hash = self.put_toolchain_manifest(&toolchain_manifest).await;

        // Publish spec → manifest_hash mapping
        if let Err(e) = self.publish_spec_mapping(&spec_key, &manifest_hash).await {
            tracing::warn!(
                "failed to publish spec mapping for {}: {}",
                spec_key,
                e
            );
        }

        tracing::info!(
            spec_key = %spec_key,
            toolchain_id = %toolchain_id,
            manifest_hash = %manifest_hash,
            "stored Zig toolchain in CAS"
        );

        EnsureToolchainResult {
            spec_key: Some(spec_key),
            toolchain_id: Some(toolchain_id),
            manifest_hash: Some(manifest_hash),
            status: ToolchainEnsureStatus::Downloaded,
            error: None,
        }
    }
}
