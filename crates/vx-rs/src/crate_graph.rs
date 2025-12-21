//! Crate graph resolution for multi-crate Rust builds
//!
//! This module builds a dependency graph from Cargo.toml manifests,
//! computing the workspace root as the Lowest Common Ancestor (LCA)
//! of all crates in the graph.
//!
//! ## Key Design Decisions
//!
//! 1. **Workspace Root = LCA of all crates**: Not the directory where `vx build`
//!    runs, but the common ancestor of all crates in the resolved graph.
//!    This ensures paths are stable across different invocation directories.
//!
//! 2. **CrateId uses normalized workspace-relative path**: Paths are canonicalized
//!    with forward slashes, no `./`, no `..`. This makes CrateIds stable across
//!    "same repo, different invocation dir" scenarios.
//!
//! 3. **All paths are workspace-relative**: No absolute paths leak into cache keys
//!    or rustc invocations.

use std::collections::{HashMap, HashSet, VecDeque};

use camino::{Utf8Path, Utf8PathBuf};
use thiserror::Error;
use vx_cas_proto::Blake3Hash;
use vx_manifest::{Manifest, ManifestError};

/// Errors during crate graph construction
#[derive(Debug, Error)]
pub enum CrateGraphError {
    #[error("failed to parse manifest: {0}")]
    ManifestError(#[from] ManifestError),

    #[error("failed to canonicalize path {path}: {source}")]
    CanonicalizationError {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("dependency cycle detected: {0}")]
    CycleDetected(String),

    #[error("dependency not found: {name} at path {path}")]
    DependencyNotFound { name: String, path: Utf8PathBuf },

    #[error("crate has no lib target but is used as dependency: {name}")]
    NotALibrary { name: String },

    #[error("path {path} is not under workspace root {workspace_root}")]
    PathOutsideWorkspace {
        path: Utf8PathBuf,
        workspace_root: Utf8PathBuf,
    },
}

/// Type of crate target
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrateType {
    Lib,
    Bin,
}

impl CrateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CrateType::Lib => "lib",
            CrateType::Bin => "bin",
        }
    }
}

/// A dependency edge in the crate graph
#[derive(Debug, Clone)]
pub struct DepEdge {
    /// The extern crate name (used in `--extern name=path`)
    pub extern_name: String,
    /// CrateId of the dependency
    pub crate_id: CrateId,
}

/// Unique identifier for a crate in the graph
///
/// This is a blake3 hash of the normalized workspace-relative path
/// plus the package name. This ensures stability across different
/// checkout locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CrateId(pub Blake3Hash);

impl CrateId {
    /// Create a CrateId from workspace-relative path and package name
    pub fn new(workspace_rel: &Utf8Path, package_name: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crate_id_v1:");
        hasher.update(workspace_rel.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(package_name.as_bytes());
        CrateId(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    /// Get a short hex representation for display/paths
    pub fn short_hex(&self) -> String {
        hex::encode(&self.0.0[..8])
    }
}

impl std::fmt::Display for CrateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short_hex())
    }
}

/// A node in the crate graph
#[derive(Debug, Clone)]
pub struct CrateNode {
    /// Unique identifier
    pub id: CrateId,
    /// Package name from Cargo.toml
    pub package_name: String,
    /// Crate name (package_name with '-' -> '_')
    pub crate_name: String,
    /// Path to crate directory, relative to workspace root (no `..`)
    pub workspace_rel: Utf8PathBuf,
    /// Path to crate root file, relative to workspace root (no `..`)
    pub crate_root_rel: Utf8PathBuf,
    /// Rust edition
    pub edition: vx_manifest::Edition,
    /// Whether this is a lib or bin crate
    pub crate_type: CrateType,
    /// Dependencies, sorted by extern_name
    pub deps: Vec<DepEdge>,
}

/// The complete crate dependency graph
#[derive(Debug)]
pub struct CrateGraph {
    /// Computed workspace root (LCA of all crates, absolute path)
    pub workspace_root: Utf8PathBuf,
    /// All crates in the graph
    pub nodes: HashMap<CrateId, CrateNode>,
    /// Topological order: dependencies before dependents
    pub topo_order: Vec<CrateId>,
    /// The crate the user invoked build on
    pub root_crate: CrateId,
}

