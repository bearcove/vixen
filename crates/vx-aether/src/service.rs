//! AetherService implementation.
//!
//! Orchestrates builds by:
//! 1. Computing cache keys via picante
//! 2. Checking CAS for cache hits
//! 3. Sending compile requests to execd for cache misses
//! 4. Materializing outputs locally

use crate::SpawnTracker;
use crate::db::Database;
use crate::error::{AetherError, Result};
use crate::inputs::*;
use crate::queries::*;
use crate::tui::{ActionType, TuiHandle};
use camino::Utf8PathBuf;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use vx_aether_proto::{Aether, AETHER_PROTOCOL_VERSION, BuildRequest, BuildResult};
use vx_oort_proto::{
    Blake3Hash, EnsureStatus, IngestTreeRequest, OortClient, RegistrySpec, RustChannel,
    RustComponent, RustToolchainSpec, ServiceVersion, TreeFile,
};
use vx_rhea_proto::{RheaClient, RustCompileRequest, RustDep};
use vx_rs::crate_graph::CrateSource;
use vx_rs::{CrateGraph, CrateId, CrateType};

/// Toolchain information (manifest-only, no materialization)
pub struct ToolchainInfo {
    /// Hash of the ToolchainManifest in CAS
    pub manifest_hash: Blake3Hash,
    /// Content-derived toolchain ID (from manifest)
    pub toolchain_id: Blake3Hash,
    /// Rustc version string (for reports)
    pub version: Option<String>,
    /// Manifest date (for reports)
    pub manifest_date: Option<String>,
}

/// Acquired toolchains (manifest references only)
pub struct AcquiredToolchains {
    /// Rust toolchain (if acquired)
    pub rust: Option<ToolchainInfo>,

    /// Zig toolchain (if acquired)
    pub zig: Option<ToolchainInfo>,
}

/// The daemon service implementation
#[derive(Clone)]
pub struct AetherService {
    /// CAS client for content-addressed storage
    cas: Arc<OortClient>,

    /// Exec client for compilation
    exec: Arc<RheaClient>,

    /// The host triple of the execd machine (used for toolchain selection + cache keys).
    exec_host_triple: String,

    /// The picante incremental database
    db: Arc<Mutex<Database>>,

    /// Path to the picante cache file
    picante_cache: Utf8PathBuf,

    /// Acquired toolchains (manifest references only)
    toolchains: Arc<Mutex<AcquiredToolchains>>,

    /// Spawn tracker for child services
    spawn_tracker: Arc<Mutex<SpawnTracker>>,

    /// TUI for progress tracking
    tui: TuiHandle,
}

impl AetherService {
    /// Create a new daemon service
    pub async fn new(
        cas: Arc<OortClient>,
        exec: Arc<RheaClient>,
        vx_home: Utf8PathBuf,
        exec_host_triple: String,
        spawn_tracker: Arc<Mutex<SpawnTracker>>,
    ) -> Self {
        let db = Database::new();
        let cache_path = vx_home.join("picante.cache");

        // Load persisted picante cache
        match db.load_from_cache(&cache_path).await {
            Ok(true) => {
                info!(path = %cache_path, "loaded picante cache");
            }
            Ok(false) => {
                debug!(path = %cache_path, "no picante cache found, starting fresh");
            }
            Err(e) => {
                warn!(path = %cache_path, error = %e, "failed to load picante cache, starting fresh");
            }
        }

        Self {
            cas,
            exec,
            exec_host_triple,
            db: Arc::new(Mutex::new(db)),
            picante_cache: cache_path,
            toolchains: Arc::new(Mutex::new(AcquiredToolchains {
                rust: None,
                zig: None,
            })),
            spawn_tracker,
            tui: TuiHandle::new(),
        }
    }

