use jiff::Timestamp;
use vx_oort_proto::Oort;
use vx_oort_proto::{
    Blake3Hash, BlobHash, CacheKey, EnsureRegistryCrateResult, EnsureStatus, EnsureToolchainResult,
    IngestTreeRequest, IngestTreeResult, ManifestHash, MaterializationPlan, MaterializeStep,
    NodeManifest, OORT_PROTOCOL_VERSION, PublishResult, RegistryCrateManifest, RegistrySpec,
    RegistrySpecKey, RustToolchainSpec, ServiceVersion, TREE_MANIFEST_SCHEMA_VERSION,
    ToolchainKind, ToolchainManifest, ToolchainSpecKey, TreeManifest, ZigToolchainSpec,
};

use crate::registry::download_crate;
use crate::types::OortService;
use vx_io::atomic_write;

const CRATES_IO_REGISTRY: &str = "https://crates.io";
const REGISTRY_MANIFEST_SCHEMA_VERSION: u32 = 1;
const MATERIALIZATION_LAYOUT_VERSION: u32 = 1;

impl Oort for OortService {
    async fn version(&self) -> ServiceVersion {
        ServiceVersion {
            service: "vx-oort".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: OORT_PROTOCOL_VERSION,
        }
    }

    async fn lookup(&self, cache_key: CacheKey) -> Option<ManifestHash> {
        let path = self.cache_path(&cache_key);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        ManifestHash::from_hex(content.trim())
    }

    async fn publish(&self, cache_key: CacheKey, manifest_hash: ManifestHash) -> PublishResult {
        let manifest_path = self.manifest_path(&manifest_hash);
        let dest = self.cache_path(&cache_key);

        // Validate manifest exists
        if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
            return PublishResult {
                success: false,
                error: Some(format!(
                    "manifest {} does not exist",
                    manifest_hash.to_hex()
                )),
            };
        }

