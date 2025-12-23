# Action Graph Refactor Plan - Issue #13

## Overview

Refactor aether from imperative sequential orchestration to declarative action graph execution.

**Current:** Sequential loop processing crates in topological order
**Target:** Action graph with explicit dependencies, scheduled for optimal parallelism

## Benefits

1. **Natural progress tracking** - Know total actions upfront, graph state = TUI state
2. **Optimal parallelism** - Independent crates compile concurrently
3. **Clear dependency reasoning** - Explicit action dependencies in graph
4. **Dynamic expansion** - Build scripts can spawn new actions (C compilation, etc.)
5. **Distribution ready** - Actions can be scheduled across multiple rhea workers
6. **Better caching** - Actions map 1:1 to cache keys

## Architecture

### Phase 1: Graph Construction

Build complete action graph upfront from CrateGraph + lockfile

### Phase 2: Graph Execution

Schedule actions when dependencies satisfied, execute with bounded parallelism

### Phase 3: Dynamic Expansion

Build scripts and other actions can insert new nodes/edges during execution

---

## Data Structures

### Action Types

```rust
enum Action {
    /// Acquire Rust toolchain from static.rust-lang.org
    AcquireToolchain {
        channel: RustChannel,
    },

    /// Acquire registry crate from crates.io
    AcquireRegistryCrate {
        name: String,
        version: String,
        checksum: String,
    },

    /// Ingest source tree to CAS
    IngestSource {
        crate_name: String,
        paths: Vec<PathBuf>,
    },

    /// Compile Rust crate (lib or bin)
    CompileRustCrate {
        crate_id: CrateId,
        crate_name: String,
        crate_type: CrateType,
        // Picante inputs (stored for execution)
        rust_crate: RustCrate,
        toolchain: RustToolchain,
        config: BuildConfig,
    },

    /// Run build script (future expansion)
    RunBuildScript {
        crate_id: CrateId,
        script_path: PathBuf,
    },

    /// Compile C source file (discovered from build script)
    CompileCFile {
        source_path: PathBuf,
        object_path: PathBuf,
    },

    /// Link static library from object files
    LinkStaticLib {
        objects: Vec<PathBuf>,
        output: PathBuf,
    },

    /// Materialize binary output
    MaterializeBinary {
        crate_name: String,
        output_manifest: Blake3Hash,
        output_path: PathBuf,
    },
}
```

### Action Graph

```rust
use petgraph::graph::{DiGraph, NodeIndex};

struct ActionGraph {
    /// Directed acyclic graph of actions
    /// Edges point from action → dependency (reverse of typical)
    /// (Or use standard dependency direction, TBD)
    graph: DiGraph<ActionNode, ()>,

    /// Map crate IDs to their compile action nodes (for dependency resolution)
    crate_to_node: HashMap<CrateId, NodeIndex>,
}

struct ActionNode {
    /// The actual work to do
    action: Action,

    /// Display name for TUI
    display_name: String,
}
```

### Execution State

```rust
struct Executor {
    /// The action graph
    graph: Arc<RwLock<ActionGraph>>,

    /// Current state of each action
    states: Arc<RwLock<HashMap<NodeIndex, ActionState>>>,

    /// Results from completed actions
    results: Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,

    /// Remaining dependency count per action (for scheduling)
    remaining_deps: Arc<RwLock<HashMap<NodeIndex, usize>>>,

    /// Ready queue (actions with all deps satisfied)
    ready_queue: Arc<RwLock<VecDeque<NodeIndex>>>,

    /// Semaphore for concurrency control
    concurrency_limit: Arc<Semaphore>,

    /// Channel for completion notifications
    completion_tx: mpsc::UnboundedSender<NodeIndex>,
    completion_rx: mpsc::UnboundedReceiver<NodeIndex>,

    /// TUI handle for progress tracking
    tui: TuiHandle,

    /// Shared resources
    cas: Arc<dyn CasClient>,
    exec: Arc<dyn RheaClient>,
    db: Arc<Mutex<Database>>,
}

enum ActionState {
    Pending,
    Running,
    Completed,
    Failed,
}

enum ActionResult {
    ToolchainAcquired { toolchain_id: Blake3Hash, manifest_hash: Blake3Hash },
    RegistryCrateAcquired { manifest_hash: Blake3Hash },
    SourceIngested { manifest_hash: Blake3Hash },
    CrateCompiled { output_manifest: Blake3Hash },
    BuildScriptCompleted { new_actions: Vec<(Action, Vec<NodeIndex>)> }, // Action + its deps
    CFileCompiled { object_hash: Blake3Hash },
    LibLinked { lib_hash: Blake3Hash },
    BinaryMaterialized { path: PathBuf },
}
```