impl CrateGraph {
    /// Build a crate graph starting from the given directory.
    ///
    /// This parses the Cargo.toml in `invocation_dir`, recursively resolves
    /// all path dependencies, computes the LCA workspace root, and produces
    /// a topological ordering for builds.
    pub fn build(invocation_dir: &Utf8Path) -> Result<Self, CrateGraphError> {
        // Canonicalize the starting directory
        let invocation_dir = invocation_dir.canonicalize_utf8().map_err(|e| {
            CrateGraphError::CanonicalizationError {
                path: invocation_dir.to_owned(),
                source: e,
            }
        })?;

        // Phase 1: Collect all crate directories (absolute paths)
        let mut all_crate_dirs: Vec<Utf8PathBuf> = Vec::new();
        let mut manifests: HashMap<Utf8PathBuf, Manifest> = HashMap::new();
        let mut to_visit: VecDeque<Utf8PathBuf> = VecDeque::new();
        let mut visited: HashSet<Utf8PathBuf> = HashSet::new();

        to_visit.push_back(invocation_dir.clone());

        while let Some(crate_dir) = to_visit.pop_front() {
            if visited.contains(&crate_dir) {
                continue;
            }
            visited.insert(crate_dir.clone());
            all_crate_dirs.push(crate_dir.clone());

            // Parse manifest
            let manifest_path = crate_dir.join("Cargo.toml");
            let manifest = Manifest::from_path(&manifest_path)?;

            // Queue dependencies
            for dep in &manifest.deps {
                let dep_dir = crate_dir.join(&dep.path);
                let dep_dir = dep_dir.canonicalize_utf8().map_err(|e| {
                    CrateGraphError::CanonicalizationError {
                        path: dep_dir.clone(),
                        source: e,
                    }
                })?;
                if !visited.contains(&dep_dir) {
                    to_visit.push_back(dep_dir);
                }
            }

            manifests.insert(crate_dir, manifest);
        }

        // Phase 2: Compute LCA workspace root
        let workspace_root = compute_lca(&all_crate_dirs);

        // Phase 3: Build nodes with workspace-relative paths
        let mut nodes: HashMap<CrateId, CrateNode> = HashMap::new();
        let mut dir_to_id: HashMap<Utf8PathBuf, CrateId> = HashMap::new();

        for (crate_dir, manifest) in &manifests {
            let workspace_rel = normalize_path(crate_dir, &workspace_root)?;

            // Determine crate type and root file
            let (crate_type, crate_root_abs) = if manifest.is_bin() {
                // Prefer bin target
                let bin = manifest.bin.as_ref().unwrap();
                (CrateType::Bin, bin.path.clone())
            } else if manifest.is_lib() {
                let lib = manifest.lib.as_ref().unwrap();
                (CrateType::Lib, lib.path.clone())
            } else {
                // Shouldn't happen - manifest parsing requires at least one target
                unreachable!("manifest has no targets");
            };

            // Make crate root workspace-relative
            let crate_root_rel = normalize_path(&crate_root_abs, &workspace_root)?;

            let id = CrateId::new(&workspace_rel, &manifest.name);

            let node = CrateNode {
                id,
                package_name: manifest.name.clone(),
                crate_name: manifest.crate_name(),
                workspace_rel,
                crate_root_rel,
                edition: manifest.edition,
                crate_type,
                deps: Vec::new(), // Filled in next phase
            };

            dir_to_id.insert(crate_dir.clone(), id);
            nodes.insert(id, node);
        }

        // Phase 4: Resolve dependency edges
        for (crate_dir, manifest) in &manifests {
            let crate_id = dir_to_id[crate_dir];

            let mut deps: Vec<DepEdge> = Vec::new();

            for dep in &manifest.deps {
                let dep_dir = crate_dir.join(&dep.path).canonicalize_utf8().map_err(|e| {
                    CrateGraphError::CanonicalizationError {
                        path: crate_dir.join(&dep.path),
                        source: e,
                    }
                })?;

                let dep_id =
                    dir_to_id
                        .get(&dep_dir)
                        .ok_or_else(|| CrateGraphError::DependencyNotFound {
                            name: dep.name.clone(),
                            path: dep_dir.clone(),
                        })?;

                // Verify the dependency is a library
                let dep_node = &nodes[dep_id];
                if dep_node.crate_type != CrateType::Lib {
                    return Err(CrateGraphError::NotALibrary {
                        name: dep.name.clone(),
                    });
                }

                deps.push(DepEdge {
                    extern_name: dep.name.replace('-', "_"),
                    crate_id: *dep_id,
                });
            }

            // Sort deps by extern_name for determinism
            deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));

