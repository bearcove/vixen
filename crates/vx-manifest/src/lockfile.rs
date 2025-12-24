//! Cargo.lock reachability analysis.
//!
//! This module extends facet-cargo-toml's lockfile parsing with dependency
//! reachability analysis.
//!
//! ## Reachability
//!
//! Starting from the root package, we BFS through dependencies to find all
//! reachable packages. Only these packages need to be materialized and built.

use std::collections::{HashMap, HashSet, VecDeque};

// Re-export from facet-cargo-toml
pub use facet_cargo_toml::{CargoLock, LockPackage, CRATES_IO_SOURCE};

/// Errors that can occur during lockfile reachability analysis
#[derive(Debug)]
pub enum LockfileError {
    UnsupportedVersion(u32),
    UnsupportedSource { name: String, source_url: String },
    MissingChecksum { name: String, version: String },
    RootNotFound { name: String },
    DependencyNotFound { package: String, dep: String },
}

impl std::fmt::Display for LockfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported lockfile version: {v} (supported: 3, 4)"),
            Self::UnsupportedSource { name, source_url } => write!(f, "package '{name}' has unsupported source: {source_url}"),
            Self::MissingChecksum { name, version } => write!(f, "registry package '{name} {version}' is missing checksum"),
            Self::RootNotFound { name } => write!(f, "root package '{name}' not found in lockfile"),
            Self::DependencyNotFound { package, dep } => write!(f, "dependency '{dep}' of package '{package}' not found in lockfile"),
        }
    }
}

impl std::error::Error for LockfileError {}

/// Returns a unique key for a package: "name version"
fn package_key(pkg: &LockPackage) -> String {
    format!("{} {}", pkg.name, pkg.version)
}

/// Index for efficient package lookups
pub struct LockfileIndex<'a> {
    /// Map from "name version" to package
    by_key: HashMap<String, &'a LockPackage>,
    /// Map from name to list of packages (for ambiguous lookups)
    by_name: HashMap<&'a str, Vec<&'a LockPackage>>,
}

impl<'a> LockfileIndex<'a> {
    fn new(lockfile: &'a CargoLock) -> Self {
        let mut by_key = HashMap::new();
        let mut by_name: HashMap<&str, Vec<&LockPackage>> = HashMap::new();

        for pkg in &lockfile.packages {
            by_key.insert(package_key(pkg), pkg);
            by_name.entry(&pkg.name).or_default().push(pkg);
        }

        LockfileIndex { by_key, by_name }
    }

    /// Resolve a dependency string to a package.
    ///
    /// Dependency strings can be:
    /// - "name" (unambiguous, single version in lockfile)
    /// - "name version"
    /// - "name version (source)" (v4 format for disambiguation)
    pub fn resolve_dep(&self, dep: &str) -> Option<&'a LockPackage> {
        // Try exact match first (handles "name version" and "name version (source)")
        // Strip the source part if present
        let dep_key = if let Some(paren_idx) = dep.find(" (") {
            &dep[..paren_idx]
        } else {
            dep
        };

        if let Some(pkg) = self.by_key.get(dep_key) {
            return Some(pkg);
        }

        // Try name-only lookup (for unique deps)
        if !dep.contains(' ')
            && let Some(pkgs) = self.by_name.get(dep)
            && pkgs.len() == 1
        {
            return Some(pkgs[0]);
        }

        None
    }
}

/// Result of reachability analysis
#[derive(Debug)]
pub struct ReachablePackages {
    /// All reachable packages (for lookups)
    packages: Vec<LockPackage>,
    /// Index by "name version" for fast lookups
    by_key: HashMap<String, usize>,
    /// Index by name for prefix lookups
    by_name: HashMap<String, Vec<usize>>,
}

impl ReachablePackages {
    /// Create from a list of packages
    fn new(packages: Vec<LockPackage>) -> Self {
        let mut by_key = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, pkg) in packages.iter().enumerate() {
            by_key.insert(package_key(pkg), idx);
            by_name.entry(pkg.name.clone()).or_default().push(idx);
        }

