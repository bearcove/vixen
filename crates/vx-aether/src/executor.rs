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
        /// Output manifest hash from CAS
        output_manifest: Blake3Hash,
    },
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
}

impl Executor {
    /// Create a new executor
    pub fn new(
        graph: ActionGraph,
        tui: TuiHandle,
        cas: Arc<OortClient>,
        exec: Arc<RheaClient>,
        db: Arc<Mutex<crate::db::Database>>,
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
        }
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
            let result = execute_action(action, &cas, &exec, &db).await;

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
    _cas: &Arc<OortClient>,
    _exec: &Arc<RheaClient>,
    _db: &Arc<Mutex<crate::db::Database>>,
) -> Result<ActionResult, AetherError> {
    match action {
        Action::CompileRustCrate { crate_name, .. } => {
            // TODO: Implement actual compilation
            // For now, just return a placeholder
            debug!(crate_name, "Would compile crate (not implemented yet)");
            Ok(ActionResult::CrateCompiled {
                output_manifest: Blake3Hash([0u8; 32]),
            })
        }
    }
}