        // Atomically write the cache mapping
        match atomic_write(&dest, manifest_hash.to_hex().as_bytes()).await {
            Ok(()) => PublishResult {
                success: true,
                error: None,
            },
            Err(e) => PublishResult {
                success: false,
                error: Some(format!("failed to write cache mapping: {}", e)),
            },
        }
    }

    async fn put_manifest(&self, manifest: NodeManifest) -> ManifestHash {
        let json = facet_json::to_string(&manifest);
        let hash = ManifestHash::from_bytes(json.as_bytes());
        let dest = self.manifest_path(&hash);

        if let Err(e) = atomic_write(&dest, json.as_bytes()).await {
            tracing::warn!("failed to write node manifest to {}: {}", dest, e);
        }

        hash
    }

    async fn get_manifest(&self, hash: ManifestHash) -> Option<NodeManifest> {
        let path = self.manifest_path(&hash);
        let json = tokio::fs::read_to_string(&path).await.ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn put_blob(&self, data: Vec<u8>) -> BlobHash {
        let hash = BlobHash::from_bytes(&data);
        let dest = self.blob_path(&hash);

        if let Err(e) = atomic_write(&dest, &data).await {
            tracing::warn!("failed to write blob to {}: {}", dest, e);
        }

        hash
    }

    async fn get_blob(&self, hash: BlobHash) -> Option<Vec<u8>> {
        let path = self.blob_path(&hash);
        tokio::fs::read(&path).await.ok()
    }

    async fn has_blob(&self, hash: BlobHash) -> bool {
        let path = self.blob_path(&hash);
        tokio::fs::try_exists(&path).await.unwrap_or(false)
    }

    #[tracing::instrument(skip(self), fields(name = %spec.name, version = %spec.version))]
    async fn ensure_registry_crate(&self, spec: RegistrySpec) -> EnsureRegistryCrateResult {
        // Validate spec and compute spec_key
        let spec_key = match spec.spec_key() {
            Ok(k) => k,
            Err(e) => {
                return EnsureRegistryCrateResult {
                    spec_key: None,
                    manifest_hash: None,
                    status: EnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };

        // Validate registry is crates.io
        if spec.registry_url != CRATES_IO_REGISTRY {
            return EnsureRegistryCrateResult {
                spec_key: Some(spec_key),
                manifest_hash: None,
                status: EnsureStatus::Failed,
                error: Some(format!(
                    "only crates.io is supported, got: {}",
                    spec.registry_url
                )),
            };
        }

        let this = self.clone();
        let this2 = self.clone();

        self.registry_manager
            .ensure(
                spec_key,
                async move || this.lookup_registry_spec_local(&spec_key).await,
                async move || {
                    // Download tarball
                    let tarball_bytes =
                        match download_crate(&spec.name, &spec.version, &spec.checksum).await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                return EnsureRegistryCrateResult {
                                    spec_key: Some(spec_key),
                                    manifest_hash: None,
                                    status: EnsureStatus::Failed,
                                    error: Some(e.to_string()),
                                };
                            }
                        };

                    // Store tarball as blob
                    let tarball_blob = this2.put_blob(tarball_bytes).await;

                    // Create manifest
                    let manifest = RegistryCrateManifest {
                        schema_version: REGISTRY_MANIFEST_SCHEMA_VERSION,
                        spec: spec.clone(),
                        crate_tarball_blob: tarball_blob,
                        created_at: Timestamp::now().in_tz("UTC").unwrap().datetime(),
                    };

                    // Store manifest
                    let manifest_hash = this2.put_registry_manifest(&manifest).await;

                    // Publish spec → manifest_hash mapping
                    if let Err(e) = this2
                        .publish_registry_spec_mapping(&spec_key, &manifest_hash)
                        .await
                    {
                        tracing::warn!(
                            "failed to publish registry spec mapping for {}: {}",
                            spec_key.short_hex(),
                            e
                        );
                    }

                    tracing::info!(
                        name = %spec.name,
                        version = %spec.version,
                        spec_key = %spec_key.short_hex(),
                        manifest_hash = %manifest_hash.short_hex(),
                        "stored registry crate in CAS"
                    );

                    EnsureRegistryCrateResult {
                        spec_key: Some(spec_key),
                        manifest_hash: Some(manifest_hash),
                        status: EnsureStatus::Downloaded,
                        error: None,
                    }
                },
            )
            .await
    }

    async fn get_registry_manifest(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<RegistryCrateManifest> {
        self.get_registry_crate_manifest(&manifest_hash).await
    }

    async fn lookup_registry_spec(&self, spec_key: RegistrySpecKey) -> Option<Blake3Hash> {
        self.lookup_registry_spec_local(&spec_key).await
    }

    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Delegate to the inherent method which handles deduplication via ToolchainManager
        OortService::ensure_rust_toolchain(self, spec).await
    }

    async fn ensure_zig_toolchain(&self, spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // Delegate to the inherent method
        OortService::ensure_zig_toolchain(self, spec).await
    }

    async fn lookup_toolchain_spec(&self, spec_key: ToolchainSpecKey) -> Option<Blake3Hash> {
        // Delegate to the inherent method
        self.lookup_spec(&spec_key).await
    }

    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest> {
        let path = self.manifest_path(&manifest_hash);
        let json = tokio::fs::read_to_string(&path).await.ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn get_materialization_plan(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Option<MaterializationPlan> {
        let manifest = self.get_toolchain_manifest(manifest_hash).await?;

        // Pure function: manifest → plan (no heuristics, bit-for-bit stable)
        let steps = match manifest.kind {
            ToolchainKind::Rust => {
                let mut steps = vec![MaterializeStep::EnsureDir {
                    relpath: "sysroot".to_string(),
                }];
                // Materialize each component tree into sysroot
                steps.extend(manifest.components.iter().map(|c| {
                    MaterializeStep::MaterializeTree {
                        tree_manifest: c.tree_manifest,
                        dest_subdir: "sysroot".to_string(),
                    }
                }));
                steps
            }
            ToolchainKind::Zig => {
                let mut steps = Vec::new();
                for c in &manifest.components {
                    match c.name.as_str() {
                        "zig-exe" | "zig-lib" => {
                            steps.push(MaterializeStep::MaterializeTree {
                                tree_manifest: c.tree_manifest,
                                dest_subdir: ".".to_string(),
                            });
                        }
                        _ => {} // Unknown component, skip
                    }
                }
                steps
            }
        };

        Some(MaterializationPlan {
            toolchain_id: manifest.toolchain_id,
            layout_version: MATERIALIZATION_LAYOUT_VERSION,
            steps,
        })
    }

    #[tracing::instrument(skip(self, request), fields(file_count = request.files.len()))]
    async fn ingest_tree(&self, request: IngestTreeRequest) -> IngestTreeResult {
        let mut manifest = TreeManifest {
            schema_version: TREE_MANIFEST_SCHEMA_VERSION,
            entries: Vec::with_capacity(request.files.len()),
            total_size_bytes: 0,
            unique_blobs: 0,
        };

        let mut new_blobs = 0u32;
        let mut total_bytes = 0u64;

        for file in &request.files {
            let size = file.contents.len() as u64;
            total_bytes += size;

            // Store the blob
            let blob_hash = BlobHash::from_bytes(&file.contents);
            let blob_path = self.blob_path(&blob_hash);

            // Check if blob already exists (for dedup stats)
            let exists = tokio::fs::try_exists(&blob_path).await.unwrap_or(false);
            if !exists {
                if let Err(e) = vx_io::atomic_write(&blob_path, &file.contents).await {
                    return IngestTreeResult {
                        success: false,
                        manifest_hash: None,
                        file_count: 0,
                        total_bytes: 0,
                        new_blobs: 0,
                        error: Some(format!("failed to write blob for {}: {}", file.path, e)),
                    };
                }
                new_blobs += 1;
            }

            // Add to manifest
            manifest.add_file(file.path.clone(), blob_hash, size, file.executable);
        }

        // Finalize manifest (sorts entries, computes unique blob count)
        manifest.finalize();

        // Serialize manifest to JSON
        let manifest_json = facet_json::to_string(&manifest);
        let manifest_hash = ManifestHash::from_bytes(manifest_json.as_bytes());

        // Store as blob (for rhea's materialize_tree_from_cas which calls get_blob)
        let blob_path = self.blob_path(&manifest_hash);
        if let Err(e) = vx_io::atomic_write(&blob_path, manifest_json.as_bytes()).await {
            return IngestTreeResult {
                success: false,
                manifest_hash: None,
                file_count: 0,
                total_bytes: 0,
                new_blobs: 0,
                error: Some(format!("failed to write tree manifest blob: {}", e)),
            };
        }

        // Also store in tree_manifests (for get_tree_manifest API)
        let manifest_path = self.tree_manifest_path(&manifest_hash);
        if let Err(e) = vx_io::atomic_write(&manifest_path, manifest_json.as_bytes()).await {
            return IngestTreeResult {
                success: false,
                manifest_hash: None,
                file_count: 0,
                total_bytes: 0,
                new_blobs: 0,
                error: Some(format!("failed to write tree manifest: {}", e)),
            };
        }

        tracing::info!(
            manifest_hash = %manifest_hash.short_hex(),
            file_count = request.files.len(),
            total_bytes,
            new_blobs,
            unique_blobs = manifest.unique_blobs,
            "ingested tree"
        );

        IngestTreeResult {
            success: true,
            manifest_hash: Some(manifest_hash),
            file_count: request.files.len() as u32,
            total_bytes,
            new_blobs,
            error: None,
        }
    }

    async fn get_tree_manifest(&self, hash: ManifestHash) -> Option<TreeManifest> {
        let path = self.tree_manifest_path(&hash);
        let json = tokio::fs::read_to_string(&path).await.ok()?;
        facet_json::from_str(&json).ok()
    }

    async fn stream_blob(&self, blob: Blake3Hash) -> rapace::Streaming<Vec<u8>> {
        use tokio::sync::mpsc;
        use tokio_stream::wrappers::ReceiverStream;

        let (tx, rx) = mpsc::channel(16);
        let blob_path = self.blob_path(&blob);

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;

            let Ok(file) = tokio::fs::File::open(&blob_path).await else {
                let _ = tx
                    .send(Err(rapace::RpcError::Status {
                        code: rapace::ErrorCode::NotFound,
                        message: "blob not found".to_string(),
                    }))
                    .await;
                return;
            };

            let mut reader = tokio::io::BufReader::new(file);
            let mut buf = vec![0u8; 16 * 1024 * 1024]; // 16MB chunks

            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(rapace::RpcError::Status {
                                code: rapace::ErrorCode::Internal,
                                message: format!("read error: {}", e),
                            }))
                            .await;
                        break;
                    }
                }
            }
        });

        Box::pin(ReceiverStream::new(rx))
    }
}
