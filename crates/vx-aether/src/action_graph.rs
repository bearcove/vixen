//! Action graph for declarative build execution
//!
//! This module defines the action-based execution model that replaces the
//! imperative sequential loop. Actions are nodes in a dependency graph,
//! and the executor schedules them for optimal parallelism.

use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use vx_oort_proto::Blake3Hash;
use vx_rs::{CrateGraph, CrateId, CrateType};

use crate::inputs::{BuildConfig, RustCrate, RustToolchain};

/// A single unit of work in the build process
#[derive(Debug, Clone)]
pub enum Action {
    /// Compile a Rust crate (lib or bin)
    CompileRustCrate {
        crate_id: CrateId,
        crate_name: String,
        crate_type: CrateType,
        /// Picante input for this crate
        rust_crate: RustCrate,
        /// Toolchain to use
        toolchain: RustToolchain,
        /// Build configuration
        config: BuildConfig,
    },
}

impl Action {
    /// Get a display name for TUI tracking
    pub fn display_name(&self) -> String {
        match self {
            Action::CompileRustCrate { crate_name, .. } => {
                format!("compile {}", crate_name)
            }
        }
    }

    /// Convert to TUI ActionType
    pub fn to_tui_action_type(&self) -> crate::tui::ActionType {
        match self {
            Action::CompileRustCrate { crate_name, .. } => {
                crate::tui::ActionType::CompileRust(crate_name.clone())
            }
        }
    }
}

/// A node in the action graph
#[derive(Debug, Clone)]
pub struct ActionNode {
    /// The actual work to perform
    pub action: Action,
}

/// The action dependency graph
pub struct ActionGraph {
    /// Directed graph: edges point from dependent → dependency
    /// (i.e., if A depends on B, there's an edge A → B)
    pub graph: DiGraph<ActionNode, ()>,

    /// Map from CrateId to its action node (for resolving dependencies)
    pub crate_to_node: HashMap<CrateId, NodeIndex>,
}

impl ActionGraph {
    /// Build action graph from a CrateGraph
    ///
    /// This creates one CompileRustCrate action per crate and connects
    /// them based on the crate dependency edges.
    pub fn from_crate_graph(
        crate_graph: &CrateGraph,
        toolchain: RustToolchain,
        config: BuildConfig,
        db: &crate::db::Database,
    ) -> Result<Self, crate::error::AetherError> {
        let mut graph = DiGraph::new();
        let mut crate_to_node = HashMap::new();

        // Create action nodes for all crates
        for crate_node in crate_graph.nodes.values() {
            // Create RustCrate input
            let crate_id_hex = crate_node.id.short_hex();
            let rust_crate = RustCrate::new(
                db,
                crate_id_hex.clone(),
                crate_node.crate_name.clone(),
                crate_node.edition.as_str().to_string(),
                crate_node.crate_type.as_str().to_string(),
                crate_node.crate_root_rel.to_string(),
                Blake3Hash([0u8; 32]), // Placeholder, will be filled during execution
            )
            .map_err(|e| crate::error::AetherError::Picante(e.to_string()))?;

            let action = Action::CompileRustCrate {
                crate_id: crate_node.id,
                crate_name: crate_node.crate_name.clone(),
                crate_type: crate_node.crate_type,
                rust_crate,
                toolchain,
                config,
            };

            let node_idx = graph.add_node(ActionNode { action });
            crate_to_node.insert(crate_node.id, node_idx);
        }

        // Add dependency edges
        // Edge from dependent → dependency means "dependent needs dependency to complete first"
        for crate_node in crate_graph.nodes.values() {
            let dependent_idx = crate_to_node[&crate_node.id];

            for dep in &crate_node.deps {
                if let Some(&dependency_idx) = crate_to_node.get(&dep.crate_id) {
                    graph.add_edge(dependent_idx, dependency_idx, ());
                }
            }
        }

        Ok(Self { graph, crate_to_node })
    }

    /// Get total number of actions
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }
}
