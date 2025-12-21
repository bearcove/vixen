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
//! 2. **CrateId uses source-aware identity**:
//!    - Path crates: hash of `(workspace_rel, package_name)`
//!    - Registry crates: hash of `(name, version, checksum)`
//!    This ensures stable cache keys regardless of checkout location.
//!
//! 3. **All paths are workspace-relative**: No absolute paths leak into cache keys
//!    or rustc invocations. Registry crates are materialized to `.vx/registry/...`.

use std::collections::{HashMap, HashSet, VecDeque};

use camino::{Utf8Path, Utf8PathBuf};
use thiserror::Error;
use vx_cas_proto::Blake3Hash;
use vx_manifest::lockfile::{Lockfile, LockfileError, ReachablePackages};
use vx_manifest::{Manifest, ManifestError};

/// Errors during crate graph construction
#[derive(Debug, Error)]
pub enum CrateGraphError {
    #[error("failed to parse manifest: {0}")]
    ManifestError(#[from] ManifestError),

    #[error("failed to parse lockfile: {0}")]
    LockfileError(#[from] LockfileError),

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

    #[error(
        "registry dependencies require Cargo.lock (found '{dep_name}' with version in [dependencies])"
    )]
    MissingLockfile { dep_name: String },

    #[error("registry dependency '{name}' version '{version}' not found in Cargo.lock")]
    RegistryDepNotInLockfile { name: String, version: String },

    #[error("registry crate '{name}' {version} has build.rs, which is not supported yet")]
    RegistryBuildScript { name: String, version: String },

    #[error("registry crate '{name}' {version} is a proc-macro, which is not supported yet")]
    RegistryProcMacro { name: String, version: String },

    #[error("registry crate '{name}' {version} has unsupported features: {details}")]
    RegistryUnsupported {
        name: String,
        version: String,
        details: String,
    },

    #[error("registry crate not materialized: {name} {version}")]
    RegistryNotMaterialized { name: String, version: String },
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

/// Source of a crate in the dependency graph
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CrateSource {
    /// Local path dependency (workspace-relative)
    Path {
        /// Workspace-relative path to crate directory
        workspace_rel: Utf8PathBuf,
    },
    /// Registry dependency (crates.io)
    Registry {
        /// Crate name on crates.io
        name: String,
        /// Exact version from Cargo.lock
        version: String,
        /// SHA256 checksum from Cargo.lock
        checksum: String,
    },
}

impl CrateSource {
    /// Returns true if this is a path dependency
    pub fn is_path(&self) -> bool {
        matches!(self, CrateSource::Path { .. })
    }

    /// Returns true if this is a registry dependency
    pub fn is_registry(&self) -> bool {
        matches!(self, CrateSource::Registry { .. })
    }