---

## Implementation Phases

### Phase 0: Picante Singleton Refactor (PREREQUISITE)

**Problem:** Current code uses `RustToolchain::set()` and `BuildConfig::set()` as singletons. This breaks concurrent builds.

**Solution:** Convert to keyed inputs passed as parameters.

#### Changes Required

1. **Update `inputs.rs`:**

```rust
// Add key to RustToolchain
#[picante::input]
pub struct RustToolchain {
    #[key]
    pub toolchain_id: Blake3Hash,  // Already unique, becomes the key
    pub toolchain_manifest: Blake3Hash,
    pub host: String,
    pub target: String,
}

// Make BuildConfig keyed by composite
#[picante::input]
pub struct BuildConfig {
    #[key]
    pub build_key: String,  // Derived from profile+target+workspace
    pub profile: String,
    pub target_triple: String,
    pub workspace_root: String,
}

impl BuildConfig {
    pub fn compute_key(profile: &str, target: &str, workspace: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"build_config:");
        hasher.update(profile.as_bytes());
        hasher.update(b":");
        hasher.update(target.as_bytes());
        hasher.update(b":");
        hasher.update(workspace.as_bytes());
        hex::encode(&hasher.finalize().as_bytes()[..16])
    }
}
```

2. **Update all cache key queries in `queries.rs`:**

Change from:
```rust
pub async fn cache_key_compile_rlib<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<CacheKey> {
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let toolchain = RustToolchain::get(db)?.expect("RustToolchain not set");
    // ...
}
```

To:
```rust
pub async fn cache_key_compile_rlib<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    toolchain: RustToolchain,  // Now passed explicitly
    config: BuildConfig,        // Now passed explicitly
) -> PicanteResult<CacheKey> {
    // Use toolchain and config directly (already have them)
    // ...
}
```

3. **Update `service.rs::do_build()`:**

Change from:
```rust
RustToolchain::set(&*db, toolchain_id, manifest_hash, host, target)?;
BuildConfig::set(&*db, profile, target_triple, workspace_root)?;
```

To:
```rust
let toolchain = RustToolchain::new(&*db, toolchain_id, manifest_hash, host, target)?;
let config = BuildConfig::new(
    &*db,
    BuildConfig::compute_key(&profile, &target_triple, &workspace_root),
    profile,
    target_triple,
    workspace_root,
)?;

// Pass to cache key queries:
cache_key_compile_rlib(&*db, rust_crate, toolchain, config).await?
```

4. **Update all tests** in `tests.rs` to use `.new()` instead of `.set()`

**Validation:** After this refactor, multiple concurrent builds can share the same Db and picante will correctly memoize based on actual configuration.

---

### Phase 1: Core Action Graph (Minimal MVP)

Goal: Replace current sequential compilation loop with action graph for Rust crates only (no build scripts, no C deps yet)

#### Step 1.1: Create Action Graph Module

**File:** `crates/vx-aether/src/action_graph.rs`