            nodes.get_mut(&crate_id).unwrap().deps = deps;
        }

        // Phase 5: Topological sort
        let root_crate = dir_to_id[&invocation_dir];
        let topo_order = topological_sort(&nodes, root_crate)?;

        Ok(CrateGraph {
            workspace_root,
            nodes,
            topo_order,
            root_crate,
        })
    }

    /// Get a node by its ID
    pub fn get(&self, id: CrateId) -> Option<&CrateNode> {
        self.nodes.get(&id)
    }

    /// Iterate over crates in topological order (dependencies first)
    pub fn iter_topo(&self) -> impl Iterator<Item = &CrateNode> {
        self.topo_order.iter().map(|id| &self.nodes[id])
    }

    /// Get the root crate node
    pub fn root(&self) -> &CrateNode {
        &self.nodes[&self.root_crate]
    }
}

/// Compute the Lowest Common Ancestor of a set of paths.
///
/// All paths must be absolute. Returns the deepest directory that is
/// a prefix of all input paths.
fn compute_lca(paths: &[Utf8PathBuf]) -> Utf8PathBuf {
    if paths.is_empty() {
        return Utf8PathBuf::from("/");
    }

    if paths.len() == 1 {
        return paths[0].clone();
    }

    // Split all paths into components
    let component_lists: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.components().map(|c| c.as_str()).collect())
        .collect();

    // Find common prefix length
    let min_len = component_lists.iter().map(|c| c.len()).min().unwrap_or(0);

    let mut common_len = 0;
    for i in 0..min_len {
        let first = component_lists[0][i];
        if component_lists.iter().all(|c| c[i] == first) {
            common_len = i + 1;
        } else {
            break;
        }
    }

    // Reconstruct path from common components
    if common_len == 0 {
        Utf8PathBuf::from("/")
    } else {
        let components: Vec<&str> = component_lists[0][..common_len].to_vec();
        Utf8PathBuf::from(components.join("/"))
    }
}

/// Normalize a path to be relative to the workspace root.
///
/// The result has no `..` components and uses forward slashes.
fn normalize_path(
    path: &Utf8Path,
    workspace_root: &Utf8Path,
) -> Result<Utf8PathBuf, CrateGraphError> {
    // Canonicalize if not already
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        path.canonicalize_utf8()
            .map_err(|e| CrateGraphError::CanonicalizationError {
                path: path.to_owned(),
                source: e,
            })?
    };

    // Strip workspace root prefix
    path.strip_prefix(workspace_root)
        .map(Utf8PathBuf::from)
        .map_err(|_| CrateGraphError::PathOutsideWorkspace {
            path: path.clone(),
            workspace_root: workspace_root.to_owned(),
        })
}