    /// Ensure Rust toolchain exists in CAS. Returns info for cache keys.
    pub async fn ensure_rust_toolchain(&self, channel: RustChannel) -> Result<ToolchainInfo> {
        // Check if already acquired
        {
            let toolchains = self.toolchains.lock().await;
            if let Some(ref rust) = toolchains.rust {
                return Ok(ToolchainInfo {
                    manifest_hash: rust.manifest_hash,
                    toolchain_id: rust.toolchain_id,
                    version: rust.version.clone(),
                    manifest_date: rust.manifest_date.clone(),
                });
            }
        }

        let spec = RustToolchainSpec {
            channel,
            host: self.exec_host_triple.clone(),
            target: self.exec_host_triple.clone(),
            components: vec![RustComponent::Rustc, RustComponent::RustStd],
        };

        let result = self
            .cas
            .ensure_rust_toolchain(spec)
            .await
            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

        if result.status == EnsureStatus::Failed {
            return Err(AetherError::ToolchainAcquisition(
                result.error.unwrap_or_else(|| "unknown error".to_string()),
            ));
        }

        let manifest_hash = result.manifest_hash.ok_or(AetherError::NoManifestHash)?;

        // Get manifest for version info (and toolchain_id on cache hit)
        let manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
            .ok_or(AetherError::ToolchainManifestNotFound(manifest_hash))?;

        // On cache hit, toolchain_id comes from manifest; on download, it's in the result
        let toolchain_id = result.toolchain_id.unwrap_or(manifest.toolchain_id);

        let info = ToolchainInfo {
            manifest_hash,
            toolchain_id,
            version: manifest.rust_version.clone(),
            manifest_date: manifest.rust_manifest_date.clone(),
        };

        // Cache it
        {
            let mut toolchains = self.toolchains.lock().await;
            toolchains.rust = Some(ToolchainInfo {
                manifest_hash,
                toolchain_id,
                version: manifest.rust_version,
                manifest_date: manifest.rust_manifest_date,
            });
        }

        Ok(info)
    }

    /// Kill all spawned child services (casd, execd)
    pub async fn kill_spawned_services(&self) {
        self.spawn_tracker.lock().await.kill_all();
    }

    /// Ingest source files into CAS and return the tree manifest hash.
    async fn ingest_source_tree(
        &self,
        paths: &[Utf8PathBuf],
        workspace_root: &camino::Utf8Path,
    ) -> Result<Blake3Hash> {
        let mut files = Vec::with_capacity(paths.len());

        for rel_path in paths {
            let abs_path = workspace_root.join(rel_path);
            let contents = tokio::fs::read(&abs_path)
                .await
                .map_err(|e| AetherError::FileRead {
                    path: abs_path.clone(),
                    message: e.to_string(),
                })?;

            files.push(TreeFile {
                path: rel_path.to_string(),
                contents,
                executable: false, // Source files are not executable
            });
        }

        let request = IngestTreeRequest { files };
        let result = self
            .cas
            .ingest_tree(request)
            .await
            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

        if !result.success {
            return Err(AetherError::TreeIngestion(
                result
                    .error
                    .unwrap_or_else(|| "tree ingestion failed".to_string()),
            ));
        }

        let manifest_hash = result.manifest_hash.ok_or(AetherError::NoManifestHash)?;

        debug!(
            manifest_hash = %manifest_hash.short_hex(),
            file_count = result.file_count,
            total_bytes = result.total_bytes,
            new_blobs = result.new_blobs,
            "ingested source tree"
        );

        Ok(manifest_hash)
    }

    /// Acquire a single registry crate
    async fn acquire_single_registry_crate(
        cas: Arc<OortClient>,
        name: String,
        version: String,
        checksum: String,
    ) -> Result<((String, String), Blake3Hash)> {
        let spec = RegistrySpec {
            registry_url: "https://crates.io".to_string(),
            name: name.clone(),
            version: version.clone(),
            checksum,
        };

        debug!(name = %name, version = %version, "ensuring registry crate");

        let result = cas
            .ensure_registry_crate(spec)
            .await
            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

        match result.status {
            EnsureStatus::Hit | EnsureStatus::Downloaded => {
                let manifest_hash =
                    result
                        .manifest_hash
                        .ok_or_else(|| AetherError::RegistryCrateNoManifest {
                            name: name.clone(),
                            version: version.clone(),
                        })?;

                debug!(
                    name = %name,
                    version = %version,
                    manifest_hash = %manifest_hash.short_hex(),
                    "acquired registry crate"
                );

                Ok(((name, version), manifest_hash))
            }
            EnsureStatus::Failed => Err(AetherError::RegistryCrateAcquisition {
                name,
                version,
                reason: result
                    .error
                    .unwrap_or_else(|| "acquisition failed".to_string()),
            }),
        }
    }

