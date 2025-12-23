//! DaemonService implementation.
//!
//! Orchestrates builds by:
//! 1. Computing cache keys via picante
//! 2. Checking CAS for cache hits
//! 3. Sending compile requests to execd for cache misses
//! 4. Materializing outputs locally

use crate::SpawnTracker;
use crate::db::Database;
use crate::inputs::*;
use crate::queries::*;
use camino::Utf8PathBuf;
// TODO: Re-enable picante cache persistence - waiting for https://github.com/bearcove/picante/issues/32
// use picante::persist::{CacheLoadOptions, OnCorruptCache, load_cache_with_options, save_cache};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info};
use vx_cas_proto::{
    Blake3Hash, CasClient, EnsureStatus, IngestTreeRequest, RegistrySpec, RustChannel,
    RustComponent, RustToolchainSpec, TreeFile,
};
use vx_daemon_proto::{BuildRequest, BuildResult};
use vx_exec_proto::{ExecClient, RustCompileRequest, RustDep};
use vx_rs::{CrateGraph, CrateGraphError, CrateId, CrateType, ModuleError};
use vx_rs::crate_graph::CrateSource;

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
pub struct DaemonService {
    /// CAS client for content-addressed storage
    cas: Arc<CasClient>,

    /// Exec client for compilation
    exec: Arc<ExecClient>,

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
}

