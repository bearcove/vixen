//! Action graph executor
//!
//! This module implements the execution engine that schedules and runs actions
//! from the action graph with optimal parallelism while respecting dependencies.

use petgraph::graph::NodeIndex;
use petgraph::Direction::Incoming;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, trace, warn};

use crate::action_graph::{Action, ActionGraph};
use crate::error::AetherError;
use vx_aether_proto::{ProgressListenerClient, ActionType};
use vx_cass_proto::{Blake3Hash, CassClient};
use vx_rhea_proto::RheaClient;
use vx_rs::CrateId;

/// State of an individual action
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionState {
    Pending,
    Running,
    Completed,
    Failed,
}

/// Result from executing an action
#[derive(Debug, Clone)]
pub enum ActionResult {
    /// Toolchain acquired successfully
    ToolchainAcquired {
        /// Toolchain ID
        toolchain_id: Blake3Hash,
        /// Toolchain manifest hash
        manifest_hash: Blake3Hash,
        /// Whether this was a cache hit
        was_cached: bool,
    },

    /// Registry crate acquired successfully
    RegistryCrateAcquired {
        /// Crate name
        name: String,
        /// Crate version
        version: String,
        /// Manifest hash
        manifest_hash: Blake3Hash,
        /// Whether this was a cache hit
        was_cached: bool,
    },

    /// Rust crate compiled successfully
    CrateCompiled {
        /// CrateId of the compiled crate
        crate_id: CrateId,
        /// Output manifest hash from CAS
        output_manifest: Blake3Hash,
        /// Path to materialized bin (if this was a bin crate)
        bin_output: Option<camino::Utf8PathBuf>,
        /// Whether this was a cache hit
        was_cached: bool,
    },
}

/// Statistics from build execution
#[derive(Debug, Clone, Default)]
pub struct ExecutionStats {
    /// Number of cache hits
    pub cache_hits: usize,
    /// Number of crates rebuilt
    pub rebuilt: usize,
    /// Path to final bin output (if any)
    pub bin_output: Option<camino::Utf8PathBuf>,
    /// Per-node reports for build report generation
    pub node_reports: Vec<vx_report::NodeReport>,
}

/// Message from spawned task to executor
enum ExecutorMessage {
    ActionCompleted {
        node_idx: NodeIndex,
        result: Result<ActionResult, AetherError>,
    },
}

/// Executor for action graph
pub struct Executor {
    /// The action graph (owned by executor, no lock needed)
    graph: ActionGraph,

    /// Current state of each action (owned by executor)
    states: HashMap<NodeIndex, ActionState>,

    /// Results from completed actions (shared read-only with spawned tasks)
    results: Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,

    /// Remaining dependency count for each action (owned by executor)
    remaining_deps: HashMap<NodeIndex, usize>,

    /// Queue of actions ready to execute (owned by executor)
    ready_queue: VecDeque<NodeIndex>,

    /// Semaphore limiting concurrency
    concurrency_limit: Arc<Semaphore>,

    /// Channel for messages from spawned tasks
    message_tx: tokio::sync::mpsc::UnboundedSender<ExecutorMessage>,
    message_rx: tokio::sync::mpsc::UnboundedReceiver<ExecutorMessage>,

    /// Progress listener client (for reporting progress to vx CLI)
    progress_listener: Option<Arc<ProgressListenerClient>>,

    /// CAS client (CAS)
    cas: Arc<CassClient>,

    /// Rhea client for execution
    exec: Arc<RheaClient>,

    /// Picante database (Arc-shareable with internal synchronization)
    db: Arc<crate::db::Database>,

    /// Execution statistics (owned by executor)
    stats: ExecutionStats,

    /// First error encountered during execution (owned by executor)
    first_error: Option<AetherError>,

    /// Workspace root for bin materialization
    workspace_root: camino::Utf8PathBuf,

    /// Target triple for bin materialization
    target_triple: String,

    /// Profile for bin materialization
    profile: String,
}