    /// Get a display string for this source
    pub fn display(&self) -> String {
        match self {
            CrateSource::Path { workspace_rel } => format!("path:{}", workspace_rel),
            CrateSource::Registry { name, version, .. } => format!("registry:{name}@{version}"),
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
/// This is a blake3 hash of source-specific fields:
/// - Path crates: hash of `(workspace_rel, package_name)`
/// - Registry crates: hash of `(name, version, checksum)`
///
/// This ensures stability across different checkout locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CrateId(pub Blake3Hash);

impl CrateId {
    /// Create a CrateId for a path dependency
    pub fn for_path(workspace_rel: &Utf8Path, package_name: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crate_id_v2:path:");
        hasher.update(workspace_rel.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(package_name.as_bytes());
        CrateId(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    /// Create a CrateId for a registry dependency
    pub fn for_registry(name: &str, version: &str, checksum: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crate_id_v2:registry:");
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(version.as_bytes());
        hasher.update(b"\0");
        hasher.update(checksum.as_bytes());
        CrateId(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    /// Create a CrateId from a CrateSource and package name
    pub fn from_source(source: &CrateSource, package_name: &str) -> Self {
        match source {
            CrateSource::Path { workspace_rel } => Self::for_path(workspace_rel, package_name),
            CrateSource::Registry {
                name,
                version,
                checksum,
            } => Self::for_registry(name, version, checksum),
        }
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
    /// Source of this crate (path or registry)
    pub source: CrateSource,
    /// Path to crate directory, relative to workspace root (no `..`)
    /// For path deps: the original crate directory
    /// For registry deps: `.vx/registry/<name>/<version>/`
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

/// Information about a registry crate needed for materialization
#[derive(Debug, Clone)]
pub struct RegistryCrateInfo {
    /// Crate name
    pub name: String,
    /// Exact version
    pub version: String,
    /// SHA256 checksum from Cargo.lock
    pub checksum: String,
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
    /// Registry crates that need to be materialized (for path-only build, empty)
    pub registry_crates: Vec<RegistryCrateInfo>,
    /// Parsed lockfile reachable packages (if any)
    pub reachable_packages: Option<ReachablePackages>,
}

impl CrateGraph {
    /// Build a crate graph starting from the given directory.
    ///
    /// This parses the Cargo.toml in `invocation_dir`, recursively resolves
    /// all path dependencies, computes the LCA workspace root, and produces
    /// a topological ordering for builds.
    ///
    /// **Note**: This method only supports path dependencies. For projects with
    /// registry dependencies (crates.io), use `build_with_lockfile` instead.
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

            // Check for version deps - these require lockfile support
            if manifest.has_version_deps() {
                let first_dep = manifest.version_deps.first().unwrap();
                return Err(CrateGraphError::MissingLockfile {
                    dep_name: first_dep.name.clone(),
                });
            }

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

            // Path dependencies use workspace-relative path as identity
            let source = CrateSource::Path {
                workspace_rel: workspace_rel.clone(),
            };
            let id = CrateId::for_path(&workspace_rel, &manifest.name);

            let node = CrateNode {
                id,
                package_name: manifest.name.clone(),
                crate_name: manifest.crate_name(),
                source,
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
            registry_crates: Vec::new(), // No registry crates in path-only build
            reachable_packages: None,
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

    /// Returns true if this graph has registry dependencies
    pub fn has_registry_deps(&self) -> bool {
        !self.registry_crates.is_empty()
    }

    /// Iterate over registry crates that need materialization
    pub fn iter_registry_crates(&self) -> impl Iterator<Item = &RegistryCrateInfo> {
        self.registry_crates.iter()
    }

    /// Build a crate graph with registry dependency support.
    ///
    /// This is a two-phase process:
    /// 1. First call `build_with_lockfile` to parse manifests and lockfile, computing
    ///    which registry crates are needed.
    /// 2. Materialize registry crates (caller's responsibility via execd).
    /// 3. Call `finalize_with_registry` to add registry nodes to the graph.
    ///
    /// For path-only projects (no version deps), use `build()` instead.
    pub fn build_with_lockfile(invocation_dir: &Utf8Path) -> Result<Self, CrateGraphError> {
        // Canonicalize the starting directory
        let invocation_dir = invocation_dir.canonicalize_utf8().map_err(|e| {
            CrateGraphError::CanonicalizationError {
                path: invocation_dir.to_owned(),
                source: e,
            }
        })?;

        // Phase 1: Collect all path-based crate directories (absolute paths)
        let mut all_crate_dirs: Vec<Utf8PathBuf> = Vec::new();
        let mut manifests: HashMap<Utf8PathBuf, Manifest> = HashMap::new();
        let mut to_visit: VecDeque<Utf8PathBuf> = VecDeque::new();
        let mut visited: HashSet<Utf8PathBuf> = HashSet::new();
        let mut has_version_deps = false;

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

            // Check for version deps
            if manifest.has_version_deps() {
                has_version_deps = true;
            }

            // Queue path dependencies
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

        // Phase 2: Parse lockfile if we have version deps
        let (reachable_packages, registry_crates) = if has_version_deps {
            let lockfile_path = invocation_dir.join("Cargo.lock");
            if !lockfile_path.exists() {
                // Find first version dep for error message
                let first_version_dep = manifests
                    .values()
                    .flat_map(|m| m.version_deps.iter())
                    .next()
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                return Err(CrateGraphError::MissingLockfile {
                    dep_name: first_version_dep,
                });
            }

            let lockfile = Lockfile::from_path(&lockfile_path)?;
            let root_name = manifests[&invocation_dir].name.as_str();
            let reachable = lockfile.compute_reachable(root_name)?;

            // Build list of registry crates to materialize
            let registry_crates: Vec<RegistryCrateInfo> = reachable
                .registry_packages()
                .map(|pkg| RegistryCrateInfo {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    checksum: pkg.checksum.clone().unwrap_or_default(),
                })
                .collect();

            (Some(reachable), registry_crates)
        } else {
            (None, Vec::new())
        };

        // Phase 3: Compute LCA workspace root (path crates only for now)
        let workspace_root = compute_lca(&all_crate_dirs);

        // Phase 4: Build nodes for path crates
        let mut nodes: HashMap<CrateId, CrateNode> = HashMap::new();
        let mut dir_to_id: HashMap<Utf8PathBuf, CrateId> = HashMap::new();

        for (crate_dir, manifest) in &manifests {
            let workspace_rel = normalize_path(crate_dir, &workspace_root)?;

            // Determine crate type and root file
            let (crate_type, crate_root_abs) = if manifest.is_bin() {
                let bin = manifest.bin.as_ref().unwrap();
                (CrateType::Bin, bin.path.clone())
            } else if manifest.is_lib() {
                let lib = manifest.lib.as_ref().unwrap();
                (CrateType::Lib, lib.path.clone())
            } else {
                unreachable!("manifest has no targets");
            };

            let crate_root_rel = normalize_path(&crate_root_abs, &workspace_root)?;

            let source = CrateSource::Path {
                workspace_rel: workspace_rel.clone(),
            };
            let id = CrateId::for_path(&workspace_rel, &manifest.name);

            let node = CrateNode {
                id,
                package_name: manifest.name.clone(),
                crate_name: manifest.crate_name(),
                source,
                workspace_rel,
                crate_root_rel,
                edition: manifest.edition,
                crate_type,
                deps: Vec::new(),
            };

            dir_to_id.insert(crate_dir.clone(), id);
            nodes.insert(id, node);
        }

        // Phase 5: Resolve path dependency edges (registry deps resolved in finalize)
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

            deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
            nodes.get_mut(&crate_id).unwrap().deps = deps;
        }

        // Phase 6: Topological sort (partial - only path crates)
        let root_crate = dir_to_id[&invocation_dir];
        let topo_order = if registry_crates.is_empty() {
            topological_sort(&nodes, root_crate)?
        } else {
            // Will be recomputed in finalize_with_registry
            Vec::new()
        };

        Ok(CrateGraph {
            workspace_root,
            nodes,
            topo_order,
            root_crate,
            registry_crates,
            reachable_packages,
        })
    }

    /// Finalize graph with materialized registry crates.
    ///
    /// `materialized_paths` maps `(name, version)` -> workspace-relative path where
    /// the registry crate was extracted (e.g., `.vx/registry/serde/1.0.197`).
    ///
    /// This parses registry crate manifests, validates v0 constraints, adds nodes
    /// to the graph, resolves registry dependency edges, and recomputes topo order.
    pub fn finalize_with_registry(
        &mut self,
        materialized_paths: &HashMap<(String, String), Utf8PathBuf>,
    ) -> Result<(), CrateGraphError> {
        let Some(ref reachable) = self.reachable_packages else {
            // No registry deps, nothing to do
            return Ok(());
        };

        // Phase 1: Parse registry crate manifests and create nodes
        let mut registry_id_map: HashMap<(String, String), CrateId> = HashMap::new();

        for info in &self.registry_crates {
            let key = (info.name.clone(), info.version.clone());
            let workspace_rel = materialized_paths.get(&key).ok_or_else(|| {
                CrateGraphError::RegistryNotMaterialized {
                    name: info.name.clone(),
                    version: info.version.clone(),
                }
            })?;

            // Parse the manifest
            let manifest_path = self.workspace_root.join(workspace_rel).join("Cargo.toml");
            let manifest = Manifest::from_path(&manifest_path)?;

            // Registry crates must be libraries
            if !manifest.is_lib() {
                return Err(CrateGraphError::NotALibrary {
                    name: info.name.clone(),
                });
            }

            let lib = manifest.lib.as_ref().unwrap();
            let crate_root_rel = workspace_rel.join(&lib.path);

            let source = CrateSource::Registry {
                name: info.name.clone(),
                version: info.version.clone(),
                checksum: info.checksum.clone(),
            };
            let id = CrateId::for_registry(&info.name, &info.version, &info.checksum);

            let node = CrateNode {
                id,
                package_name: manifest.name.clone(),
                crate_name: manifest.crate_name(),
                source,
                workspace_rel: workspace_rel.clone(),
                crate_root_rel,
                edition: manifest.edition,
                crate_type: CrateType::Lib,
                deps: Vec::new(), // Filled in next phase
            };

            registry_id_map.insert(key, id);
            self.nodes.insert(id, node);
        }

        // Phase 2: Resolve registry dependency edges
        // Registry crates can depend on other registry crates (from lockfile)
        for info in &self.registry_crates {
            let key = (info.name.clone(), info.version.clone());
            let crate_id = registry_id_map[&key];

            // Get dependencies from lockfile
            let pkg = reachable
                .get_package(&info.name, &info.version)
                .expect("registry crate must be in reachable packages");

            let mut deps: Vec<DepEdge> = Vec::new();

            for dep_spec in &pkg.dependencies {
                // Parse dep spec: "name version" or "name version (registry+...)"
                let dep_name = dep_spec.split_whitespace().next().unwrap_or(dep_spec);

                // Find matching package in lockfile
                if let Some(dep_pkg) = reachable.find_dependency(dep_spec) {
                    // Check if it's a registry dep
                    if dep_pkg.is_registry() {
                        let dep_key = (dep_pkg.name.clone(), dep_pkg.version.clone());
                        if let Some(&dep_id) = registry_id_map.get(&dep_key) {
                            deps.push(DepEdge {
                                extern_name: dep_name.replace('-', "_"),
                                crate_id: dep_id,
                            });
                        }
                    }
                    // Path deps of registry crates are not supported in v1
                }
            }

            deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
            self.nodes.get_mut(&crate_id).unwrap().deps = deps;
        }

        // Phase 3: Link path crates to their registry deps
        // We need to re-walk path manifests to connect version_deps
        for (_id, node) in self.nodes.iter_mut() {
            if node.source.is_path() {
                let manifest_path = self
                    .workspace_root
                    .join(&node.workspace_rel)
                    .join("Cargo.toml");
                if let Ok(manifest) = Manifest::from_path(&manifest_path) {
                    for version_dep in &manifest.version_deps {
                        // Find in lockfile
                        if let Some(pkg) = reachable.find_by_name_prefix(&version_dep.name) {
                            if pkg.is_registry() {
                                let dep_key = (pkg.name.clone(), pkg.version.clone());
                                if let Some(&dep_id) = registry_id_map.get(&dep_key) {
                                    node.deps.push(DepEdge {
                                        extern_name: version_dep.name.replace('-', "_"),
                                        crate_id: dep_id,
                                    });
                                }
                            }
                        }
                    }
                    node.deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
                }
            }
        }

        // Phase 4: Recompute topological order with all nodes
        self.topo_order = topological_sort(&self.nodes, self.root_crate)?;

        Ok(())
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
    fn test_crate_id_path_deterministic() {
        let id1 = CrateId::for_path(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::for_path(Utf8Path::new("app"), "my-app");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_crate_id_path_different_paths() {
        let id1 = CrateId::for_path(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::for_path(Utf8Path::new("other"), "my-app");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_crate_id_path_different_names() {
        let id1 = CrateId::for_path(Utf8Path::new("app"), "my-app");
        let id2 = CrateId::for_path(Utf8Path::new("app"), "other-app");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_crate_id_registry_deterministic() {
        let id1 = CrateId::for_registry("serde", "1.0.197", "abc123");
        let id2 = CrateId::for_registry("serde", "1.0.197", "abc123");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_crate_id_registry_different_version() {
        let id1 = CrateId::for_registry("serde", "1.0.197", "abc123");
        let id2 = CrateId::for_registry("serde", "1.0.198", "def456");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_crate_id_path_vs_registry_different() {
        // A path crate and registry crate with same name should have different IDs
        let path_id = CrateId::for_path(Utf8Path::new("serde"), "serde");
        let registry_id = CrateId::for_registry("serde", "1.0.197", "abc123");
        assert_ne!(path_id, registry_id);
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