        ReachablePackages {
            packages,
            by_key,
            by_name,
        }
    }

    /// Iterate over registry packages only
    pub fn registry_packages(&self) -> impl Iterator<Item = &LockPackage> {
        self.packages.iter().filter(|p| p.is_registry())
    }

    /// Iterate over path packages only
    pub fn path_packages(&self) -> impl Iterator<Item = &LockPackage> {
        self.packages.iter().filter(|p| p.is_path())
    }

    /// Get a package by name and version
    pub fn get_package(&self, name: &str, version: &str) -> Option<&LockPackage> {
        let key = format!("{} {}", name, version);
        self.by_key.get(&key).map(|&idx| &self.packages[idx])
    }

    /// Find a package by name prefix (for resolving dependencies)
    pub fn find_by_name_prefix(&self, name: &str) -> Option<&LockPackage> {
        self.by_name
            .get(name)
            .and_then(|indices| indices.first())
            .map(|&idx| &self.packages[idx])
    }

    /// Find a path package by name (source.is_none()).
    /// Returns None if no path package with that name exists.
    /// Use this for resolving path crate entries in the lockfile.
    pub fn find_path_package(&self, name: &str) -> Option<&LockPackage> {
        self.by_name.get(name).and_then(|indices| {
            indices
                .iter()
                .map(|&idx| &self.packages[idx])
                .find(|pkg| pkg.source.is_none())
        })
    }

    /// Resolve a dependency string to a package.
    ///
    /// Handles formats: "name", "name version", "name version (source)"
    pub fn find_dependency(&self, dep_str: &str) -> Option<&LockPackage> {
        // Strip source part if present: "name version (source)" -> "name version"
        let dep_key = if let Some(paren_idx) = dep_str.find(" (") {
            &dep_str[..paren_idx]
        } else {
            dep_str
        };

        // Try exact "name version" match
        if let Some(&idx) = self.by_key.get(dep_key) {
            return Some(&self.packages[idx]);
        }

        // Try name-only lookup (for unique deps)
        if !dep_key.contains(' ')
            && let Some(indices) = self.by_name.get(dep_key)
            && indices.len() == 1
        {
            return Some(&self.packages[indices[0]]);
        }

        None
    }

    /// Check if a package is in the reachable set
    pub fn contains(&self, name: &str, version: &str) -> bool {
        let key = format!("{} {}", name, version);
        self.by_key.contains_key(&key)
    }
}

/// Extension trait for CargoLock with reachability analysis
pub trait CargoLockExt {
    /// Find a path package by name (source = None).
    /// This prevents matching a registry crate with the same name.
    fn find_path_by_name(&self, name: &str) -> Option<&LockPackage>;

    /// Build an index for efficient lookups
    fn build_index(&self) -> LockfileIndex<'_>;

    /// Compute reachable packages starting from the root package name.
    ///
    /// This performs BFS through the dependency graph and returns all
    /// reachable registry and path packages.
    fn compute_reachable(&self, root_name: &str) -> Result<ReachablePackages, LockfileError>;
}

impl CargoLockExt for CargoLock {
    fn find_path_by_name(&self, name: &str) -> Option<&LockPackage> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.source.is_none())
    }

    fn build_index(&self) -> LockfileIndex<'_> {
        LockfileIndex::new(self)
    }

    fn compute_reachable(&self, root_name: &str) -> Result<ReachablePackages, LockfileError> {
        // Validate version
        if self.version != 3 && self.version != 4 {
            return Err(LockfileError::UnsupportedVersion(self.version));
        }

        let index = self.build_index();

        // Find root package - must be a path package (source = None).
        // This prevents matching a registry crate with the same name.
        let root =
            self.find_path_by_name(root_name)
                .ok_or_else(|| LockfileError::RootNotFound {
                    name: root_name.to_string(),
                })?;

        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<&LockPackage> = VecDeque::new();
        let mut reachable_packages: Vec<LockPackage> = Vec::new();

        queue.push_back(root);
        visited.insert(package_key(root));

        while let Some(pkg) = queue.pop_front() {
            // Validate the package source
            if pkg.is_registry() {
                // Validate checksum exists for registry packages
                if pkg.checksum.is_none() {
                    return Err(LockfileError::MissingChecksum {
                        name: pkg.name.clone(),
                        version: pkg.version.clone(),
                    });
                }
            } else if !pkg.is_path() {
                // Unsupported source (git, other registry, etc.)
                return Err(LockfileError::UnsupportedSource {
                    name: pkg.name.clone(),
                    source_url: pkg.source.clone().unwrap_or_default(),
                });
            }

            // Clone the package into our result set
            reachable_packages.push(pkg.clone());

            // Enqueue dependencies
            for dep_str in &pkg.dependencies {
                let dep_pkg = index.resolve_dep(dep_str).ok_or_else(|| {
                    LockfileError::DependencyNotFound {
                        package: package_key(pkg),
                        dep: dep_str.clone(),
                    }
                })?;

                let dep_key = package_key(dep_pkg);
                if !visited.contains(&dep_key) {
                    visited.insert(dep_key);
                    queue.push_back(dep_pkg);
                }
            }
        }

        Ok(ReachablePackages::new(reachable_packages))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v4_lockfile() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "serde",
]

