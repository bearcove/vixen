//! Workspace snapshotting for remote Rust compilation
//!
//! This module implements the "maximalist snapshot" strategy for sending
//! source files to remote execd for compilation:
//!
//! Instead of trying to predict which files will be needed (which requires
//! understanding cfg, macros, include!, etc.), we send a conservative superset:
//! - All *.rs files under the crate root directory
//! - Cargo manifest files (Cargo.toml, Cargo.lock)
//! - Any explicitly declared extra inputs
//!
//! This is "dumb but correct" - rustc will only use what it needs, and
//! dep-info will tell us what was actually used.

use camino::{Utf8Path, Utf8PathBuf};
use facet::Facet;
use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("failed to read directory {path}: {source}")]
    ReadDirError {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to get metadata for {path}: {source}")]
    MetadataError {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("path is not valid UTF-8: {path:?}")]
    InvalidUtf8 { path: std::path::PathBuf },
}

/// A snapshot manifest entry: a file that should be materialized in the remote workspace
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct SnapshotEntry {
    /// Workspace-relative path (e.g., "crates/foo/src/lib.rs")
    pub path: String,
    /// Blake3 hash of the file content (for CAS lookup)
    pub blob_hash: String,
    /// Unix file mode (e.g., 0o644 for regular file, 0o755 for executable)
    pub mode: u32,
}

/// A complete snapshot manifest for a crate build
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct SnapshotManifest {
    /// All files to materialize
    pub entries: Vec<SnapshotEntry>,
}

impl SnapshotManifest {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add an entry to the manifest
    pub fn add_entry(&mut self, entry: SnapshotEntry) {
        self.entries.push(entry);
    }

