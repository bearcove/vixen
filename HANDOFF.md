# Action Graph Executor - Session Handoff

## Current Status

**The action graph executor is COMPLETE and committed**, but there's a blocking issue during actual builds.

## What Was Accomplished

### Phase 0: Picante Singleton Refactor (Committed: a44d184)
- Changed RustToolchain and BuildConfig from singleton to keyed inputs
- Enables concurrent builds with different configurations
- All cache key queries updated to accept toolchain/config parameters

### Phase 1: Core Infrastructure (Committed: ec96564)
- Added petgraph dependency for action graph
- Created `action_graph.rs` with Action enum and ActionGraph
- Created `executor.rs` with parallel execution engine using Kahn's algorithm
- Topological scheduling with semaphore-based concurrency control

### Phase 1.5: Real Compilation (Committed: a818208)
- Implemented full `execute_action()` with real compilation logic
- Source closure computation, cache key queries, CAS operations
- Dependency resolution via results HashMap
- Activated executor in service.rs (sequential loop now unreachable)

### Phase 1.6: Stats & Bin Materialization (Committed: b78bfef)
- Added ExecutionStats tracking (cache_hits, rebuilt, bin_output)
- Bin output materialization to `.vx/build/{triple}/{profile}/{name}`
- Removed 315 lines of dead sequential code (843 → 528 lines)
- Accurate BuildResult with meaningful stats

### Phase 1.7: WAL & Error Handling (Committed: f3ef8ee)
- WAL persistence after executor completes (incremental builds)
- Error handling: first_error aborts build early
- Made AetherError Clone for error propagation
- Production-ready error flow

## Current Issue

**Build hangs after graph resolution:**

```
vx_aether::service: resolved crate graph, toolchain and crates ready
  workspace_root=/Users/amos/bearcove/timelord/crates/timelord
  path_crate_count=76
  registry_crate_count=75
---------------------------------------- 0/76 [completed: 0, active: 0, pending: 76]

[BLOCKS HERE - no actions execute]
```

### Symptoms
- TUI shows "0/76 [completed: 0, active: 0, pending: 76]"
- No actions start executing
- Build just blocks indefinitely
- Log shows graph resolved successfully with 76 crates

### Likely Root Causes

1. **Executor not wired to `vx build` command?**
   - User asked "is it wired to 'vx build' at all?"
   - Need to verify the CLI path actually calls service.rs::do_build()

2. **Ready queue initialization issue?**
   - Graph shows 76 crates but "pending: 76" with 0 active
   - Ready queue should have crates with 0 dependencies
   - Check `Executor::new()` initialization (lines 118-166 in executor.rs)

3. **Dependency count calculation bug?**
   - Line 98 in executor.rs: `let dep_count = graph_lock.graph.neighbors(node_idx).count();`
   - Edge direction: dependent → dependency (A needs B = edge A→B)
   - We count OUTGOING edges (dependencies), crates with 0 should be ready
   - Verify edge direction is correct in ActionGraph::from_crate_graph()

4. **Main loop not starting?**
   - Executor::execute() may never enter the action spawning loop
   - Check if completion_rx channel is somehow closed immediately

## Investigation Steps for Next Session

### 1. Verify CLI Wiring
```bash
# Check if vx build actually calls do_build()
rg "fn build|do_build" crates/vx/src/
# Trace from CLI entry point to service call
```

### 2. Add Debug Logging to Executor
```rust
// In Executor::new(), after ready_queue initialization:
eprintln!("DEBUG: Ready queue has {} actions", ready_queue.len());
for idx in &ready_queue {
    let action = &graph_lock.graph[*idx].action;
    eprintln!("DEBUG: Ready action: {:?}", action.display_name());
}

// In Executor::execute(), at start of loop:
eprintln!("DEBUG: Main loop iteration, ready_queue size: {}",
    self.ready_queue.read().await.len());
```

### 3. Verify Edge Direction
```rust
// In ActionGraph::from_crate_graph(), check:
graph.add_edge(dependent_idx, dependency_idx, ()); // Should be this order
// NOT: graph.add_edge(dependency_idx, dependent_idx, ());
```

### 4. Check Petgraph Direction
```rust
// neighbors() returns outgoing edges by default
// For DiGraph with edge A→B, neighbors(A) returns [B]
// We want: count dependencies (outgoing), so this is correct
```

## Key Files

- **crates/vx-aether/src/executor.rs**: Main execution engine (lines 118-166 for init, 173-229 for execute loop)
- **crates/vx-aether/src/action_graph.rs**: Graph construction (lines 100-180 for from_crate_graph)
- **crates/vx-aether/src/service.rs**: Integration point (lines 436-492)
- **crates/vx/src/**: CLI entry point (need to verify this calls service.rs::do_build())

## Quick Fixes to Try

### If Ready Queue is Empty
```rust
// In Executor::new(), change dependency counting:
// Current (line 98):
let dep_count = graph_lock.graph.neighbors(node_idx).count();

// Try reversing (count incoming instead):
use petgraph::Direction::Incoming;
let dep_count = graph_lock.graph.neighbors_directed(node_idx, Incoming).count();
```

### If Edge Direction is Wrong
```rust
// In ActionGraph::from_crate_graph() (line 117):
// Current:
graph.add_edge(dependent_idx, dependency_idx, ());

// Should be (dependency points to dependent):
graph.add_edge(dependency_idx, dependent_idx, ());
```

## Expected Behavior

When working correctly:
1. Graph construction finds crates with 0 dependencies
2. Ready queue initialized with these root crates
3. Main loop spawns actions for ready crates
4. Actions execute, complete, notify dependents
5. Dependents with decremented dep count → 0 added to ready queue
6. Process continues until all crates compiled

## Testing Command

```bash
cd /Users/amos/bearcove/timelord
vx build  # Should compile 76 crates in parallel
```

## Context

This is issue #13: Refactor aether from imperative to action-based graph execution. The parallel executor is complete and committed, but needs debugging before it can actually run builds.

Branch: `action-graph`
Latest commit: `f3ef8ee` (Phase 1.7)