[[package]]
name = "serde"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        assert_eq!(lockfile.version, 4);
        assert_eq!(lockfile.packages.len(), 2);

        let serde = lockfile.find_by_name("serde").unwrap();
        assert!(serde.is_registry());
        assert_eq!(
            serde.checksum.as_deref(),
            Some("3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2")
        );
    }

    #[test]
    fn parse_v3_lockfile() {
        let contents = r#"
version = 3

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "serde 1.0.197",
]

[[package]]
name = "serde"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        assert_eq!(lockfile.version, 3);
    }

    #[test]
    fn reject_unsupported_version() {
        let contents = r#"
version = 2

[[package]]
name = "myapp"
version = "0.1.0"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let err = lockfile.compute_reachable("myapp").unwrap_err();
        assert!(matches!(err, LockfileError::UnsupportedVersion(2)));
    }

    #[test]
    fn compute_reachable_simple() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "serde",
]

[[package]]
name = "serde"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        let path_pkgs: Vec<_> = reachable.path_packages().collect();
        assert_eq!(path_pkgs.len(), 1);
        assert_eq!(path_pkgs[0].name, "myapp");

        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 1);
        assert_eq!(registry_pkgs[0].name, "serde");
        assert_eq!(registry_pkgs[0].version, "1.0.197");
    }

    #[test]
    fn compute_reachable_transitive() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "serde",
]

[[package]]
name = "serde"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"
dependencies = [
    "serde_derive",
]

[[package]]
name = "serde_derive"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 2);
        let names: Vec<_> = registry_pkgs.iter().map(|p| &p.name).collect();
        assert!(names.contains(&&"serde".to_string()));
        assert!(names.contains(&&"serde_derive".to_string()));
    }

    #[test]
    fn compute_reachable_skips_unreachable() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "used_crate",
]

[[package]]
name = "used_crate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"

[[package]]
name = "unused_crate"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 1);
        assert_eq!(registry_pkgs[0].name, "used_crate");
    }

    #[test]
    fn reject_git_dependency() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "git_dep",
]

[[package]]
name = "git_dep"
version = "0.1.0"
source = "git+https://github.com/example/repo#abcd1234"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let err = lockfile.compute_reachable("myapp").unwrap_err();
        assert!(matches!(err, LockfileError::UnsupportedSource { name, .. } if name == "git_dep"));
    }

    #[test]
    fn reject_missing_checksum() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "bad_crate",
]

[[package]]
name = "bad_crate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let err = lockfile.compute_reachable("myapp").unwrap_err();
        assert!(matches!(err, LockfileError::MissingChecksum { name, .. } if name == "bad_crate"));
    }

    #[test]
    fn resolve_versioned_dep_string() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "foo 1.0.0",
    "foo 2.0.0",
]

[[package]]
name = "foo"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"

[[package]]
name = "foo"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 2);
        let versions: Vec<_> = registry_pkgs.iter().map(|p| &p.version).collect();
        assert!(versions.contains(&&"1.0.0".to_string()));
        assert!(versions.contains(&&"2.0.0".to_string()));
    }

    #[test]
    fn resolve_dep_with_source_suffix() {
        // v4 format can have "name version (source)" for disambiguation
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "foo 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
]

[[package]]
name = "foo"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 1);
        assert_eq!(registry_pkgs[0].name, "foo");
    }

    #[test]
    fn path_deps_with_path_deps() {
        let contents = r#"
version = 4

[[package]]
name = "myapp"
version = "0.1.0"
dependencies = [
    "mylib",
]

[[package]]
name = "mylib"
version = "0.1.0"
dependencies = [
    "serde",
]

[[package]]
name = "serde"
version = "1.0.197"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"
"#;
        let lockfile = CargoLock::parse(contents).unwrap();
        let reachable = lockfile.compute_reachable("myapp").unwrap();

        // Both myapp and mylib are path deps
        let path_pkgs: Vec<_> = reachable.path_packages().collect();
        assert_eq!(path_pkgs.len(), 2);
        let path_names: Vec<_> = path_pkgs.iter().map(|p| &p.name).collect();
        assert!(path_names.contains(&&"myapp".to_string()));
        assert!(path_names.contains(&&"mylib".to_string()));

        // serde is a registry dep
        let registry_pkgs: Vec<_> = reachable.registry_packages().collect();
        assert_eq!(registry_pkgs.len(), 1);
        assert_eq!(registry_pkgs[0].name, "serde");
    }
}