impl Executor {
    /// Create a new executor
    pub fn new(
        graph: ActionGraph,
        progress_listener: Option<Arc<ProgressListenerClient>>,
        cas: Arc<CassClient>,
        exec: Arc<RheaClient>,
        db: Arc<crate::db::Database>,
        workspace_root: camino::Utf8PathBuf,
        target_triple: String,
        profile: String,
        max_concurrency: usize,
    ) -> Self {
        trace!("EXECUTOR NEW: starting initialization");
        trace!("EXECUTOR NEW: graph has {} nodes", graph.graph.node_count());

        // Initialize remaining deps count and ready queue
        // We own the graph here, so no locking needed
        let mut remaining_deps = HashMap::new();
        let mut ready_queue = VecDeque::new();

        for node_idx in graph.graph.node_indices() {
            // Count outgoing edges (dependencies)
            let dep_count = graph.graph.neighbors(node_idx).count();
            let action = &graph.graph[node_idx].action;
            trace!(
                "EXECUTOR NEW: node {:?} action={} dep_count={}",
                node_idx, action.display_name(), dep_count
            );
            remaining_deps.insert(node_idx, dep_count);

            // If no dependencies, it's ready immediately
            if dep_count == 0 {
                info!(
                    "EXECUTOR NEW: READY node {:?} action={}",
                    node_idx, action.display_name()
                );
                ready_queue.push_back(node_idx);
            }
        }

        info!(
            "EXECUTOR NEW: initialized with ready_queue_size={} total_nodes={}",
            ready_queue.len(),
            graph.graph.node_count()
        );

        // Log first few ready actions for debugging
        for (i, &node_idx) in ready_queue.iter().enumerate().take(3) {
            let action = &graph.graph[node_idx].action;
            info!(
                "EXECUTOR NEW: ready[{}] = {:?} {}",
                i, node_idx, action.display_name()
            );
        }

        let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();

        Self {
            graph,
            states: HashMap::new(),
            results: Arc::new(RwLock::new(HashMap::new())),
            remaining_deps,
            ready_queue,
            concurrency_limit: Arc::new(Semaphore::new(max_concurrency)),
            message_tx,
            message_rx,
            progress_listener,
            cas,
            exec,
            db,
            stats: ExecutionStats::default(),
            first_error: None,
            workspace_root,
            target_triple,
            profile,
        }
    }

    /// Get execution statistics
    pub fn get_stats(&self) -> ExecutionStats {
        self.stats.clone()
    }

