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
use crate::executor::Executor;
use crate::inputs::*;
use crate::queries::*;
use camino::Utf8PathBuf;
use picante::wal::WalWriter;
use picante::HasRuntime;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};
use vx_aether_proto::{Aether, AETHER_PROTOCOL_VERSION, BuildRequest, BuildResult, ProgressListenerClient, ActionType};
use vx_cass_proto::{
    Blake3Hash, EnsureStatus, IngestTreeRequest, CassClient, RegistrySpec, RustChannel,
    RustComponent, RustToolchainSpec, ServiceVersion, TreeFile,
};
use vx_rhea_proto::{RheaClient, RustCompileRequest, RustDep};
use vx_report::{BuildReport, ReportStore};
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
    cas: Arc<CassClient>,

    /// Exec client for compilation
    exec: Arc<RheaClient>,

    /// The host triple of the execd machine (used for toolchain selection + cache keys).
    exec_host_triple: String,

    /// The picante incremental database
    db: Arc<Database>,

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

    /// Progress listener client (for reporting progress to vx CLI)
    /// Uses RwLock for interior mutability so it can be updated per-connection
    progress_listener: Arc<RwLock<Option<Arc<ProgressListenerClient>>>>,
}

impl AetherService {
    /// Report progress to CLI if connected
    async fn report_start_action(&self, action_type: ActionType) -> Option<u64> {
        let listener_guard = self.progress_listener.read().await;
        if let Some(ref listener) = *listener_guard {
            listener.start_action(action_type).await.ok()
        } else {
            None
        }
    }

    async fn report_complete_action(&self, action_id: Option<u64>) {
        let listener_guard = self.progress_listener.read().await;
        if let (Some(listener), Some(id)) = (listener_guard.as_ref(), action_id) {
            let _ = listener.complete_action(id).await;
        }
    }

    async fn report_set_total(&self, total: u64) {
        let listener_guard = self.progress_listener.read().await;
        if let Some(ref listener) = *listener_guard {
            let _ = listener.set_total(total).await;
        }
    }

