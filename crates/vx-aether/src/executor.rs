//! Action graph executor
//!
//! This module implements the execution engine that schedules and runs actions
//! from the action graph with optimal parallelism while respecting dependencies.

use petgraph::graph::NodeIndex;
use petgraph::Direction::Incoming;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{debug, info};

use crate::action_graph::{Action, ActionGraph};
use crate::error::AetherError;
use crate::tui::TuiHandle;
use vx_oort_proto::{Blake3Hash, OortClient};
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
    /// Rust crate compiled successfully
    CrateCompiled {
        /// CrateId of the compiled crate
        crate_id: CrateId,
        /// Output manifest hash from CAS
        output_manifest: Blake3Hash,
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
}

/// Executor for action graph
pub struct Executor {
    /// The action graph (wrapped in RwLock to allow dynamic expansion in future)
    graph: Arc<RwLock<ActionGraph>>,

    /// Current state of each action
    states: Arc<RwLock<HashMap<NodeIndex, ActionState>>>,

    /// Results from completed actions
    results: Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,

    /// Remaining dependency count for each action
    remaining_deps: Arc<RwLock<HashMap<NodeIndex, usize>>>,

    /// Queue of actions ready to execute
    ready_queue: Arc<RwLock<VecDeque<NodeIndex>>>,

    /// Semaphore limiting concurrency
    concurrency_limit: Arc<Semaphore>,

    /// Channel for completion notifications
    completion_tx: tokio::sync::mpsc::UnboundedSender<NodeIndex>,
    completion_rx: tokio::sync::mpsc::UnboundedReceiver<NodeIndex>,

    /// TUI handle for progress tracking
    tui: TuiHandle,

    /// Oort client (CAS)
    cas: Arc<OortClient>,

    /// Rhea client for execution
    exec: Arc<RheaClient>,

    /// Picante database
    db: Arc<Mutex<crate::db::Database>>,

    /// Registry crate manifests (for registry dependencies)
    registry_manifests: Arc<HashMap<(String, String), Blake3Hash>>,