    /// Main execution loop
    pub async fn execute(&mut self) -> Result<(), AetherError> {
        info!("EXECUTE: Starting action graph execution");

        loop {
            // Check if any action has failed
            if let Some(ref error) = self.first_error {
                warn!("EXECUTE: error detected, aborting");
                return Err(error.clone());
            }

            // Spawn all ready actions
            while let Some(node_idx) = self.ready_queue.pop_front() {
                info!("EXECUTE: spawning action for node {:?}", node_idx);
                self.states.insert(node_idx, ActionState::Running);
                let permit = self.concurrency_limit.clone().acquire_owned().await.unwrap();
                self.spawn_action(node_idx, permit);
            }

            // Check if we're done
            let total = self.graph.graph.node_count();
            let completed = self.states.values().filter(|&&s| s == ActionState::Completed).count();
            if completed == total && total > 0 {
                info!("EXECUTE: all {} actions completed", total);
                break;
            }

            // Wait for a completion message
            match self.message_rx.recv().await {
                Some(ExecutorMessage::ActionCompleted { node_idx, result }) => {
                    match result {
                        Ok(action_result) => {
                            info!("EXECUTE: action {:?} completed successfully", node_idx);
                            self.states.insert(node_idx, ActionState::Completed);
                            self.results.write().await.insert(node_idx, action_result);

                            // Decrement dependency counts and add newly ready actions to queue
                            for dependent_idx in self.graph.graph.neighbors_directed(node_idx, Incoming) {
                                let count = self.remaining_deps.get_mut(&dependent_idx).unwrap();
                                *count -= 1;
                                if *count == 0 {
                                    info!("EXECUTE: action {:?} now ready", dependent_idx);
                                    self.ready_queue.push_back(dependent_idx);
                                }
                            }
                        }
                        Err(error) => {
                            warn!("EXECUTE: action {:?} failed: {}", node_idx, error);
                            self.first_error = Some(error.clone());
                            return Err(error);
                        }
                    }
                }
                None => {
                    // Channel closed
                    break;
                }
            }
        }

        // Compute stats from results
        let results_lock = self.results.read().await;
        let mut cache_hits = 0;
        let mut rebuilt = 0;
        let mut bin_output = None;
        let mut node_reports = Vec::new();

        for (node_idx, result) in results_lock.iter() {
            let action = &self.graph.graph[*node_idx].action;

            match result {
                ActionResult::ToolchainAcquired { was_cached, manifest_hash, .. } => {
                    if *was_cached {
                        cache_hits += 1;
                    } else {
                        rebuilt += 1;
                    }

                    // Generate node report for toolchain acquisition
                    node_reports.push(vx_report::NodeReport {
                        node_id: action.display_name(),
                        kind: "toolchain.acquire".to_string(),
                        cache_key: manifest_hash.to_hex(),
                        cache: if *was_cached {
                            vx_report::CacheOutcome::Hit {
                                manifest: manifest_hash.to_hex(),
                            }
                        } else {
                            vx_report::CacheOutcome::Miss {
                                reason: vx_report::MissReason::FirstBuild,
                            }
                        },
                        timing: vx_report::NodeTiming::default(),
                        inputs: vec![],
                        deps: vec![],
                        outputs: vec![vx_report::OutputRecord {
                            logical: "toolchain".to_string(),
                            manifest: Some(manifest_hash.to_hex()),
                            blob: None,
                            path: None,
                        }],
                        invocation: None,
                        diagnostics: vx_report::DiagnosticsRecord::default(),
                    });
                }
                ActionResult::RegistryCrateAcquired { name, version, manifest_hash, was_cached } => {
                    if *was_cached {
                        cache_hits += 1;
                    } else {
                        rebuilt += 1;
                    }

                    // Generate node report for registry crate acquisition
                    node_reports.push(vx_report::NodeReport {
                        node_id: format!("acquire-registry:{}:{}", name, version),
                        kind: "registry.acquire".to_string(),
                        cache_key: manifest_hash.to_hex(),
                        cache: if *was_cached {
                            vx_report::CacheOutcome::Hit {
                                manifest: manifest_hash.to_hex(),
                            }
                        } else {
                            vx_report::CacheOutcome::Miss {
                                reason: vx_report::MissReason::FirstBuild,
                            }
                        },
                        timing: vx_report::NodeTiming::default(),
                        inputs: vec![
                            vx_report::InputRecord {
                                label: "crate".to_string(),
                                value: format!("{}:{}", name, version),
                            },
                        ],
                        deps: vec![],
                        outputs: vec![vx_report::OutputRecord {
                            logical: "source".to_string(),
                            manifest: Some(manifest_hash.to_hex()),
                            blob: None,
                            path: None,
                        }],
                        invocation: None,
                        diagnostics: vx_report::DiagnosticsRecord::default(),
                    });
                }
                ActionResult::CrateCompiled {
                    crate_id,
                    output_manifest,
                    bin_output: bin_path,
                    was_cached,
                } => {
                    if *was_cached {
                        cache_hits += 1;
                    } else {
                        rebuilt += 1;
                    }
                    // Track the bin output (if any)
                    if let Some(path) = bin_path {
                        bin_output = Some(path.clone());
                    }

                    // Determine crate type from action
                    let (crate_type_str, kind) = match action {
                        Action::CompileRustCrate { crate_type, .. } => {
                            let type_str = crate_type.as_str();
                            let kind = if type_str == "bin" {
                                "rust.compile_bin"
                            } else {
                                "rust.compile_rlib"
                            };
                            (type_str.to_string(), kind.to_string())
                        }
                        _ => ("unknown".to_string(), "rust.compile".to_string()),
                    };

                    // Generate node report for crate compilation
                    node_reports.push(vx_report::NodeReport {
                        node_id: format!("compile-{}:{}", crate_type_str, action.display_name().replace("compile ", "")),
                        kind,
                        cache_key: crate_id.to_string(),
                        cache: if *was_cached {
                            vx_report::CacheOutcome::Hit {
                                manifest: output_manifest.to_hex(),
                            }
                        } else {
                            vx_report::CacheOutcome::Miss {
                                reason: vx_report::MissReason::KeyNotFound,
                            }
                        },
                        timing: vx_report::NodeTiming::default(),
                        inputs: vec![
                            vx_report::InputRecord {
                                label: "source_closure".to_string(),
                                value: crate_id.to_string(),
                            },
                        ],
                        deps: vec![],
                        outputs: vec![vx_report::OutputRecord {
                            logical: crate_type_str,
                            manifest: Some(output_manifest.to_hex()),
                            blob: None,
                            path: bin_path.as_ref().map(|p| p.to_string()),
                        }],
                        invocation: None,
                        diagnostics: vx_report::DiagnosticsRecord::default(),
                    });
                }
            }
        }

        self.stats.cache_hits = cache_hits;
        self.stats.rebuilt = rebuilt;
        self.stats.bin_output = bin_output;
        self.stats.node_reports = node_reports;

        Ok(())
    }

