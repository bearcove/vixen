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
use picante::wal::WalWriter;
use picante::HasRuntime;
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

    /// Path to the picante WAL file
    picante_wal: Utf8PathBuf,

    /// WAL writer for incremental persistence (kept open across builds)
    wal_writer: Arc<Mutex<Option<WalWriter>>>,

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
        let wal_path = vx_home.join("picante.wal");

        // Load persisted picante cache
        let cache_loaded = match db.load_from_cache(&cache_path).await {
            Ok(true) => {
                info!(path = %cache_path, "loaded picante cache");
                true
            }
            Ok(false) => {
                debug!(path = %cache_path, "no picante cache found, starting fresh");
                false
            }
            Err(e) => {
                warn!(path = %cache_path, error = %e, "failed to load picante cache, starting fresh");
                false
            }
        };

        // If cache was loaded, replay WAL to apply incremental changes
        if cache_loaded {
            match db.replay_wal(&wal_path).await {
                Ok(count) if count > 0 => {
                    info!(path = %wal_path, entries = count, "replayed WAL entries");
                }
                Ok(_) => {
                    debug!(path = %wal_path, "no WAL entries to replay");
                }
                Err(e) => {
                    warn!(path = %wal_path, error = %e, "failed to replay WAL, continuing without it");
                }
            }
        }

        // Create a fresh WAL file for this session based on current revision
        // (which includes any changes from the replayed WAL)
        let current_revision = db.runtime().current_revision().0;
        let wal_writer = match WalWriter::create(&wal_path, current_revision) {
            Ok(writer) => {
                info!(path = %wal_path, base_revision = current_revision, "created WAL writer for incremental persistence");
                Some(writer)
            }
            Err(e) => {
                warn!(path = %wal_path, error = %e, "failed to create WAL writer, persistence will be disabled");
                None
            }
        };

        Self {
            cas,
            exec,
            exec_host_triple,
            db: Arc::new(Mutex::new(db)),
            picante_cache: cache_path,
            picante_wal: wal_path,
            wal_writer: Arc::new(Mutex::new(wal_writer)),
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

        let toolchain = RustToolchain::new(
            &*db,
            rust_toolchain.toolchain_id,
            rust_toolchain.manifest_hash,
            target_triple.clone(),
            target_triple.clone(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        RustToolchainManifest::set(&*db, rust_toolchain.manifest_hash)
            .map_err(|e| AetherError::Picante(e.to_string()))?;

        let build_key = BuildConfig::compute_key(
            &profile,
            &target_triple,
            &graph.workspace_root.to_string(),
        );
        let config = BuildConfig::new(
            &*db,
            build_key,
            profile.to_string(),
            target_triple.clone(),
            graph.workspace_root.to_string(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        // Action graph execution (Phase 1.5)
        use crate::action_graph::ActionGraph;
        use crate::executor::Executor;

        let action_graph = ActionGraph::from_crate_graph(&graph, toolchain, config, &*db)?;
        let max_concurrency = std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(16);
        let mut executor = Executor::new(
            action_graph,
            self.tui.clone(),
            self.cas.clone(),
            self.exec.clone(),
            self.db.clone(),
            registry_manifests,
            graph.workspace_root.clone(),
            target_triple.clone(),
            profile.to_string(),
            max_concurrency,
        );
        executor.execute().await?;

        // Append changes to WAL after build (incremental persistence)
        if let Some(ref mut wal) = *self.wal_writer.lock().await {
            match db.append_to_wal(wal).await {
                Ok(count) if count > 0 => {
                    info!(path = %self.picante_wal, entries = count, "appended changes to WAL");
                    // Explicit flush to ensure durability (WAL auto-flushes after threshold, but we flush after each build)
                    if let Err(e) = wal.flush() {
                        warn!(path = %self.picante_wal, error = %e, "failed to flush WAL");
                    }
                }
                Ok(_) => {
                    debug!("no WAL changes to persist");
                }
                Err(e) => {
                    warn!(path = %self.picante_wal, error = %e, "failed to append to WAL");
                }
            }
        }
        drop(db);

        // Get execution statistics
        let exec_stats = executor.get_stats().await;
        let duration = total_start.elapsed();

        Ok(BuildResult {
            success: true,
            message: format!(
                "Build completed: {} rebuilt, {} cached",
                exec_stats.rebuilt, exec_stats.cache_hits
            ),
            cached: exec_stats.rebuilt == 0 && exec_stats.cache_hits > 0,
            duration_ms: duration.as_millis() as u64,
            output_path: exec_stats.bin_output,
            error: None,
        })
    }
}


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
        tracing::info!("Shutdown requested");

        // Compact WAL before shutdown to create a clean snapshot
        let db = self.db.lock().await;
        match db
            .compact_wal(&self.picante_cache, &self.picante_wal, false)
            .await
        {
            Ok(revision) => {
                info!(
                    cache = %self.picante_cache,
                    revision = revision,
                    "compacted WAL to snapshot"
                );
            }
            Err(e) => {
                warn!(error = %e, "failed to compact WAL on shutdown");
            }
        }
        drop(db);

        // Kill spawned services
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
