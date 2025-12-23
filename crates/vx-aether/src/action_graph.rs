//! Action graph for declarative build execution
//!
//! This module defines the action-based execution model that replaces the
//! imperative sequential loop. Actions are nodes in a dependency graph,
//! and the executor schedules them for optimal parallelism.

use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use vx_oort_proto::Blake3Hash;
use vx_rs::crate_graph::{CrateGraph, CrateId, CrateSource, CrateType};

use crate::inputs::{BuildConfig, RustCrate, RustToolchain};

/// Dependency with source information (for building RustDep at execution time)
#[derive(Debug, Clone)]
pub struct ActionDep {
    /// The extern crate name
    pub extern_name: String,
    /// CrateId of the dependency
    pub crate_id: CrateId,
    /// Source of the dependency (to determine if registry_crate_manifest is needed)
    pub source: CrateSource,
}

/// A single unit of work in the build process
#[derive(Debug, Clone)]
pub enum Action {
    /// Compile a Rust crate (lib or bin)
    CompileRustCrate {
        crate_id: CrateId,
        crate_name: String,
        crate_type: CrateType,
        /// Rust edition
        edition: String,
        /// Crate root path (workspace-relative)
        crate_root_rel: String,
        /// Source type (Path or Registry)
        source: CrateSource,
        /// Dependencies this crate needs (with source info for RustDep construction)
        deps: Vec<ActionDep>,
        /// Workspace root path
        workspace_root: String,
        /// Target triple
        target_triple: String,
        /// Build profile
        profile: String,
        /// Toolchain manifest hash
        toolchain_manifest: Blake3Hash,
        /// Picante input for this crate (for cache key computation)
        rust_crate: RustCrate,
        /// Toolchain to use (for cache key computation)
        toolchain: RustToolchain,
        /// Build configuration (for cache key computation)
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

            // Build ActionDep list with source information
            let action_deps: Vec<ActionDep> = crate_node
                .deps
                .iter()
                .map(|dep| {
                    let dep_node = &crate_graph.nodes[&dep.crate_id];
                    ActionDep {
                        extern_name: dep.extern_name.clone(),
                        crate_id: dep.crate_id,
                        source: dep_node.source.clone(),
                    }
                })
                .collect();

            let target_triple_str = toolchain.target(db).map_err(|e| crate::error::AetherError::Picante(e.to_string()))?;
            let profile_str = config.profile(db).map_err(|e| crate::error::AetherError::Picante(e.to_string()))?;
            let manifest_hash = toolchain.toolchain_manifest(db).map_err(|e| crate::error::AetherError::Picante(e.to_string()))?;

            let action = Action::CompileRustCrate {
                crate_id: crate_node.id,
                crate_name: crate_node.crate_name.clone(),
                crate_type: crate_node.crate_type,
                edition: crate_node.edition.as_str().to_string(),
                crate_root_rel: crate_node.crate_root_rel.to_string(),
                source: crate_node.source.clone(),
                deps: action_deps,
                workspace_root: crate_graph.workspace_root.to_string(),
                target_triple: target_triple_str,
                profile: profile_str,
                toolchain_manifest: manifest_hash,
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