    /// Spawn an action for execution
    fn spawn_action(&self, node_idx: NodeIndex, _permit: tokio::sync::OwnedSemaphorePermit) {
        // Get action before spawning (we own the graph, no lock needed)
        let action = self.graph.graph[node_idx].action.clone();

        let message_tx = self.message_tx.clone();
        let cas = self.cas.clone();
        let exec = self.exec.clone();
        let db = self.db.clone();
        let progress_listener = self.progress_listener.clone();
        let workspace_root = self.workspace_root.clone();
        let target_triple = self.target_triple.clone();
        let profile = self.profile.clone();
        let results = self.results.clone(); // Cloned Arc for execute_action
        let toolchain_node_idx = self.graph.toolchain_node; // For looking up toolchain result

        tokio::spawn(async move {
            info!("SPAWN_ACTION task: started for {:?}", node_idx);

            // Start progress tracking
            let action_type = action.to_progress_action_type();
            let action_id = if let Some(ref listener) = progress_listener {
                listener.start_action(action_type).await.ok()
            } else {
                None
            };

            info!("SPAWN_ACTION task: calling execute_action for {:?}", node_idx);
            // Execute action - pass results as immutable ref
            let result = execute_action(
                action,
                &cas,
                &exec,
                &db,
                &results,
                &workspace_root,
                &target_triple,
                &profile,
                toolchain_node_idx,
            )
            .await;

            info!("SPAWN_ACTION task: execute_action returned for {:?}: {:?}", node_idx, result.is_ok());

            // Complete progress tracking
            if let (Some(listener), Some(id)) = (&progress_listener, action_id) {
                let _ = listener.complete_action(id).await;
            }

            info!("SPAWN_ACTION task: sending completion message for {:?}", node_idx);
            // Send result back to executor
            let _ = message_tx.send(ExecutorMessage::ActionCompleted { node_idx, result });

            info!("SPAWN_ACTION task: done for {:?}", node_idx);
            // Permit dropped here, allowing next action to run
        });
    }

}