```rust
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use vx_rs::{CrateId, CrateGraph};

pub struct ActionGraph {
    graph: DiGraph<ActionNode, ()>,
    crate_to_node: HashMap<CrateId, NodeIndex>,
}

pub struct ActionNode {
    pub action: Action,
    pub display_name: String,
}

impl ActionGraph {
    /// Build initial graph from CrateGraph
    pub fn from_crate_graph(
        graph: &CrateGraph,
        toolchain: RustToolchain,
        config: BuildConfig,
    ) -> Self {
        let mut action_graph = DiGraph::new();
        let mut crate_to_node = HashMap::new();

        // Create actions for all crates
        for crate_node in graph.nodes.values() {
            let action = Action::CompileRustCrate {
                crate_id: crate_node.id,
                crate_name: crate_node.crate_name.clone(),
                crate_type: crate_node.crate_type,
                rust_crate: /* create from crate_node */,
                toolchain: toolchain.clone(),
                config: config.clone(),
            };

            let node_idx = action_graph.add_node(ActionNode {
                action,
                display_name: format!("compile {}", crate_node.crate_name),
            });

            crate_to_node.insert(crate_node.id, node_idx);
        }

        // Add dependency edges
        for crate_node in graph.nodes.values() {
            let dependent_idx = crate_to_node[&crate_node.id];
            for dep in &crate_node.deps {
                let dependency_idx = crate_to_node[&dep.crate_id];
                // Edge from dependent → dependency
                action_graph.add_edge(dependent_idx, dependency_idx, ());
            }
        }

        Self { graph: action_graph, crate_to_node }
    }
}
```

#### Step 1.2: Create Executor

**File:** `crates/vx-aether/src/executor.rs`

```rust
pub struct Executor {
    graph: Arc<RwLock<ActionGraph>>,
    states: Arc<RwLock<HashMap<NodeIndex, ActionState>>>,
    results: Arc<RwLock<HashMap<NodeIndex, ActionResult>>>,
    remaining_deps: Arc<RwLock<HashMap<NodeIndex, usize>>>,
    ready_queue: Arc<RwLock<VecDeque<NodeIndex>>>,
    concurrency_limit: Arc<Semaphore>,
    completion_tx: mpsc::UnboundedSender<NodeIndex>,
    tui: TuiHandle,
    // ... other fields
}

impl Executor {
    pub fn new(
        graph: ActionGraph,
        tui: TuiHandle,
        cas: Arc<dyn CasClient>,
        exec: Arc<dyn RheaClient>,
        db: Arc<Mutex<Database>>,
        max_concurrency: usize,
    ) -> Self {
        let graph_ref = Arc::new(RwLock::new(graph));

        // Initialize remaining deps count
        let mut remaining_deps = HashMap::new();
        let graph_lock = graph_ref.blocking_read();
        for node_idx in graph_lock.graph.node_indices() {
            let dep_count = graph_lock.graph.neighbors(node_idx).count();
            remaining_deps.insert(node_idx, dep_count);
        }
        drop(graph_lock);

        // Find initially ready actions (no dependencies)
        let mut ready_queue = VecDeque::new();
        for (node_idx, &count) in &remaining_deps {
            if count == 0 {
                ready_queue.push_back(*node_idx);
            }
        }

        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        Self {
            graph: graph_ref,
            states: Arc::new(RwLock::new(HashMap::new())),
            results: Arc::new(RwLock::new(HashMap::new())),
            remaining_deps: Arc::new(RwLock::new(remaining_deps)),
            ready_queue: Arc::new(RwLock::new(ready_queue)),
            concurrency_limit: Arc::new(Semaphore::new(max_concurrency)),
            completion_tx,
            tui,
            cas,
            exec,
            db,
        }
    }

    /// Main execution loop
    pub async fn execute(&mut self) -> Result<BuildResult> {
        loop {
            // Try to spawn ready actions
            while let Some(node_idx) = self.ready_queue.write().await.pop_front() {
                let permit = self.concurrency_limit.clone().acquire_owned().await.unwrap();
                self.spawn_action(node_idx, permit).await;
            }

            // Wait for completion
            let Some(completed_idx) = self.completion_rx.recv().await else {
                break; // All done
            };

            self.handle_completion(completed_idx).await?;
        }

        Ok(BuildResult { /* ... */ })
    }

    async fn spawn_action(&self, node_idx: NodeIndex, _permit: OwnedSemaphorePermit) {
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
                    states.write().await.insert(node_idx, ActionState::Completed);
                    results.write().await.insert(node_idx, r);
                }
                Err(e) => {
                    states.write().await.insert(node_idx, ActionState::Failed);
                    // Handle error
                }
            }

            // Notify completion
            let _ = completion_tx.send(node_idx);

            // Permit dropped here, allowing next action to run
        });
    }

    async fn handle_completion(&mut self, completed_idx: NodeIndex) -> Result<()> {
        // Find dependents (actions waiting on this one)
        let dependents: Vec<NodeIndex> = {
            let graph = self.graph.read().await;
            graph.graph.neighbors_directed(completed_idx, Incoming)
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
    cas: &Arc<dyn CasClient>,
    exec: &Arc<dyn RheaClient>,
    db: &Arc<Mutex<Database>>,
) -> Result<ActionResult> {
    match action {
        Action::CompileRustCrate { crate_id, rust_crate, toolchain, config, .. } => {
            // Compute cache key
            let cache_key = {
                let db = db.lock().await;
                cache_key_compile_rlib(&*db, rust_crate, toolchain, config).await?
            };

            // Check cache
            if let Some(cached) = cas.lookup(cache_key).await? {
                return Ok(ActionResult::CrateCompiled { output_manifest: cached });
            }

            // Cache miss - compile
            // (similar to current service.rs logic)
            // ...

            Ok(ActionResult::CrateCompiled { output_manifest })
        }
        // Handle other action types...
        _ => todo!(),
    }
}
```

