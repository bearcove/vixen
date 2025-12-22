use jiff::Timestamp;
use vx_cas_proto::Cas;
use vx_cas_proto::{
    Blake3Hash, BlobHash, CacheKey, EnsureRegistryCrateResult, EnsureStatus, EnsureToolchainResult,
    ManifestHash, MaterializationPlan, MaterializeStep, NodeManifest, PublishResult,
    RegistryCrateManifest, RegistrySpec, RegistrySpecKey, RustToolchainSpec, ToolchainKind,
    ToolchainManifest, ToolchainSpecKey, ZigToolchainSpec,
};

use crate::registry::download_crate;
use crate::types::CasService;
use vx_io::atomic_write;

const CRATES_IO_REGISTRY: &str = "https://crates.io";
const REGISTRY_MANIFEST_SCHEMA_VERSION: u32 = 1;
const MATERIALIZATION_LAYOUT_VERSION: u32 = 1;

impl Cas for CasService {
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

        let _ = atomic_write(&dest, json.as_bytes()).await;

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

        let _ = atomic_write(&dest, &data).await;

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
                async move || this.lookup_registry_spec_local(&spec_key),
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
                                    error: Some(e),
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
                    let _ = this2.publish_registry_spec_mapping(&spec_key, &manifest_hash);

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
        self.get_registry_crate_manifest(&manifest_hash)
    }

    async fn lookup_registry_spec(&self, spec_key: RegistrySpecKey) -> Option<Blake3Hash> {
        self.lookup_registry_spec_local(&spec_key)
    }

    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Delegate to the inherent method which handles deduplication via ToolchainManager
        CasService::ensure_rust_toolchain(self, spec).await
    }

    async fn ensure_zig_toolchain(&self, spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // Delegate to the inherent method
        CasService::ensure_zig_toolchain(self, spec).await
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
                steps.extend(manifest.components.iter().map(|c| {
                    MaterializeStep::ExtractTarXz {
                        blob: c.blob,
                        strip_components: 1, // Validated during acquisition
                        dest_subdir: "sysroot".to_string(),
                    }
                }));
                steps
            }
            ToolchainKind::Zig => {
                let mut steps = Vec::new();
                for c in &manifest.components {
                    match c.name.as_str() {
                        "zig-exe" => {
                            steps.push(MaterializeStep::WriteFile {
                                relpath: "zig".to_string(),
                                blob: c.blob,
                                mode: 0o755,
                            });
                        }
                        "zig-lib" => {
                            steps.push(MaterializeStep::EnsureDir {
                                relpath: "lib".to_string(),
                            });
                            steps.push(MaterializeStep::ExtractTarXz {
                                blob: c.blob,
                                strip_components: 0,
                                dest_subdir: "lib".to_string(),
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