/// Execute a single action
async fn execute_action(
    action: Action,
    cas: &Arc<CassClient>,
    exec: &Arc<RheaClient>,
    db: &Arc<crate::db::Database>,
    results: &Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,
    workspace_root: &camino::Utf8PathBuf,
    target_triple: &str,
    profile: &str,
    toolchain_node_idx: Option<NodeIndex>,
) -> Result<ActionResult, AetherError> {
    use camino::Utf8PathBuf;
    use vx_rhea_proto::{RustCompileRequest, RustDep};
    use vx_cass_proto::{TreeFile, IngestTreeRequest};
    use vx_rs::crate_graph::CrateSource;
    use crate::queries::*;

    info!("EXECUTE_ACTION: processing action {}", action.display_name());

    match action {
        Action::AcquireToolchain { channel, target_triple } => {
            use vx_cass_proto::{RustToolchainSpec, RustComponent, EnsureStatus};

            let spec = RustToolchainSpec {
                channel,
                host: target_triple.clone(),
                target: target_triple.clone(),
                components: vec![RustComponent::Rustc, RustComponent::RustStd],
            };

            let result = cas
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
            let manifest = cas
                .get_toolchain_manifest(manifest_hash)
                .await
                .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                .ok_or(AetherError::ToolchainManifestNotFound(manifest_hash))?;

            // On cache hit, toolchain_id comes from manifest; on download, it's in the result
            let toolchain_id = result.toolchain_id.unwrap_or(manifest.toolchain_id);

            info!(
                toolchain_id = %toolchain_id.short_hex(),
                manifest_hash = %manifest_hash.short_hex(),
                version = ?manifest.rust_version,
                "toolchain acquired"
            );

            Ok(ActionResult::ToolchainAcquired {
                toolchain_id,
                manifest_hash,
                was_cached: result.status == EnsureStatus::Hit,
            })
        }

        Action::AcquireRegistryCrate { name, version, checksum } => {
            use vx_cass_proto::{RegistrySpec, EnsureStatus};

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

                    let was_cached = result.status == EnsureStatus::Hit;

                    debug!(
                        name = %name,
                        version = %version,
                        manifest_hash = %manifest_hash.short_hex(),
                        was_cached = was_cached,
                        "acquired registry crate"
                    );

                    Ok(ActionResult::RegistryCrateAcquired {
                        name,
                        version,
                        manifest_hash,
                        was_cached,
                    })
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

        Action::CompileRustCrate {
            crate_id,
            crate_name,
            crate_type,
            edition,
            crate_root_rel,
            source,
            deps: action_deps,
            workspace_root: workspace_root_str,
            target_triple,
            profile,
            mut rust_crate,
            toolchain,
            config,
        } => {
            info!("COMPILE: starting compilation for crate {}", crate_name);
            let workspace_root = Utf8PathBuf::from(&workspace_root_str);

            // Look up the toolchain result to get the real manifest hash and toolchain ID
            let (toolchain_manifest, toolchain_id) = if let Some(toolchain_idx) = toolchain_node_idx {
                let results_lock = results.read().await;
                match results_lock.get(&toolchain_idx) {
                    Some(ActionResult::ToolchainAcquired { toolchain_id, manifest_hash, .. }) => {
                        info!("COMPILE: using acquired toolchain manifest_hash={} toolchain_id={}",
                              manifest_hash.short_hex(), toolchain_id.short_hex());
                        (*manifest_hash, *toolchain_id)
                    }
                    _ => {
                        return Err(AetherError::Compilation {
                            crate_name: crate_name.clone(),
                            message: "toolchain not acquired before compilation".to_string(),
                        });
                    }
                }
            } else {
                return Err(AetherError::Compilation {
                    crate_name: crate_name.clone(),
                    message: "no toolchain node found".to_string(),
                });
            };

            // Update the RustToolchainManifest and RustToolchain in the database with real values
            use crate::inputs::{RustToolchain as RustToolchainInput, RustToolchainManifest};
            RustToolchainManifest::set(&**db, toolchain_manifest)
                .map_err(|e| AetherError::Picante(e.to_string()))?;

            // Update the toolchain input with real values for cache key computation
            let toolchain = RustToolchainInput::new(
                &**db,
                toolchain_id,
                toolchain_manifest,
                toolchain.host(&**db).map_err(|e| AetherError::Picante(e.to_string()))?,
                toolchain.target(&**db).map_err(|e| AetherError::Picante(e.to_string()))?,
            )
            .map_err(|e| AetherError::Picante(e.to_string()))?;

            info!("COMPILE: computing source closure for {}", crate_name);
            // Compute source closure and hash (for path crates)
            let (closure_paths, closure_hash) = match &source {
                CrateSource::Path { .. } => {
                    let crate_root_abs = workspace_root.join(&crate_root_rel);
                    let paths = vx_rs::rust_source_closure(&crate_root_abs, &workspace_root)
                        .map_err(|e| AetherError::SourceClosure {
                            crate_name: crate_name.clone(),
                            message: e.to_string(),
                        })?;

                    let hash = vx_rs::hash_source_closure(&paths, &workspace_root)
                        .map_err(|e| AetherError::SourceHash(e.to_string()))?;

                    (paths, hash)
                }
                CrateSource::Registry { .. } => {
                    // Registry crates don't need source closure
                    (vec![], Blake3Hash([0u8; 32]))
                }
            };

            info!("COMPILE: source closure computed for {}, updating picante", crate_name);
            // Update RustCrate with actual closure hash
            let crate_id_hex = crate_id.short_hex();
            rust_crate = crate::inputs::RustCrate::new(
                &**db,
                crate_id_hex,
                crate_name.clone(),
                edition.clone(),
                crate_type.as_str().to_string(),
                crate_root_rel.clone(),
                closure_hash,
            )
            .map_err(|e| AetherError::Picante(e.to_string()))?;

            // Collect dependency results from completed actions
            let (deps, registry_crate_manifests): (Vec<RustDep>, HashMap<(String, String), Blake3Hash>) = {
                let results_lock = results.read().await;

                // Build map of CrateId -> output_manifest from completed compilation actions
                let completed_manifests: HashMap<CrateId, Blake3Hash> = results_lock
                    .iter()
                    .filter_map(|(_, result)| {
                        match result {
                            ActionResult::CrateCompiled {
                                crate_id,
                                output_manifest,
                                ..
                            } => Some((*crate_id, *output_manifest)),
                            _ => None,
                        }
                    })
                    .collect();

                // Build map of (name, version) -> manifest_hash from registry acquisition actions
                let registry_crate_manifests: HashMap<(String, String), Blake3Hash> = results_lock
                    .iter()
                    .filter_map(|(_, result)| {
                        match result {
                            ActionResult::RegistryCrateAcquired {
                                name,
                                version,
                                manifest_hash,
                                ..
                            } => Some(((name.clone(), version.clone()), *manifest_hash)),
                            _ => None,
                        }
                    })
                    .collect();

                // Build RustDep list by resolving each ActionDep
                let deps = action_deps
                    .iter()
                    .map(|action_dep| {
                        let manifest_hash = completed_manifests
                            .get(&action_dep.crate_id)
                            .ok_or_else(|| AetherError::DependencyNotCompiled {
                                dep_name: action_dep.extern_name.clone(),
                                crate_name: crate_name.clone(),
                            })?;

                        // Determine registry_crate_manifest based on source (from ActionResults)
                        let registry_crate_manifest = match &action_dep.source {
                            CrateSource::Registry { name, version, .. } => {
                                registry_crate_manifests.get(&(name.clone(), version.clone())).copied()
                            }
                            CrateSource::Path { .. } => None,
                        };

                        Ok(RustDep {
                            extern_name: action_dep.extern_name.clone(),
                            manifest_hash: *manifest_hash,
                            registry_crate_manifest,
                        })
                    })
                    .collect::<Result<Vec<_>, AetherError>>()?;

                (deps, registry_crate_manifests)
            };

            let dep_rlib_hashes: Vec<(String, Blake3Hash)> = deps
                .iter()
                .map(|d| (d.extern_name.clone(), d.manifest_hash))
                .collect();

            // Compute cache key - tracked functions need db but don't hold lock during execution
            let cache_key = {
                match crate_type {
                    vx_rs::CrateType::Lib => {
                        if dep_rlib_hashes.is_empty() {
                            cache_key_compile_rlib(&**db, rust_crate, toolchain, config)
                                .await
                                .map_err(|e| AetherError::CacheKey(e.to_string()))?
                        } else {
                            cache_key_compile_rlib_with_deps(&**db, rust_crate, toolchain, config, dep_rlib_hashes.clone())
                                .await
                                .map_err(|e| AetherError::CacheKey(e.to_string()))?
                        }
                    }
                    vx_rs::CrateType::Bin => {
                        cache_key_compile_bin_with_deps(&**db, rust_crate, toolchain, config, dep_rlib_hashes.clone())
                            .await
                            .map_err(|e| AetherError::CacheKey(e.to_string()))?
                    }
                }
            };

            // Check cache
            let (output_manifest, was_cached) = if let Some(cached) = cas
                .lookup(cache_key)
                .await
                .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
            {
                info!(
                    crate_name = %crate_name,
                    manifest = %cached.short_hex(),
                    "cache hit"
                );
                // Track cache hit
                (cached, true)
            } else {
                // Cache miss - need to compile
                info!(
                    crate_name = %crate_name,
                    "cache miss, compiling"
                );

                // Build compile request based on crate source
                let compile_request = match &source {
                    CrateSource::Path { .. } => {
                        // Path crate - ingest source tree to CAS
                        let mut files = Vec::with_capacity(closure_paths.len());

                        for rel_path in &closure_paths {
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
                                executable: false,
                            });
                        }

                        let ingest_req = IngestTreeRequest { files };
                        let ingest_result = cas
                            .ingest_tree(ingest_req)
                            .await
                            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

                        if !ingest_result.success {
                            return Err(AetherError::TreeIngestion(
                                ingest_result
                                    .error
                                    .unwrap_or_else(|| "tree ingestion failed".to_string()),
                            ));
                        }

                        let source_manifest = ingest_result.manifest_hash.ok_or_else(|| {
                            AetherError::TreeIngestion("missing manifest hash".to_string())
                        })?;

                        RustCompileRequest {
                            toolchain_manifest,
                            source_manifest,
                            crate_root: crate_root_rel.clone(),
                            crate_name: crate_name.clone(),
                            crate_type: crate_type.as_str().to_string(),
                            edition: edition.clone(),
                            target_triple: target_triple.clone(),
                            profile: profile.clone(),
                            deps: deps.clone(),
                            registry_crate_manifest: None,
                        }
                    }
                    CrateSource::Registry { name, version, .. } => {
                        // Registry crate - rhea will extract and determine edition/lib_path
                        let registry_manifest = registry_crate_manifests
                            .get(&(name.clone(), version.clone()))
                            .copied()
                            .ok_or_else(|| AetherError::RegistryCrateNoManifest {
                                name: name.clone(),
                                version: version.clone(),
                            })?;

                        RustCompileRequest {
                            toolchain_manifest,
                            source_manifest: Blake3Hash([0u8; 32]), // Placeholder, rhea ignores
                            crate_root: String::new(),              // Placeholder, rhea ignores
                            crate_name: crate_name.clone(),
                            crate_type: crate_type.as_str().to_string(),
                            edition: String::new(), // Placeholder, rhea reads from Cargo.toml
                            target_triple: target_triple.clone(),
                            profile: profile.clone(),
                            deps: deps.clone(),
                            registry_crate_manifest: Some(registry_manifest),
                        }
                    }
                };

                let result = exec
                    .compile_rust(compile_request)
                    .await
                    .map_err(|e| AetherError::ExecRpc(std::sync::Arc::new(e)))?;

                if !result.success {
                    return Err(AetherError::Compilation {
                        crate_name: crate_name.clone(),
                        message: result.error.unwrap_or(result.stderr),
                    });
                }

                let output_manifest =
                    result
                        .output_manifest
                        .ok_or_else(|| AetherError::NoOutputManifest {
                            crate_name: crate_name.clone(),
                        })?;

                // Publish to cache
                cas
                    .publish(cache_key, output_manifest)
                    .await
                    .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?;

                // Track rebuild

                (output_manifest, false)
            };

            // Materialize bin outputs (always materialize, even on cache hit)
            let bin_output = if crate_type == vx_rs::crate_graph::CrateType::Bin {
                let output_dir = workspace_root
                    .join(".vx/build")
                    .join(target_triple)
                    .join(profile);

                tokio::fs::create_dir_all(&output_dir).await.map_err(|e| {
                    AetherError::CreateDir {
                        path: output_dir.clone(),
                        message: e.to_string(),
                    }
                })?;

                // Ensure .vx is in .gitignore (best-effort, don't fail build if this fails)
                match vx_io::git::ensure_vx_gitignored(&workspace_root).await {
                    Ok(true) => {
                        debug!("Added /.vx to .gitignore");
                    }
                    Ok(false) => {
                        // Already in gitignore or not in a git repo
                    }
                    Err(e) => {
                        warn!("Failed to update .gitignore: {}", e);
                    }
                }

                let output_path = output_dir.join(&crate_name);

                // Fetch manifest and materialize
                let manifest = cas
                    .get_manifest(output_manifest)
                    .await
                    .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                    .ok_or(AetherError::OutputManifestNotFound(output_manifest))?;

                let mut materialized = false;
                for output in &manifest.outputs {
                    if output.logical == "bin" {
                        let blob_data = cas
                            .get_blob(output.blob)
                            .await
                            .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                            .ok_or(AetherError::BlobNotFound(output.blob))?;

                        vx_io::sync::atomic_write_executable(&output_path, &blob_data, true)
                            .map_err(|e| AetherError::WriteOutput {
                                path: output_path.clone(),
                                message: e.to_string(),
                            })?;

                        materialized = true;
                        break;
                    }
                }

                if materialized {
                    Some(output_path)
                } else {
                    None
                }
            } else {
                None
            };

            Ok(ActionResult::CrateCompiled {
                crate_id,
                output_manifest,
                bin_output,
                was_cached,
            })
        }
    }
}