    /// Acquire registry crates in parallel and return mapping of (name, version) -> manifest_hash
    async fn acquire_registry_crates(
        &self,
        graph: &CrateGraph,
    ) -> Result<HashMap<(String, String), Blake3Hash>> {
        if !graph.has_registry_deps() {
            return Ok(HashMap::new());
        }

        info!(
            registry_crate_count = graph.iter_registry_crates().count(),
            "acquiring registry crates"
        );

        let futures: Vec<_> = graph
            .iter_registry_crates()
            .map(|registry_crate| {
                Self::acquire_single_registry_crate(
                    self.cas.clone(),
                    registry_crate.name.clone(),
                    registry_crate.version.clone(),
                    registry_crate.checksum.clone(),
                )
            })
            .collect();

        let results = futures_util::future::try_join_all(futures).await?;

        info!(
            acquired_count = results.len(),
            "all registry crates acquired"
        );

        Ok(results.into_iter().collect())
    }

    /// Build a project.
    pub async fn do_build(&self, request: BuildRequest) -> Result<BuildResult> {
        let project_path = &request.project_path;
        let total_start = std::time::Instant::now();

        // Start toolchain acquisition in background - doesn't block manifest parsing
        let toolchain_fut = self.ensure_rust_toolchain(RustChannel::Stable);

        let target_triple = self.exec_host_triple.clone();

        // Build the crate graph with lockfile support (can happen while toolchain downloads)
        let graph = CrateGraph::build_with_lockfile(project_path)
            .map_err(|e| AetherError::CrateGraph(format!("{:?}", miette::Report::new(e))))?;

        // Set total number of actions for the TUI
        // Total = all crates in the graph
        self.tui.set_total(graph.nodes.len()).await;

        // Acquire registry crates in parallel (can happen while toolchain downloads)
        // This spawns all crate downloads concurrently
        let registry_fut = self.acquire_registry_crates(&graph);

        // Now wait for both toolchain and registry crates
        let (rust_toolchain, registry_manifests) = tokio::try_join!(toolchain_fut, registry_fut)?;

        info!(
            workspace_root = %graph.workspace_root,
            path_crate_count = graph.nodes.len(),
            registry_crate_count = graph.iter_registry_crates().count(),
            toolchain_id = %rust_toolchain.toolchain_id.short_hex(),
            "resolved crate graph, toolchain and crates ready"
        );

        let profile = if request.release { "release" } else { "debug" };

        // Set up picante inputs
        let db = self.db.lock().await;

        RustToolchain::set(
            &*db,
            rust_toolchain.toolchain_id,
            rust_toolchain.manifest_hash,
            target_triple.clone(),
            target_triple.clone(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        RustToolchainManifest::set(&*db, rust_toolchain.manifest_hash)
            .map_err(|e| AetherError::Picante(e.to_string()))?;

        BuildConfig::set(
            &*db,
            profile.to_string(),
            target_triple.clone(),
            graph.workspace_root.to_string(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        // Track compiled outputs for dependencies
        // Maps CrateId -> manifest_hash of the compiled output
        let mut compiled_outputs: HashMap<CrateId, Blake3Hash> = HashMap::new();

        let mut any_rebuilt = false;
        let mut final_output_path: Option<Utf8PathBuf> = None;

        // Process crates in topological order
        for crate_node in graph.iter_topo() {
            // Start tracking this crate compilation
            let action_id = self
                .tui
                .start_action(ActionType::CompileRust(crate_node.crate_name.clone()))
                .await;

            debug!(
                crate_name = %crate_node.crate_name,
                crate_type = ?crate_node.crate_type,
                "processing crate"
            );

            // Compute source closure (always needed for path crates)
            let crate_root_abs = graph.workspace_root.join(&crate_node.crate_root_rel);
            let closure_paths = vx_rs::rust_source_closure(&crate_root_abs, &graph.workspace_root)
                .map_err(|e| AetherError::SourceClosure {
                    crate_name: crate_node.crate_name.clone(),
                    message: e.to_string(),
                })?;

            // Compute closure hash
            // NOTE: With persistence (#18), InputIngredient::set() will automatically
            // detect if the hash is unchanged and avoid bumping the revision, which
            // allows picante to memoize cache key queries for unchanged crates.
            // Future optimization (#16): with file watching, we could skip this
            // computation when we know source files haven't been modified.
            let closure_hash = vx_rs::hash_source_closure(&closure_paths, &graph.workspace_root)
                .map_err(|e| AetherError::SourceHash(e.to_string()))?;

            // Create or reuse RustCrate input
            // Thanks to persistence and InputIngredient::set()'s smart value comparison,
            // this will automatically reuse the existing input if the closure_hash and
            // metadata are unchanged, without bumping the revision.
            let crate_id_hex = crate_node.id.short_hex();
            let rust_crate = RustCrate::new(
                &*db,
                crate_id_hex.clone(),
                crate_node.crate_name.clone(),
                crate_node.edition.as_str().to_string(),
                crate_node.crate_type.as_str().to_string(),
                crate_node.crate_root_rel.to_string(),
                closure_hash,
            )
            .map_err(|e| AetherError::Picante(e.to_string()))?;

            // Collect dependency manifest hashes
            let deps: Vec<RustDep> = crate_node
                .deps
                .iter()
                .map(|dep| {
                    let manifest_hash = compiled_outputs.get(&dep.crate_id).ok_or_else(|| {
                        AetherError::DependencyNotCompiled {
                            dep_name: dep.extern_name.clone(),
                            crate_name: crate_node.crate_name.clone(),
                        }
                    })?;

                    // Look up dependency node to check if it's a registry crate
                    let dep_node = graph.nodes.get(&dep.crate_id).ok_or_else(|| {
                        AetherError::DependencyNodeNotFound {
                            dep_name: dep.extern_name.clone(),
                        }
                    })?;

                    let registry_crate_manifest = match &dep_node.source {
                        CrateSource::Registry { name, version, .. } => {
                            registry_manifests
                                .get(&(name.clone(), version.clone()))
                                .copied()
                        }
                        CrateSource::Path { .. } => None,
                    };

                    Ok(RustDep {
                        extern_name: dep.extern_name.clone(),
                        manifest_hash: *manifest_hash,
                        registry_crate_manifest,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            // Compute cache key
            let dep_rlib_hashes: Vec<(String, Blake3Hash)> = deps
                .iter()
                .map(|d| (d.extern_name.clone(), d.manifest_hash))
                .collect();

            let cache_key = match crate_node.crate_type {
                CrateType::Lib => {
                    if dep_rlib_hashes.is_empty() {
                        cache_key_compile_rlib(&*db, rust_crate)
                            .await
                            .map_err(|e| AetherError::CacheKey(e.to_string()))?
                    } else {
                        cache_key_compile_rlib_with_deps(&*db, rust_crate, dep_rlib_hashes)
                            .await
                            .map_err(|e| AetherError::CacheKey(e.to_string()))?
                    }
                }
                CrateType::Bin => {
                    cache_key_compile_bin_with_deps(&*db, rust_crate, dep_rlib_hashes)
                        .await
                        .map_err(|e| AetherError::CacheKey(e.to_string()))?
                }
            };

            // Check cache
            let output_manifest = if let Some(cached) = self
                .cas
                .lookup(cache_key)
                .await
                .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
            {
                info!(
                    crate_name = %crate_node.crate_name,
                    manifest = %cached.short_hex(),
                    "cache hit"
                );
                // Complete action immediately on cache hit
                self.tui.complete_action(action_id).await;
                cached
            } else {
                // Cache miss - need to compile
                info!(
                    crate_name = %crate_node.crate_name,
                    "cache miss, compiling"
                );

                // Build compile request based on crate source
                let compile_request = match &crate_node.source {
                    CrateSource::Path { .. } => {
                        // Path crate - ingest source tree to CAS
                        let source_manifest = self
                            .ingest_source_tree(&closure_paths, &graph.workspace_root)
                            .await?;

                        RustCompileRequest {
                            toolchain_manifest: rust_toolchain.manifest_hash,
                            source_manifest,
                            crate_root: crate_node.crate_root_rel.to_string(),
                            crate_name: crate_node.crate_name.clone(),
                            crate_type: crate_node.crate_type.as_str().to_string(),
                            edition: crate_node.edition.as_str().to_string(),
                            target_triple: target_triple.clone(),
                            profile: profile.to_string(),
                            deps: deps.clone(),
                            registry_crate_manifest: None,
                        }
                    }
                    CrateSource::Registry { name, version, .. } => {
                        // Registry crate - rhea will extract and determine edition/lib_path
                        let registry_manifest = registry_manifests
                            .get(&(name.clone(), version.clone()))
                            .copied()
                            .ok_or_else(|| AetherError::RegistryCrateNoManifest {
                                name: name.clone(),
                                version: version.clone(),
                            })?;

                        RustCompileRequest {
                            toolchain_manifest: rust_toolchain.manifest_hash,
                            source_manifest: Blake3Hash([0u8; 32]), // Placeholder, rhea ignores
                            crate_root: String::new(),              // Placeholder, rhea ignores
                            crate_name: crate_node.crate_name.clone(),
                            crate_type: crate_node.crate_type.as_str().to_string(),
                            edition: String::new(), // Placeholder, rhea reads from Cargo.toml
                            target_triple: target_triple.clone(),
                            profile: profile.to_string(),
                            deps: deps.clone(),
                            registry_crate_manifest: Some(registry_manifest),
                        }
                    }
                };

                let result = self
                    .exec
                    .compile_rust(compile_request)
                    .await
                    .map_err(|e| AetherError::ExecRpc(std::sync::Arc::new(e)))?;

                if !result.success {
                    // Complete action even on failure
                    self.tui.complete_action(action_id).await;
                    return Err(AetherError::Compilation {
                        crate_name: crate_node.crate_name.clone(),
                        message: result.error.unwrap_or(result.stderr),
                    });
                }

                let output_manifest =
                    result
                        .output_manifest
                        .ok_or_else(|| AetherError::NoOutputManifest {
                            crate_name: crate_node.crate_name.clone(),
                        })?;

                // Publish to cache
                self.cas
                    .publish(cache_key, output_manifest)
                    .await
                    .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

                any_rebuilt = true;

                // Complete action after successful compilation
                self.tui.complete_action(action_id).await;

                output_manifest
            };

            // Record output for dependents
            compiled_outputs.insert(crate_node.id, output_manifest);

            // For bin crates, materialize the output
            if crate_node.crate_type == CrateType::Bin {
                let output_dir = graph
                    .workspace_root
                    .join(".vx/build")
                    .join(&target_triple)
                    .join(profile);

                tokio::fs::create_dir_all(&output_dir).await.map_err(|e| {
                    AetherError::CreateDir {
                        path: output_dir.clone(),
                        message: e.to_string(),
                    }
                })?;

                let output_path = output_dir.join(&crate_node.crate_name);

                // Fetch manifest and materialize
                let manifest = self
                    .cas
                    .get_manifest(output_manifest)
                    .await
                    .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                    .ok_or(AetherError::OutputManifestNotFound(output_manifest))?;

                for output in &manifest.outputs {
                    if output.logical == "bin" {
                        let blob_data = self
                            .cas
                            .get_blob(output.blob)
                            .await
                            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                            .ok_or(AetherError::BlobNotFound(output.blob))?;

                        vx_io::sync::atomic_write_executable(&output_path, &blob_data, true)
                            .map_err(|e| AetherError::WriteOutput {
                                path: output_path.clone(),
                                message: e.to_string(),
                            })?;

                        final_output_path = Some(output_path.clone());
                        break;
                    }
                }
            }
        }

        // Persist picante cache after build
        if let Err(e) = db.save_to_cache(&self.picante_cache).await {
            warn!(
                path = %self.picante_cache,
                error = %e,
                "failed to save picante cache"
            );
        } else {
            debug!(path = %self.picante_cache, "saved picante cache");
        }

        drop(db);

        let total_duration = total_start.elapsed();
        let root_name = &graph.root().crate_name;

        let cached = !any_rebuilt;
        let message = if cached {
            format!("{} {} (cached)", root_name, profile)
        } else {
            format!(
                "{} {} in {:.2}s",
                root_name,
                profile,
                total_duration.as_secs_f64()
            )
        };

        Ok(BuildResult {
            success: true,
            message,
            cached,
            duration_ms: total_duration.as_millis() as u64,
            output_path: final_output_path,
            error: None,
        })
    }
}

// Implement the Aether trait directly on AetherService
// The #[rapace::service] macro will generate a blanket impl for Arc<AetherService>
impl Aether for AetherService {
    async fn build(&self, request: BuildRequest) -> BuildResult {
        match self.do_build(request).await {
            Ok(result) => result,
            Err(e) => BuildResult {
                success: false,
                message: "Build failed".to_string(),
                cached: false,
                duration_ms: 0,
                output_path: None,
                // Convert structured error to string at RPC boundary
                error: Some(e.to_string()),
            },
        }
    }

    async fn shutdown(&self) {
        tracing::info!("Shutdown requested, killing spawned services");
        self.kill_spawned_services().await;
        tracing::info!("Spawned services killed, exiting aether");
        std::process::exit(0);
    }

    async fn version(&self) -> ServiceVersion {
        ServiceVersion {
            service: "vx-aether".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: AETHER_PROTOCOL_VERSION,
        }
    }
}