    /// Execution statistics
    stats: Arc<RwLock<ExecutionStats>>,

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
        tui: TuiHandle,
        cas: Arc<OortClient>,
        exec: Arc<RheaClient>,
        db: Arc<Mutex<crate::db::Database>>,
        registry_manifests: HashMap<(String, String), Blake3Hash>,
        workspace_root: camino::Utf8PathBuf,
        target_triple: String,
        profile: String,
        max_concurrency: usize,
    ) -> Self {
        let graph_ref = Arc::new(RwLock::new(graph));

        // Initialize remaining deps count
        let mut remaining_deps = HashMap::new();
        let mut ready_queue = VecDeque::new();

        // We need to use tokio::runtime::Handle to block on async operations
        // during initialization. This is safe because we're in a tokio context.
        let rt = tokio::runtime::Handle::current();
        let graph_lock = rt.block_on(graph_ref.read());

        for node_idx in graph_lock.graph.node_indices() {
            // Count outgoing edges (dependencies)
            let dep_count = graph_lock.graph.neighbors(node_idx).count();
            remaining_deps.insert(node_idx, dep_count);

            // If no dependencies, it's ready immediately
            if dep_count == 0 {
                ready_queue.push_back(node_idx);
            }
        }
        drop(graph_lock);

        let (completion_tx, completion_rx) = tokio::sync::mpsc::unbounded_channel();

        Self {
            graph: graph_ref,
            states: Arc::new(RwLock::new(HashMap::new())),
            results: Arc::new(RwLock::new(HashMap::new())),
            remaining_deps: Arc::new(RwLock::new(remaining_deps)),
            ready_queue: Arc::new(RwLock::new(ready_queue)),
            concurrency_limit: Arc::new(Semaphore::new(max_concurrency)),
            completion_tx,
            completion_rx,
            tui,
            cas,
            exec,
            db,
            registry_manifests: Arc::new(registry_manifests),
            stats: Arc::new(RwLock::new(ExecutionStats::default())),
            workspace_root,
            target_triple,
            profile,
        }
    }

    /// Get execution statistics
    pub async fn get_stats(&self) -> ExecutionStats {
        self.stats.read().await.clone()
    }

    /// Main execution loop
    pub async fn execute(&mut self) -> Result<(), AetherError> {
        info!("Starting action graph execution");

        loop {
            // Try to spawn all ready actions
            loop {
                let node_idx = {
                    let mut queue = self.ready_queue.write().await;
                    queue.pop_front()
                };

                match node_idx {
                    Some(idx) => {
                        let permit = self.concurrency_limit.clone().acquire_owned().await.unwrap();
                        self.spawn_action(idx, permit).await;
                    }
                    None => break, // No more ready actions
                }
            }

            // Wait for completion or check if we're done
            tokio::select! {
                Some(completed_idx) = self.completion_rx.recv() => {
                    self.handle_completion(completed_idx).await?;
                }
                else => {
                    // Channel closed and no more messages
                    break;
                }
            }

            // Check if all actions are complete
            let (total, completed) = {
                let graph = self.graph.read().await;
                let states = self.states.read().await;
                let total = graph.graph.node_count();
                let completed = states
                    .values()
                    .filter(|&&s| s == ActionState::Completed)
                    .count();
                (total, completed)
            };

            if completed == total && total > 0 {
                info!(total, "All actions completed");
                break;
            }
        }

        Ok(())
    }

    /// Spawn an action for execution
    async fn spawn_action(&self, node_idx: NodeIndex, _permit: tokio::sync::OwnedSemaphorePermit) {
        let graph = self.graph.clone();
        let states = self.states.clone();
        let results = self.results.clone();
        let completion_tx = self.completion_tx.clone();
        let cas = self.cas.clone();
        let exec = self.exec.clone();
        let db = self.db.clone();
        let tui = self.tui.clone();
        let registry_manifests = self.registry_manifests.clone();
        let stats = self.stats.clone();
        let workspace_root = self.workspace_root.clone();
        let target_triple = self.target_triple.clone();
        let profile = self.profile.clone();

        tokio::spawn(async move {
            // Mark as running
            states.write().await.insert(node_idx, ActionState::Running);

            // Get action
            let action = {
                let g = graph.read().await;
                g.graph[node_idx].action.clone()
            };

            // Start TUI tracking
            let action_type = action.to_tui_action_type();
            let action_id = tui.start_action(action_type).await;

            // Execute action
            let result = execute_action(
                action,
                &cas,
                &exec,
                &db,
                &registry_manifests,
                &results,
                &stats,
                &workspace_root,
                &target_triple,
                &profile,
            )
            .await;

            // Complete TUI tracking
            tui.complete_action(action_id).await;

            // Store result
            match result {
                Ok(r) => {
                    debug!(node = ?node_idx, "Action completed successfully");
                    states.write().await.insert(node_idx, ActionState::Completed);
                    results.write().await.insert(node_idx, r);
                }
                Err(e) => {
                    tracing::error!(node = ?node_idx, error = %e, "Action failed");
                    states.write().await.insert(node_idx, ActionState::Failed);
                    // TODO: Better error handling - should we continue or abort?
                }
            }

            // Notify completion
            let _ = completion_tx.send(node_idx);

            // Permit dropped here, allowing next action to run
        });
    }

    /// Handle completion of an action
    async fn handle_completion(&mut self, completed_idx: NodeIndex) -> Result<(), AetherError> {
        // Find dependents (actions waiting on this one)
        let dependents: Vec<NodeIndex> = {
            let graph = self.graph.read().await;
            graph
                .graph
                .neighbors_directed(completed_idx, Incoming)
                .collect()
        };

        // Decrement remaining deps for each dependent
        let mut newly_ready = Vec::new();
        {
            let mut remaining = self.remaining_deps.write().await;
            for dependent_idx in dependents {
                if let Some(count) = remaining.get_mut(&dependent_idx) {
                    *count -= 1;
                    if *count == 0 {
                        newly_ready.push(dependent_idx);
                    }
                }
            }
        }

        // Add newly ready actions to queue
        {
            let mut queue = self.ready_queue.write().await;
            for idx in newly_ready {
                queue.push_back(idx);
            }
        }

        Ok(())
    }
}