    /// Sort entries by path for determinism
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

impl Default for SnapshotManifest {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a maximalist snapshot for a Rust crate
///
/// This includes:
/// - All *.rs files recursively under `crate_root_dir`
/// - The crate's Cargo.toml
/// - Workspace Cargo.toml and Cargo.lock (if provided)
///
/// The snapshot is intentionally over-inclusive to avoid missing files
/// due to cfg/macro/include! complexity.
///
/// # Arguments
///
/// * `crate_root_dir` - Directory containing the crate (e.g., "crates/foo")
/// * `workspace_root` - Workspace root for computing relative paths
/// * `include_workspace_manifest` - Whether to include workspace Cargo.toml/lock
///
/// # Returns
///
/// A list of workspace-relative paths that should be included in the snapshot.
/// The caller is responsible for computing blob hashes and creating SnapshotEntries.
pub fn build_maximalist_snapshot(
    crate_root_dir: &Utf8Path,
    workspace_root: &Utf8Path,
    include_workspace_manifest: bool,
) -> Result<Vec<Utf8PathBuf>, SnapshotError> {
    let mut files = Vec::new();

    // Find all *.rs files recursively
    collect_rs_files_recursive(crate_root_dir, &mut files)?;

    // Add crate Cargo.toml
    let crate_toml = crate_root_dir.join("Cargo.toml");
    if crate_toml.exists() {
        files.push(crate_toml);
    }

    // Add workspace manifests if requested
    if include_workspace_manifest {
        let workspace_toml = workspace_root.join("Cargo.toml");
        if workspace_toml.exists() {
            files.push(workspace_toml);
        }

        let workspace_lock = workspace_root.join("Cargo.lock");
        if workspace_lock.exists() {
            files.push(workspace_lock);
        }
    }

    // Convert to workspace-relative paths
    let mut relative_paths = Vec::new();
    for path in files {
        let rel_path = path
            .strip_prefix(workspace_root)
            .map(|p| p.to_owned())
            .unwrap_or(path);
        relative_paths.push(rel_path);
    }

    // Sort for determinism
    relative_paths.sort();
    relative_paths.dedup();

    Ok(relative_paths)
}

/// Recursively collect all *.rs files in a directory
fn collect_rs_files_recursive(
    dir: &Utf8Path,
    output: &mut Vec<Utf8PathBuf>,
) -> Result<(), SnapshotError> {
    let entries = std::fs::read_dir(dir).map_err(|source| SnapshotError::ReadDirError {
        path: dir.to_owned(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| SnapshotError::ReadDirError {
            path: dir.to_owned(),
            source,
        })?;

        let path = entry.path();
        let path_utf8 = Utf8PathBuf::try_from(path.clone())
            .map_err(|_| SnapshotError::InvalidUtf8 { path: path.clone() })?;

        let metadata = entry
            .metadata()
            .map_err(|source| SnapshotError::MetadataError {
                path: path_utf8.clone(),
                source,
            })?;

        if metadata.is_dir() {
            // Recurse into subdirectories
            collect_rs_files_recursive(&path_utf8, output)?;
        } else if metadata.is_file() && path_utf8.extension() == Some("rs") {
            output.push(path_utf8);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_crate(dir: &Utf8Path) -> io::Result<()> {
        // Create src directory structure
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("src/lib.rs"), "// lib.rs")?;
        fs::write(dir.join("src/main.rs"), "// main.rs")?;
        fs::write(dir.join("src/module.rs"), "// module.rs")?;

        // Create nested module
        fs::create_dir_all(dir.join("src/nested"))?;
        fs::write(dir.join("src/nested/mod.rs"), "// nested/mod.rs")?;
        fs::write(dir.join("src/nested/child.rs"), "// nested/child.rs")?;

        // Create Cargo.toml
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"\n")?;

        // Create non-.rs files (should be ignored)
        fs::write(dir.join("README.md"), "# Test")?;
        fs::write(dir.join("src/data.txt"), "data")?;

        Ok(())
    }

    #[test]
    fn test_maximalist_snapshot_basic() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let temp_path = Utf8Path::from_path(temp.path()).unwrap();
        let crate_dir = temp_path.join("crates/foo");

        create_test_crate(&crate_dir)?;

        let snapshot = build_maximalist_snapshot(&crate_dir, temp_path, false)?;

        // Should include all .rs files
        assert!(snapshot.contains(&"crates/foo/src/lib.rs".into()));
        assert!(snapshot.contains(&"crates/foo/src/main.rs".into()));
        assert!(snapshot.contains(&"crates/foo/src/module.rs".into()));
        assert!(snapshot.contains(&"crates/foo/src/nested/mod.rs".into()));
        assert!(snapshot.contains(&"crates/foo/src/nested/child.rs".into()));

        // Should include Cargo.toml
        assert!(snapshot.contains(&"crates/foo/Cargo.toml".into()));

        // Should NOT include non-.rs files
        assert!(!snapshot.contains(&"crates/foo/README.md".into()));
        assert!(!snapshot.contains(&"crates/foo/src/data.txt".into()));

        Ok(())
    }

    #[test]
    fn test_snapshot_includes_workspace_manifests() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let temp_path = Utf8Path::from_path(temp.path()).unwrap();
        let crate_dir = temp_path.join("crates/foo");

        create_test_crate(&crate_dir)?;

        // Create workspace manifests
        fs::write(temp_path.join("Cargo.toml"), "[workspace]\n")?;
        fs::write(temp_path.join("Cargo.lock"), "# Lock\n")?;

        let snapshot = build_maximalist_snapshot(&crate_dir, temp_path, true)?;

        // Should include workspace manifests
        assert!(snapshot.contains(&"Cargo.toml".into()));
        assert!(snapshot.contains(&"Cargo.lock".into()));

        Ok(())
    }

    #[test]
    fn test_snapshot_deterministic() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let temp_path = Utf8Path::from_path(temp.path()).unwrap();
        let crate_dir = temp_path.join("crates/foo");

        create_test_crate(&crate_dir)?;

        let snapshot1 = build_maximalist_snapshot(&crate_dir, temp_path, false)?;
        let snapshot2 = build_maximalist_snapshot(&crate_dir, temp_path, false)?;

        // Should be identical
        assert_eq!(snapshot1, snapshot2);

        // Should be sorted
        assert_eq!(snapshot1, {
            let mut sorted = snapshot1.clone();
            sorted.sort();
            sorted
        });

        Ok(())
    }
}