#### Step 1.3: Integrate into `service.rs::do_build()`

Replace current sequential loop (lines 437-697) with:

```rust
// After Phase 0 refactor, we have toolchain and config as keyed inputs
let toolchain = RustToolchain::new(&*db, rust_toolchain.toolchain_id, ...)?;
let config = BuildConfig::new(&*db, build_key, profile, target_triple, workspace_root)?;

// Build action graph
let action_graph = ActionGraph::from_crate_graph(&graph, toolchain, config);

// Set TUI total
self.tui.set_total(action_graph.graph.node_count()).await;

// Execute
let mut executor = Executor::new(
    action_graph,
    self.tui.clone(),
    self.cas.clone(),
    self.exec.clone(),
    self.db.clone(),
    num_cpus::get() * 2, // Concurrency limit
);

executor.execute().await?
```

---

### Phase 2: Add Toolchain and Registry Actions

Refactor toolchain/registry acquisition to be actions in the graph

#### Step 2.1: Add Actions to Initial Graph

```rust
impl ActionGraph {
    pub fn from_crate_graph(
        graph: &CrateGraph,
        target_toolchain_id: Blake3Hash,
    ) -> Self {
        let mut action_graph = DiGraph::new();

        // Add toolchain acquisition action
        let toolchain_action = action_graph.add_node(ActionNode {
            action: Action::AcquireToolchain { channel: RustChannel::Stable },
            display_name: "acquire toolchain".to_string(),
        });

        // Add registry crate acquisition actions
        let mut registry_nodes = HashMap::new();
        for registry_crate in graph.iter_registry_crates() {
            let registry_action = action_graph.add_node(ActionNode {
                action: Action::AcquireRegistryCrate {
                    name: registry_crate.name.clone(),
                    version: registry_crate.version.clone(),
                    checksum: registry_crate.checksum.clone(),
                },
                display_name: format!("acquire {}@{}", registry_crate.name, registry_crate.version),
            });
            registry_nodes.insert((registry_crate.name.clone(), registry_crate.version.clone()), registry_action);
        }

        // Add compile actions with dependencies on toolchain + registry crates
        for crate_node in graph.nodes.values() {
            let compile_action = action_graph.add_node(/* ... */);

            // Depend on toolchain
            action_graph.add_edge(compile_action, toolchain_action, ());

            // Depend on registry crate if applicable
            if let CrateSource::Registry { name, version, .. } = &crate_node.source {
                let registry_action = registry_nodes[&(name.clone(), version.clone())];
                action_graph.add_edge(compile_action, registry_action, ());
            }

            // Depend on other crate compilations
            for dep in &crate_node.deps {
                let dep_action = crate_to_node[&dep.crate_id];
                action_graph.add_edge(compile_action, dep_action, ());
            }
        }

        Self { graph: action_graph, crate_to_node }
    }
}
```

#### Step 2.2: Implement Action Execution

Update `execute_action()` to handle:
- `Action::AcquireToolchain` → call `ensure_rust_toolchain()`
- `Action::AcquireRegistryCrate` → call `acquire_single_registry_crate()`

---

### Phase 3: Dynamic Graph Expansion (Build Scripts)

Add support for actions that spawn new actions during execution

#### Step 3.1: Update ActionResult