/// Execute a single action
async fn execute_action(
    action: Action,
    cas: &Arc<OortClient>,
    exec: &Arc<RheaClient>,
    db: &Arc<Mutex<crate::db::Database>>,
    registry_manifests: &Arc<HashMap<(String, String), Blake3Hash>>,
    results: &Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,
    stats: &Arc<RwLock<ExecutionStats>>,
    workspace_root: &camino::Utf8PathBuf,
    target_triple: &str,
    profile: &str,
) -> Result<ActionResult, AetherError> {
    use camino::Utf8PathBuf;
    use vx_rhea_proto::{RustCompileRequest, RustDep};
    use vx_oort_proto::{TreeFile, IngestTreeRequest};
    use vx_rs::crate_graph::CrateSource;
    use crate::queries::*;

    match action {
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
            toolchain_manifest,
            mut rust_crate,
            toolchain,
            config,
        } => {
            let workspace_root = Utf8PathBuf::from(&workspace_root_str);

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

            // Update RustCrate with actual closure hash
            let crate_id_hex = crate_id.short_hex();
            let db_guard = db.lock().await;
            rust_crate = crate::inputs::RustCrate::new(
                &*db_guard,
                crate_id_hex,
                crate_name.clone(),
                edition.clone(),
                crate_type.as_str().to_string(),
                crate_root_rel.clone(),
                closure_hash,
            )
            .map_err(|e| AetherError::Picante(e.to_string()))?;
            drop(db_guard);

            // Collect dependency results from completed actions
            let deps: Vec<RustDep> = {
                let results_lock = results.read().await;

                // Build map of CrateId -> output_manifest from completed actions
                let completed_manifests: HashMap<CrateId, Blake3Hash> = results_lock
                    .iter()
                    .filter_map(|(_, result)| {
                        match result {
                            ActionResult::CrateCompiled {
                                crate_id,
                                output_manifest,
                            } => Some((*crate_id, *output_manifest)),
                        }
                    })
                    .collect();

                // Build RustDep list by resolving each ActionDep
                action_deps
                    .iter()
                    .map(|action_dep| {
                        let manifest_hash = completed_manifests
                            .get(&action_dep.crate_id)
                            .ok_or_else(|| AetherError::DependencyNotCompiled {
                                dep_name: action_dep.extern_name.clone(),
                                crate_name: crate_name.clone(),
                            })?;

                        // Determine registry_crate_manifest based on source
                        let registry_crate_manifest = match &action_dep.source {
                            CrateSource::Registry { name, version, .. } => {
                                registry_manifests.get(&(name.clone(), version.clone())).copied()
                            }
                            CrateSource::Path { .. } => None,
                        };

                        Ok(RustDep {
                            extern_name: action_dep.extern_name.clone(),
                            manifest_hash: *manifest_hash,
                            registry_crate_manifest,
                        })
                    })
                    .collect::<Result<Vec<_>, AetherError>>()?
            };

            let dep_rlib_hashes: Vec<(String, Blake3Hash)> = deps
                .iter()
                .map(|d| (d.extern_name.clone(), d.manifest_hash))
                .collect();

            // Compute cache key
            let db_guard = db.lock().await;
            let cache_key = match crate_type {
                vx_rs::CrateType::Lib => {
                    if dep_rlib_hashes.is_empty() {
                        cache_key_compile_rlib(&*db_guard, rust_crate, toolchain, config)
                            .await
                            .map_err(|e| AetherError::CacheKey(e.to_string()))?
                    } else {
                        cache_key_compile_rlib_with_deps(&*db_guard, rust_crate, toolchain, config, dep_rlib_hashes.clone())
                            .await
                            .map_err(|e| AetherError::CacheKey(e.to_string()))?
                    }
                }
                vx_rs::CrateType::Bin => {
                    cache_key_compile_bin_with_deps(&*db_guard, rust_crate, toolchain, config, dep_rlib_hashes.clone())
                        .await
                        .map_err(|e| AetherError::CacheKey(e.to_string()))?
                }
            };
            drop(db_guard);

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
                stats.write().await.cache_hits += 1;
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
                        let registry_manifest = registry_manifests
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
                stats.write().await.rebuilt += 1;

                (output_manifest, false)
            };

            // Materialize bin outputs
            if crate_type == vx_rs::crate_graph::CrateType::Bin && !was_cached {
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

                let output_path = output_dir.join(&crate_name);

                // Fetch manifest and materialize
                let manifest = cas
                    .get_manifest(output_manifest)
                    .await
                    .map_err(|e| AetherError::CasRpc(std::sync::Arc::new(e)))?
                    .ok_or(AetherError::OutputManifestNotFound(output_manifest))?;

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

                        // Update stats with bin output path
                        stats.write().await.bin_output = Some(output_path.clone());
                        break;
                    }
                }
            }

            Ok(ActionResult::CrateCompiled {
                crate_id,
                output_manifest,
            })
        }
    }
}
