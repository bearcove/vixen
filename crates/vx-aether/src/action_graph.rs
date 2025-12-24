//! Action graph for declarative build execution
//!
//! This module defines the action-based execution model that replaces the
//! imperative sequential loop. Actions are nodes in a dependency graph,
//! and the executor schedules them for optimal parallelism.

use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use vx_oort_proto::{Blake3Hash, RustChannel};
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
    /// Acquire Rust toolchain from static.rust-lang.org
    AcquireToolchain {
        /// Rust channel (Stable, Beta, Nightly)
        channel: RustChannel,
        /// Target triple
        target_triple: String,
    },

    /// Acquire registry crate from crates.io
    AcquireRegistryCrate {
        /// Crate name
        name: String,
        /// Crate version
        version: String,
        /// Checksum for verification
        checksum: String,
    },

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
            Action::AcquireToolchain { channel, .. } => {
                format!("acquire toolchain {:?}", channel)
            }
            Action::AcquireRegistryCrate { name, version, .. } => {
                format!("acquire {}@{}", name, version)
            }
            Action::CompileRustCrate { crate_name, .. } => {
                format!("compile {}", crate_name)
            }
        }
    }

    /// Convert to TUI ActionType
    pub fn to_tui_action_type(&self) -> crate::tui::ActionType {
        match self {
            Action::AcquireToolchain { .. } => {
                crate::tui::ActionType::AcquireToolchain
            }
            Action::AcquireRegistryCrate { name, version, .. } => {
                crate::tui::ActionType::AcquireRegistryCrate(name.clone(), version.clone())
            }
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

    /// Toolchain acquisition node (all compilations depend on this)
    pub toolchain_node: Option<NodeIndex>,

    /// Map from (name, version) to registry crate acquisition node
    pub registry_nodes: HashMap<(String, String), NodeIndex>,
}

impl ActionGraph {
    /// Build action graph from a CrateGraph
    ///
    /// This creates AcquireToolchain, AcquireRegistryCrate, and CompileRustCrate
    /// actions with proper dependency edges.
    pub fn from_crate_graph(
        crate_graph: &CrateGraph,
        rust_channel: RustChannel,
        target_triple: String,
        toolchain: RustToolchain,
        config: BuildConfig,
        db: &crate::db::Database,
    ) -> Result<Self, crate::error::AetherError> {
        use tracing::debug;

        debug!("Building action graph from crate graph");
        let mut graph = DiGraph::new();
        let mut crate_to_node = HashMap::new();
        let mut registry_nodes = HashMap::new();

        // 1. Create toolchain acquisition action (all compilations depend on this)
        debug!("Creating toolchain acquisition action");
        let toolchain_node = graph.add_node(ActionNode {
            action: Action::AcquireToolchain {
                channel: rust_channel,
                target_triple: target_triple.clone(),
            },
        });
        debug!(node = ?toolchain_node, "Toolchain node created");

        // 2. Create registry crate acquisition actions
        debug!(registry_count = crate_graph.iter_registry_crates().count(), "Creating registry acquisition actions");
        for (i, registry_crate) in crate_graph.iter_registry_crates().enumerate() {
            let registry_action = graph.add_node(ActionNode {
                action: Action::AcquireRegistryCrate {
                    name: registry_crate.name.clone(),
                    version: registry_crate.version.clone(),
                    checksum: registry_crate.checksum.clone(),
                },
            });
            registry_nodes.insert(
                (registry_crate.name.clone(), registry_crate.version.clone()),
                registry_action,
            );
            if (i + 1) % 10 == 0 {
                debug!(created = i + 1, "Registry nodes created");
            }
        }
        debug!(total = registry_nodes.len(), "All registry nodes created");

        // 3. Create compilation action nodes for all crates
        debug!(crate_count = crate_graph.nodes.len(), "Creating compilation actions");
        for (i, crate_node) in crate_graph.nodes.values().enumerate() {
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

            if (i + 1) % 10 == 0 {
                debug!(created = i + 1, "Compilation nodes created");
            }
        }
        debug!(total = crate_to_node.len(), "All compilation nodes created");

        // 4. Add dependency edges
        debug!("Adding dependency edges");
        // Edge from dependent → dependency means "dependent needs dependency to complete first"
        for crate_node in crate_graph.nodes.values() {
            let compile_idx = crate_to_node[&crate_node.id];

            // Every compilation depends on toolchain acquisition
            graph.add_edge(compile_idx, toolchain_node, ());

            // Registry crates also depend on their registry acquisition
            if let CrateSource::Registry { name, version, .. } = &crate_node.source {
                if let Some(&registry_idx) = registry_nodes.get(&(name.clone(), version.clone())) {
                    graph.add_edge(compile_idx, registry_idx, ());
                }
            }

            // Add edges to crate dependencies
            for dep in &crate_node.deps {
                if let Some(&dep_compile_idx) = crate_to_node.get(&dep.crate_id) {
                    graph.add_edge(compile_idx, dep_compile_idx, ());
                }
            }
        }

        debug!(
            nodes = graph.node_count(),
            edges = graph.edge_count(),
            "Action graph construction complete"
        );

        Ok(Self {
            graph,
            crate_to_node,
            toolchain_node: Some(toolchain_node),
            registry_nodes,
        })
    }

    /// Get total number of actions
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Dump graph structure for debugging
    pub fn dump_graph(&self) {
        use petgraph::Direction::Incoming;
        use tracing::info;

        // Collect nodes with their dependency counts
        let mut nodes_info: Vec<_> = self.graph.node_indices().map(|idx| {
            let node = &self.graph[idx];
            let outgoing = self.graph.neighbors(idx).count();
            let incoming = self.graph.neighbors_directed(idx, Incoming).count();
            (idx, node, outgoing, incoming)
        }).collect();

        // Count ready nodes
        let ready_count = nodes_info.iter().filter(|(_, _, out, _)| *out == 0).count();

        info!(
            nodes = self.graph.node_count(),
            edges = self.graph.edge_count(),
            ready_nodes = ready_count,
            "Graph summary"
        );

        // Show first few ready nodes as examples
        let ready_examples: Vec<_> = nodes_info.iter()
            .filter(|(_, _, out, _)| *out == 0)
            .take(3)
            .collect();

        for (idx, node, outgoing, incoming) in ready_examples {
            info!(
                node = ?idx,
                action = node.action.display_name(),
                incoming = incoming,
                "Example ready node"
            );
        }

        // Show first few non-ready nodes with their deps
        let blocked_examples: Vec<_> = nodes_info.iter()
            .filter(|(_, _, out, _)| *out > 0)
            .take(2)
            .collect();

        for (idx, node, outgoing, _incoming) in blocked_examples {
            let deps: Vec<_> = self.graph.neighbors(*idx)
                .take(3)
                .map(|dep_idx| self.graph[dep_idx].action.display_name())
                .collect();
            info!(
                node = ?idx,
                action = node.action.display_name(),
                dep_count = outgoing,
                deps = ?deps,
                "Example blocked node"
            );
        }
    }
}