    /// Create a new daemon service
    pub async fn new(
        cas: Arc<CassClient>,
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
            db: Arc::new(db),
            picante_cache: cache_path,
            picante_wal: wal_path,
            wal_writer: Arc::new(Mutex::new(wal_writer)),
            toolchains: Arc::new(Mutex::new(AcquiredToolchains {
                rust: None,
                zig: None,
            })),
            spawn_tracker,
            progress_listener: Arc::new(RwLock::new(None)),
        }
    }

    /// Update the progress listener for the current connection
    pub async fn set_progress_listener(&self, listener: Option<Arc<ProgressListenerClient>>) {
        *self.progress_listener.write().await = listener;
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

        // Track toolchain acquisition
        let action_id = self.report_start_action(ActionType::AcquireToolchain).await;

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
            .map_err(|e| {
                // Complete action on error
                let this = self.clone();
                tokio::spawn(async move { this.report_complete_action(action_id).await });
                AetherError::CasRpc(std::sync::Arc::new(e))
            })?;

        if result.status == EnsureStatus::Failed {
            self.report_complete_action(action_id).await;
            return Err(AetherError::ToolchainAcquisition(
                result.error.unwrap_or_else(|| "unknown error".to_string()),
            ));
        }

        let manifest_hash = result.manifest_hash.ok_or_else(|| {
            let this = self.clone();
            tokio::spawn(async move { this.report_complete_action(action_id).await });
            AetherError::NoManifestHash
        })?;

        // Get manifest for version info (and toolchain_id on cache hit)
        let manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .map_err(|e| {
                let this = self.clone();
                tokio::spawn(async move { this.report_complete_action(action_id).await });
                AetherError::CasRpc(std::sync::Arc::new(e))
            })?
            .ok_or_else(|| {
                let this = self.clone();
                tokio::spawn(async move { this.report_complete_action(action_id).await });
                AetherError::ToolchainManifestNotFound(manifest_hash)
            })?;

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

        // Complete the action
        self.report_complete_action(action_id).await;

        Ok(info)
    }

    /// Kill all spawned child services (casd, execd)
    pub async fn kill_spawned_services(&self) {
        self.spawn_tracker.lock().await.kill_all();
    }

    /// Build from vx.kdl manifest (C, Rust, or mixed projects)
    async fn build_from_vx_kdl(
        &self,
        _request: BuildRequest,
        vx_kdl_path: camino::Utf8PathBuf,
    ) -> Result<BuildResult> {
        use vx_project::{VxManifest, Language};

        // Parse vx.kdl
        let manifest = VxManifest::from_path(&vx_kdl_path)
            .map_err(|e| AetherError::InvalidManifest(format!("Failed to parse vx.kdl: {}", e)))?;

        manifest.validate()
            .map_err(|e| AetherError::InvalidManifest(format!("Invalid vx.kdl: {}", e)))?;

        // Determine project language
        let language = manifest.project.language()
            .ok_or_else(|| AetherError::InvalidManifest(
                format!("Unknown language: {}", manifest.project.lang)
            ))?;

        match language {
            Language::C => {
                info!(
                    project = %manifest.project.name,
                    bins = manifest.bins.len(),
                    "C project detected"
                );

                // TODO: Implement C project building
                // - Create action graph with AcquireZigToolchain, CompileCObject, LinkCBinary
                // - Execute action graph
                // - Return build result

                Ok(BuildResult {
                    success: false,
                    message: "C compilation not yet implemented".to_string(),
                    cached: false,
                    duration_ms: 0,
                    output_path: None,
                    error: Some("C compilation support is coming soon - action types are defined but execution is not yet implemented".to_string()),
                    total_actions: 0,
                    cache_hits: 0,
                    rebuilt: 0,
                })
            }
            Language::Rust => {
                // TODO: Convert vx.kdl Rust project to Cargo.toml path
                // For now, fail with helpful message
                Err(AetherError::InvalidManifest(
                    "Rust projects via vx.kdl not yet supported - use Cargo.toml for now".to_string()
                ))
            }
        }
    }

    /// Build a project.
    pub async fn do_build(&self, request: BuildRequest) -> Result<BuildResult> {
        let project_path = &request.project_path;
        let total_start = std::time::Instant::now();

        let target_triple = self.exec_host_triple.clone();

        // Check for vx.kdl first (unified project manifest)
        let vx_kdl_path = project_path.join("vx.kdl");
        if vx_kdl_path.exists() {
            info!("Found vx.kdl, using unified project manifest");
            return self.build_from_vx_kdl(request, vx_kdl_path).await;
        }

        // Fall back to Cargo.toml (Rust-only projects)
        // Build the crate graph with lockfile support
        let graph = CrateGraph::build_with_lockfile(project_path)
            .map_err(|e| AetherError::CrateGraph(format!("{:?}", miette::Report::new(e))))?;

        let profile = if request.release { "release" } else { "debug" };

        info!(
            workspace_root = %graph.workspace_root,
            path_crate_count = graph.nodes.len(),
            registry_crate_count = graph.iter_registry_crates().count(),
            "resolved crate graph, building action graph"
        );

        // Set up picante inputs (for cache key computation)
        // We create placeholder toolchain/config inputs since we don't have the actual
        // toolchain yet - it will be acquired by the action graph executor
        // Use a placeholder toolchain_id and manifest_hash - these will be acquired dynamically
        let placeholder_hash = Blake3Hash([0u8; 32]);
        let toolchain = RustToolchain::new(
            &*self.db,
            placeholder_hash,
            placeholder_hash,
            target_triple.clone(),
            target_triple.clone(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        RustToolchainManifest::set(&*self.db, placeholder_hash)
            .map_err(|e| AetherError::Picante(e.to_string()))?;

        let build_key = BuildConfig::compute_key(
            profile,
            &target_triple,
            graph.workspace_root.as_ref(),
        );
        let config = BuildConfig::new(
            &*self.db,
            build_key,
            profile.to_string(),
            target_triple.clone(),
            graph.workspace_root.to_string(),
        )
        .map_err(|e| AetherError::Picante(e.to_string()))?;

        // Action graph execution (Phase 2)
        use crate::action_graph::ActionGraph;
        use vx_cass_proto::RustChannel;

        let action_graph = ActionGraph::from_crate_graph(
            &graph,
            RustChannel::Stable,
            target_triple.clone(),
            toolchain,
            config,
            &self.db,
        )?;

        // Dump graph for debugging
        action_graph.dump_graph();

        // Set total number of actions for progress tracking
        // Total = 1 toolchain + N registry crates + M compilations
        let total_actions = action_graph.node_count();
        self.report_set_total(total_actions as u64).await;

        let max_concurrency = std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(16);
        tracing::info!("SERVICE: creating executor with max_concurrency={}", max_concurrency);

        // Get current progress listener (clones the Option<Arc<...>> inside the RwLock)
        let progress_listener = self.progress_listener.read().await.clone();

        let mut executor = Executor::new(
            action_graph,
            progress_listener,
            self.cas.clone(),
            self.exec.clone(),
            self.db.clone(),
            graph.workspace_root.clone(),
            target_triple.clone(),
            profile.to_string(),
            max_concurrency,
        );
        tracing::info!("SERVICE: calling executor.execute()");
        executor.execute().await?;
        tracing::info!("SERVICE: executor.execute() completed");

        // Append changes to WAL after build (incremental persistence)
        if let Some(ref mut wal) = *self.wal_writer.lock().await {
            match self.db.append_to_wal(wal).await {
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

        // Get execution statistics
        let exec_stats = executor.get_stats();
        let duration = total_start.elapsed();

        // Create and save build report
        let mut report = BuildReport::new(
            graph.workspace_root.to_string(),
            profile.to_string(),
            target_triple.clone(),
        );

        // Add node reports from execution
        for node_report in exec_stats.node_reports.iter() {
            report.add_node(node_report.clone());
        }

        // Finalize the report
        report.finalize(true, None);

        // Save the report to the project's .vx/runs/ directory
        let report_store = ReportStore::new(&graph.workspace_root);
        if let Err(e) = report_store.save(&report) {
            warn!(error = %e, "failed to save build report");
        } else {
            debug!(run_id = %report.run_id, "saved build report");
        }

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
            total_actions,
            cache_hits: exec_stats.cache_hits,
            rebuilt: exec_stats.rebuilt,
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
                total_actions: 0,
                cache_hits: 0,
                rebuilt: 0,
            },
        }
    }

    async fn shutdown(&self) {
        tracing::info!("Shutdown requested");

        // Compact WAL before shutdown to create a clean snapshot
        match self
            .db
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