```rust
enum ActionResult {
    // ... existing variants

    BuildScriptCompleted {
        // New actions discovered (e.g., C compile tasks)
        new_actions: Vec<(Action, Vec<NodeIndex>)>,  // (action, dependencies)
    },
}
```

#### Step 3.2: Handle Dynamic Expansion in Executor

```rust
impl Executor {
    async fn handle_completion(&mut self, completed_idx: NodeIndex) -> Result<()> {
        // Get result
        let result = self.results.read().await.get(&completed_idx).cloned();

        if let Some(ActionResult::BuildScriptCompleted { new_actions }) = result {
            // Add new nodes to graph
            let mut new_node_indices = Vec::new();
            {
                let mut graph = self.graph.write().await;
                for (action, deps) in new_actions {
                    let new_idx = graph.graph.add_node(ActionNode {
                        action,
                        display_name: /* ... */,
                    });

                    // Add edges to dependencies
                    for dep_idx in deps {
                        graph.graph.add_edge(new_idx, dep_idx, ());
                    }

                    new_node_indices.push(new_idx);
                }
            }

            // Initialize dependency counts for new nodes
            {
                let graph = self.graph.read().await;
                let mut remaining = self.remaining_deps.write().await;
                for idx in &new_node_indices {
                    let dep_count = graph.graph.neighbors(*idx).count();
                    remaining.insert(*idx, dep_count);
                }
            }

            // Find which new actions are ready immediately
            {
                let remaining = self.remaining_deps.read().await;
                let mut queue = self.ready_queue.write().await;
                for idx in new_node_indices {
                    if remaining[&idx] == 0 {
                        queue.push_back(idx);
                    }
                }
            }

            // Update TUI total
            let total = self.graph.read().await.graph.node_count();
            self.tui.set_total(total).await;
        }

        // ... rest of completion handling
    }
}
```

---

## Testing Strategy

1. **Unit tests** - Test ActionGraph construction from CrateGraph
2. **Integration tests** - Build small projects, verify parallelism
3. **Regression tests** - Ensure all existing test projects still build
4. **Performance tests** - Measure speedup on large dependency graphs

---

## Migration Path

1. ✅ **Phase 0** - Picante singleton refactor (makes concurrent builds safe)
2. ✅ **Phase 1** - Core action graph (replace sequential loop, Rust only)
3. **Phase 2** - Move toolchain/registry to actions (cleaner graph)
4. **Phase 3** - Dynamic expansion (build scripts, C deps)

Each phase is independently valuable and can be shipped incrementally.

---

## Dependencies

Add to `Cargo.toml`:
```toml
[dependencies]
petgraph = "0.6"
```

---

## Files to Create

- `crates/vx-aether/src/action_graph.rs` - Action graph data structures
- `crates/vx-aether/src/executor.rs` - Execution scheduler

## Files to Modify

- `crates/vx-aether/src/inputs.rs` - Add keys to RustToolchain, BuildConfig
- `crates/vx-aether/src/queries.rs` - Thread toolchain/config as parameters
- `crates/vx-aether/src/service.rs` - Replace `do_build()` loop with executor
- `crates/vx-aether/src/tests.rs` - Update to use `.new()` instead of `.set()`
- `crates/vx-aether/src/tui.rs` - Already supports concurrent actions, no changes
- `crates/vx-aether/Cargo.toml` - Add petgraph dependency

---

## Open Questions

1. **Edge direction in petgraph** - Should edges point from dependent → dependency, or dependency → dependent? Need to decide based on traversal patterns.

2. **Concurrency limit** - Start with `num_cpus * 2`? Make configurable?

3. **Error handling** - Should one failed action abort the build, or should we try to compile as much as possible?

4. **TUI updates** - Do we need to update TUI more frequently as graph changes? Or is current 15fps sufficient?

5. **Build script interception** - How exactly do we intercept `cc` crate calls? Hook into env vars? Patch the cc crate?

---

## Success Criteria

- [ ] Concurrent crate compilation (observed via TUI showing multiple active actions)
- [ ] Cache hits still work correctly
- [ ] Build times improve on projects with >10 crates
- [ ] All existing tests pass
- [ ] No regressions in incremental build behavior