/// Perform a topological sort of crates reachable from root_crate.
///
/// Returns crates in dependency order: dependencies before dependents.
fn topological_sort(
    nodes: &HashMap<CrateId, CrateNode>,
    root_crate: CrateId,
) -> Result<Vec<CrateId>, CrateGraphError> {
    // Kahn's algorithm with cycle detection
    let mut in_degree: HashMap<CrateId, usize> = HashMap::new();
    let mut reachable: HashSet<CrateId> = HashSet::new();

    // First, find all reachable nodes from root
    let mut queue: VecDeque<CrateId> = VecDeque::new();
    queue.push_back(root_crate);

    while let Some(id) = queue.pop_front() {
        if reachable.contains(&id) {
            continue;
        }
        reachable.insert(id);

        if let Some(node) = nodes.get(&id) {
            for dep in &node.deps {
                queue.push_back(dep.crate_id);
            }
        }
    }

    // Initialize in-degrees for reachable nodes
    for &id in &reachable {
        in_degree.entry(id).or_insert(0);
    }

    // Count incoming edges
    for &id in &reachable {
        if let Some(node) = nodes.get(&id) {
            for dep in &node.deps {
                if reachable.contains(&dep.crate_id) {
                    *in_degree.entry(dep.crate_id).or_insert(0) += 1;
                }
            }
        }
    }

    // Wait, this is backwards. We want deps BEFORE dependents.
    // A crate with deps should have higher "level" than its deps.
    // Let's recalculate: in_degree should count how many crates depend ON this crate.

    // Actually, let's think again:
    // - We want to output crates so that all dependencies come before dependents
    // - If A depends on B, then B should appear before A
    // - In a standard topo sort of a DAG where edges go from dependent to dependency,
    //   we want to output in reverse order of DFS finish times, or use in-degree
    //   counting where in_degree counts incoming edges from dependencies.
    //
    // Let's define: edge A -> B means "A depends on B"
    // We want B before A in the output.
    // So we count how many nodes point TO each node (in-degree in reversed graph).

    // Reset and recalculate correctly
    let mut in_degree: HashMap<CrateId, usize> = HashMap::new();
    for &id in &reachable {
        in_degree.insert(id, 0);
    }

    // For each node, count how many dependents it has (reversed perspective)
    // Actually no - let's use the standard approach:
    // Edge: A --depends_on--> B
    // We want to process B before A.
    // In standard topo sort, we count in-degree as "how many edges point to me".
    // Here, if A depends on B, the edge is A -> B.
    // So B has in-degree 1 from A.
    // Nodes with in-degree 0 have no dependents and can be processed last.
    // That's wrong.
    //
    // Let me think differently:
    // We want to output leaves first (nodes with no deps) and work up.
    // A node can be output when all its deps have been output.
    //
    // So: count for each node how many of its deps remain unprocessed.
    // When that count reaches 0, the node is ready.

    let mut remaining_deps: HashMap<CrateId, usize> = HashMap::new();
    for &id in &reachable {
        if let Some(node) = nodes.get(&id) {
            remaining_deps.insert(id, node.deps.len());
        } else {
            remaining_deps.insert(id, 0);
        }
    }

    // Start with nodes that have no dependencies
    let mut ready: VecDeque<CrateId> = remaining_deps
        .iter()
        .filter(|&(_, count)| *count == 0)
        .map(|(&id, _)| id)
        .collect();

    // Sort for determinism
    let mut ready_vec: Vec<_> = ready.drain(..).collect();
    ready_vec.sort_by_key(|id| nodes.get(id).map(|n| n.package_name.clone()));
    ready.extend(ready_vec);

    let mut result: Vec<CrateId> = Vec::new();

    while let Some(id) = ready.pop_front() {
        result.push(id);

        // For each node that depends on this one, decrement their remaining count
        for &other_id in &reachable {
            if let Some(node) = nodes.get(&other_id) {
                if node.deps.iter().any(|d| d.crate_id == id) {
                    let count = remaining_deps.get_mut(&other_id).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        ready.push_back(other_id);
                    }
                }
            }
        }
    }

    // Check for cycles
    if result.len() != reachable.len() {
        // Find a node in a cycle for error message
        let in_cycle: Vec<_> = reachable.iter().filter(|id| !result.contains(id)).collect();
        let names: Vec<_> = in_cycle
            .iter()
            .filter_map(|id| nodes.get(id).map(|n| n.package_name.as_str()))
            .collect();
        return Err(CrateGraphError::CycleDetected(names.join(" -> ")));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crate_id_deterministic() {
        let id1 = CrateId::new(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::new(Utf8Path::new("app"), "my-app");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_crate_id_different_paths() {
        let id1 = CrateId::new(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::new(Utf8Path::new("other"), "my-app");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_crate_id_different_names() {
        let id1 = CrateId::new(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::new(Utf8Path::new("app"), "other-app");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_compute_lca_single() {
        let paths = vec![Utf8PathBuf::from("/home/user/project/app")];
        let lca = compute_lca(&paths);
        assert_eq!(lca, Utf8PathBuf::from("/home/user/project/app"));
    }

    #[test]
    fn test_compute_lca_siblings() {
        let paths = vec![
            Utf8PathBuf::from("/home/user/project/app"),
            Utf8PathBuf::from("/home/user/project/util"),
        ];
        let lca = compute_lca(&paths);
        assert_eq!(lca, Utf8PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_compute_lca_nested() {
        let paths = vec![
            Utf8PathBuf::from("/home/user/project/app"),
            Utf8PathBuf::from("/home/user/project/libs/util"),
            Utf8PathBuf::from("/home/user/project/libs/common"),
        ];
        let lca = compute_lca(&paths);
        assert_eq!(lca, Utf8PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_compute_lca_deeply_nested() {
        let paths = vec![
            Utf8PathBuf::from("/a/b/c/d/e"),
            Utf8PathBuf::from("/a/b/c/x/y"),
        ];
        let lca = compute_lca(&paths);
        assert_eq!(lca, Utf8PathBuf::from("/a/b/c"));
    }

    #[test]
    fn test_normalize_path() {
        let workspace = Utf8PathBuf::from("/home/user/project");
        let path = Utf8PathBuf::from("/home/user/project/app/src/main.rs");
        let normalized = normalize_path(&path, &workspace).unwrap();
        assert_eq!(normalized, Utf8PathBuf::from("app/src/main.rs"));
    }

    #[test]
    fn test_normalize_path_outside_workspace() {
        let workspace = Utf8PathBuf::from("/home/user/project");
        let path = Utf8PathBuf::from("/home/other/file.rs");
        let err = normalize_path(&path, &workspace).unwrap_err();
        assert!(matches!(err, CrateGraphError::PathOutsideWorkspace { .. }));
    }

    /// Helper to create temp directory structure for testing
    fn setup_multi_crate(files: &[(&str, &str)]) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();

        for (path, content) in files {
            let full_path = base.join(path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full_path, content).unwrap();
        }

        (dir, base)
    }

    #[test]
    fn test_single_crate_graph() {
        let (_dir, base) = setup_multi_crate(&[
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
edition = "2021"
"#,
            ),
            ("app/src/main.rs", "fn main() {}"),
        ]);

        let graph = CrateGraph::build(&base.join("app")).unwrap();

        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.topo_order.len(), 1);

        let root = graph.root();
        assert_eq!(root.package_name, "app");
        assert_eq!(root.crate_name, "app");
        assert_eq!(root.crate_type, CrateType::Bin);
        assert!(root.deps.is_empty());
    }

    #[test]
    fn test_two_crate_graph() {
        let (_dir, base) = setup_multi_crate(&[
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
edition = "2021"

[dependencies]
util = { path = "../util" }
"#,
            ),
            ("app/src/main.rs", "fn main() {}"),
            (
                "util/Cargo.toml",
                r#"
[package]
name = "util"
edition = "2021"
"#,
            ),
            ("util/src/lib.rs", "pub fn greet() {}"),
        ]);

        let graph = CrateGraph::build(&base.join("app")).unwrap();

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.topo_order.len(), 2);

        // Workspace root should be the parent (LCA of app and util)
        assert_eq!(graph.workspace_root, base.canonicalize_utf8().unwrap());

        // Check root crate
        let root = graph.root();
        assert_eq!(root.package_name, "app");
        assert_eq!(root.deps.len(), 1);
        assert_eq!(root.deps[0].extern_name, "util");

        // Check dependency
        let util_id = root.deps[0].crate_id;
        let util = graph.get(util_id).unwrap();
        assert_eq!(util.package_name, "util");
        assert_eq!(util.crate_type, CrateType::Lib);

        // Topo order: util before app
        assert_eq!(graph.topo_order[0], util_id);
        assert_eq!(graph.topo_order[1], graph.root_crate);
    }

    #[test]
    fn test_diamond_dependency() {
        // app depends on both foo and bar
        // both foo and bar depend on common
        let (_dir, base) = setup_multi_crate(&[
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
edition = "2021"

[dependencies]
foo = { path = "../foo" }
bar = { path = "../bar" }
"#,
            ),
            ("app/src/main.rs", "fn main() {}"),
            (
                "foo/Cargo.toml",
                r#"
[package]
name = "foo"
edition = "2021"

[dependencies]
common = { path = "../common" }
"#,
            ),
            ("foo/src/lib.rs", ""),
            (
                "bar/Cargo.toml",
                r#"
[package]
name = "bar"
edition = "2021"

[dependencies]
common = { path = "../common" }
"#,
            ),
            ("bar/src/lib.rs", ""),
            (
                "common/Cargo.toml",
                r#"
[package]
name = "common"
edition = "2021"
"#,
            ),
            ("common/src/lib.rs", ""),
        ]);

        let graph = CrateGraph::build(&base.join("app")).unwrap();

        assert_eq!(graph.nodes.len(), 4);

        // Topo order: common must come before foo and bar, which come before app
        let names: Vec<_> = graph.iter_topo().map(|n| n.package_name.as_str()).collect();

        // common must be first
        assert_eq!(names[0], "common");
        // app must be last
        assert_eq!(names[3], "app");
        // foo and bar in the middle (order between them is deterministic but either is valid)
        assert!(names[1] == "bar" || names[1] == "foo");
        assert!(names[2] == "bar" || names[2] == "foo");
        assert_ne!(names[1], names[2]);
    }

    #[test]
    fn test_workspace_relative_paths() {
        let (_dir, base) = setup_multi_crate(&[
            (
                "project/app/Cargo.toml",
                r#"
[package]
name = "app"
edition = "2021"

[dependencies]
util = { path = "../util" }
"#,
            ),
            ("project/app/src/main.rs", "fn main() {}"),
            (
                "project/util/Cargo.toml",
                r#"
[package]
name = "util"
edition = "2021"
"#,
            ),
            ("project/util/src/lib.rs", ""),
        ]);

        let graph = CrateGraph::build(&base.join("project/app")).unwrap();

        // All paths should be workspace-relative with no ..
        for node in graph.nodes.values() {
            assert!(
                !node.workspace_rel.as_str().contains(".."),
                "workspace_rel should not contain ..: {}",
                node.workspace_rel
            );
            assert!(
                !node.crate_root_rel.as_str().contains(".."),
                "crate_root_rel should not contain ..: {}",
                node.crate_root_rel
            );
        }
    }

    #[test]
    fn test_reject_bin_as_dependency() {
        let (_dir, base) = setup_multi_crate(&[
            (
                "app/Cargo.toml",
                r#"
[package]
name = "app"
edition = "2021"

[dependencies]
other = { path = "../other" }
"#,
            ),
            ("app/src/main.rs", "fn main() {}"),
            (
                "other/Cargo.toml",
                r#"
[package]
name = "other"
edition = "2021"
"#,
            ),
            ("other/src/main.rs", "fn main() {}"), // bin, not lib!
        ]);

        let err = CrateGraph::build(&base.join("app")).unwrap_err();
        assert!(matches!(err, CrateGraphError::NotALibrary { name } if name == "other"));
    }

    #[test]
    fn test_crate_name_hyphen_conversion() {
        let (_dir, base) = setup_multi_crate(&[
            (
                "my-app/Cargo.toml",
                r#"
[package]
name = "my-app"
edition = "2021"

[dependencies]
my-util = { path = "../my-util" }
"#,
            ),
            ("my-app/src/main.rs", "fn main() {}"),
            (
                "my-util/Cargo.toml",
                r#"
[package]
name = "my-util"
edition = "2021"
"#,
            ),
            ("my-util/src/lib.rs", ""),
        ]);

        let graph = CrateGraph::build(&base.join("my-app")).unwrap();

        let root = graph.root();
        assert_eq!(root.package_name, "my-app");
        assert_eq!(root.crate_name, "my_app");
        assert_eq!(root.deps[0].extern_name, "my_util");
    }
}