impl DaemonService {
    /// Create a new daemon service
    pub fn new(
        cas: Arc<CasClient>,
        exec: Arc<ExecClient>,
        vx_home: Utf8PathBuf,
        exec_host_triple: String,
        spawn_tracker: Arc<Mutex<SpawnTracker>>,
    ) -> Self {
        let db = Database::new();
        let cache_path = vx_home.join("picante.cache");

        // TODO: Load persisted picante cache when https://github.com/bearcove/picante/issues/32 is resolved

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
        }
    }

    /// Ensure Rust toolchain exists in CAS. Returns info for cache keys.
    pub async fn ensure_rust_toolchain(
        &self,
        channel: RustChannel,
    ) -> Result<ToolchainInfo, String> {
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
            .map_err(|e| format!("RPC error: {:?}", e))?;

        if result.status == EnsureStatus::Failed {
            return Err(result.error.unwrap_or_else(|| "unknown error".to_string()));
        }

        let manifest_hash = result
            .manifest_hash
            .ok_or_else(|| "no manifest hash returned".to_string())?;
        let toolchain_id = result
            .toolchain_id
            .ok_or_else(|| "no toolchain id returned".to_string())?;

        // Get manifest for version info
        let manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .map_err(|e| format!("RPC error: {:?}", e))?
            .ok_or_else(|| "manifest not found".to_string())?;

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
    ) -> Result<Blake3Hash, String> {
        let mut files = Vec::with_capacity(paths.len());

        for rel_path in paths {
            let abs_path = workspace_root.join(rel_path);
            let contents = tokio::fs::read(&abs_path)
                .await
                .map_err(|e| format!("failed to read {}: {}", abs_path, e))?;

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
            .map_err(|e| format!("RPC error: {:?}", e))?;

        if !result.success {
            return Err(result
                .error
                .unwrap_or_else(|| "tree ingestion failed".to_string()));
        }

        let manifest_hash = result
            .manifest_hash
            .ok_or_else(|| "no manifest hash returned".to_string())?;

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
        cas: Arc<CasClient>,
        name: String,
        version: String,
        checksum: String,
    ) -> Result<(String, Blake3Hash), String> {
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
            .map_err(|e| format!("RPC error for {}@{}: {:?}", name, version, e))?;

        match result.status {
            EnsureStatus::Hit | EnsureStatus::Downloaded => {
                let manifest_hash = result
                    .manifest_hash
                    .ok_or_else(|| format!("{}@{}: missing manifest_hash", name, version))?;

                debug!(
                    name = %name,
                    version = %version,
                    manifest_hash = %manifest_hash.short_hex(),
                    "acquired registry crate"
                );

                Ok((format!("{}@{}", name, version), manifest_hash))
            }
            EnsureStatus::Failed => Err(result
                .error
                .unwrap_or_else(|| format!("{}@{}: acquisition failed", name, version))),
        }
    }

    /// Acquire registry crates in parallel and return mapping of "name@version" -> manifest_hash
    async fn acquire_registry_crates(
        &self,
        graph: &CrateGraph,
    ) -> Result<HashMap<String, Blake3Hash>, String> {
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
    pub async fn do_build(&self, request: BuildRequest) -> Result<BuildResult, String> {
        let project_path = &request.project_path;
        let total_start = std::time::Instant::now();

        // Acquire hermetic Rust toolchain
        let rust_toolchain = self.ensure_rust_toolchain(RustChannel::Stable).await?;

        let target_triple = self.exec_host_triple.clone();

        // Build the crate graph with lockfile support
        let graph = CrateGraph::build_with_lockfile(project_path)
            .map_err(|e: CrateGraphError| format_diagnostic(&e))?;

        info!(
            workspace_root = %graph.workspace_root,
            path_crate_count = graph.nodes.len(),
            registry_crate_count = graph.iter_registry_crates().count(),
            toolchain_id = %rust_toolchain.toolchain_id.short_hex(),
            "resolved crate graph"
        );

        // Acquire registry crates in parallel
        let registry_manifests = self.acquire_registry_crates(&graph).await?;

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
        .map_err(|e| format!("picante error: {}", e))?;

        RustToolchainManifest::set(&*db, rust_toolchain.manifest_hash)
            .map_err(|e| format!("picante error: {}", e))?;

        BuildConfig::set(
            &*db,
            profile.to_string(),
            target_triple.clone(),
            graph.workspace_root.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Track compiled outputs for dependencies
        // Maps CrateId -> manifest_hash of the compiled output
        let mut compiled_outputs: HashMap<CrateId, Blake3Hash> = HashMap::new();

        let mut any_rebuilt = false;
        let mut final_output_path: Option<Utf8PathBuf> = None;

        // Process crates in topological order
        for crate_node in graph.iter_topo() {
            debug!(
                crate_name = %crate_node.crate_name,
                crate_type = ?crate_node.crate_type,
                "processing crate"
            );

            // Compute source closure
            let crate_root_abs = graph.workspace_root.join(&crate_node.crate_root_rel);
            let closure_paths = vx_rs::rust_source_closure(&crate_root_abs, &graph.workspace_root)
                .map_err(|e| format_module_error(&e))?;

            let closure_hash = vx_rs::hash_source_closure(&closure_paths, &graph.workspace_root)
                .map_err(|e| format!("failed to hash source closure: {}", e))?;

            // Create RustCrate input
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
            .map_err(|e| format!("picante error: {}", e))?;

            // Collect dependency manifest hashes
            let deps: Vec<RustDep> = crate_node
                .deps
                .iter()
                .map(|dep| {
                    let manifest_hash = compiled_outputs.get(&dep.crate_id).ok_or_else(|| {
                        format!(
                            "dependency {} not yet compiled for {}",
                            dep.extern_name, crate_node.crate_name
                        )
                    })?;

                    // Look up dependency node to check if it's a registry crate
                    let dep_node = graph
                        .nodes
                        .get(&dep.crate_id)
                        .ok_or_else(|| format!("dependency {} node not found", dep.extern_name))?;

                    let registry_crate_manifest = match &dep_node.source {
                        CrateSource::Registry { name, version, .. } => {
                            let key = format!("{}@{}", name, version);
                            registry_manifests.get(&key).copied()
                        }
                        CrateSource::Path { .. } => None,
                    };

                    Ok(RustDep {
                        extern_name: dep.extern_name.clone(),
                        manifest_hash: *manifest_hash,
                        registry_crate_manifest,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;

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
                            .map_err(|e| format!("cache key error: {}", e))?
                    } else {
                        cache_key_compile_rlib_with_deps(&*db, rust_crate, dep_rlib_hashes)
                            .await
                            .map_err(|e| format!("cache key error: {}", e))?
                    }
                }
                CrateType::Bin => {
                    cache_key_compile_bin_with_deps(&*db, rust_crate, dep_rlib_hashes)
                        .await
                        .map_err(|e| format!("cache key error: {}", e))?
                }
            };

            // Check cache
            let output_manifest = if let Some(cached) = self
                .cas
                .lookup(cache_key)
                .await
                .map_err(|e| format!("RPC error: {:?}", e))?
            {
                info!(
                    crate_name = %crate_node.crate_name,
                    manifest = %cached.short_hex(),
                    "cache hit"
                );
                cached
            } else {
                // Cache miss - need to compile
                info!(
                    crate_name = %crate_node.crate_name,
                    "cache miss, compiling"
                );

                // Ingest source tree to CAS
                let source_manifest = self
                    .ingest_source_tree(&closure_paths, &graph.workspace_root)
                    .await?;

                let compile_request = RustCompileRequest {
                    toolchain_manifest: rust_toolchain.manifest_hash,
                    source_manifest,
                    crate_root: crate_node.crate_root_rel.to_string(),
                    crate_name: crate_node.crate_name.clone(),
                    crate_type: crate_node.crate_type.as_str().to_string(),
                    edition: crate_node.edition.as_str().to_string(),
                    target_triple: target_triple.clone(),
                    profile: profile.to_string(),
                    deps: deps.clone(),
                };

                let result = self
                    .exec
                    .compile_rust(compile_request)
                    .await
                    .map_err(|e| format!("RPC error: {:?}", e))?;

                if !result.success {
                    return Err(format!(
                        "compilation failed for {}: {}",
                        crate_node.crate_name,
                        result.error.unwrap_or(result.stderr)
                    ));
                }

                let output_manifest = result
                    .output_manifest
                    .ok_or_else(|| "no output manifest returned".to_string())?;

                // Publish to cache
                self.cas
                    .publish(cache_key, output_manifest)
                    .await
                    .map_err(|e| format!("RPC error: {:?}", e))?;

                any_rebuilt = true;
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

                tokio::fs::create_dir_all(&output_dir)
                    .await
                    .map_err(|e| format!("failed to create output dir: {}", e))?;

                let output_path = output_dir.join(&crate_node.crate_name);

                // Fetch manifest and materialize
                let manifest = self
                    .cas
                    .get_manifest(output_manifest)
                    .await
                    .map_err(|e| format!("RPC error: {:?}", e))?
                    .ok_or_else(|| "output manifest not found".to_string())?;

                for output in &manifest.outputs {
                    if output.logical == "bin" {
                        let blob_data = self
                            .cas
                            .get_blob(output.blob)
                            .await
                            .map_err(|e| format!("RPC error: {:?}", e))?
                            .ok_or_else(|| "blob not found".to_string())?;

                        vx_io::sync::atomic_write_executable(&output_path, &blob_data, true)
                            .map_err(|e| format!("failed to write output: {}", e))?;

                        final_output_path = Some(output_path.clone());
                        break;
                    }
                }
            }
        }

        // TODO: Persist picante cache after build when https://github.com/bearcove/picante/issues/32 is resolved

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

/// Format a diagnostic error using miette
fn format_diagnostic(err: &dyn miette::Diagnostic) -> String {
    let mut buf = String::new();
    let handler = miette::GraphicalReportHandler::new_themed(miette::GraphicalTheme::unicode());
    if handler.render_report(&mut buf, err).is_ok() {
        buf
    } else {
        format!("{}", err)
    }
}

/// Format a module error
fn format_module_error(e: &ModuleError) -> String {
    format!("{}", e)
}
