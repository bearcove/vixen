//! Cache key computation for Rust compilation nodes
//!
//! Cache keys are based on observed_inputs (not snapshot_inputs) to ensure
//! correctness. The key is computed from:
//! - Toolchain manifest hash (hermetic toolchain)
//! - Rustc invocation args (normalized)
//! - Observed inputs (workspace files → blob hashes, toolchain tracked via manifest)
//!
//! ## Invariants
//! - Cache keys MUST be based on observed_inputs (authoritative from dep-info)
//! - Snapshot inputs are transport only, NOT part of cache key
//! - First build may be cache miss (no observed_inputs yet)

use blake3::Hasher;
use camino::Utf8Path;
use std::collections::BTreeMap;

use crate::input_set::{InputSet, WorkspaceFile};

/// Compute a cache key for a Rust compilation node
///
/// The cache key is deterministic and based on:
/// 1. Toolchain manifest hash (already a Blake3 hash)
/// 2. Normalized rustc arguments
/// 3. Observed inputs from dep-info
///
/// Returns a Blake3 hash string (hex-encoded)
pub fn compute_cache_key(
    toolchain_manifest: &str,
    rustc_args: &[String],
    observed_inputs: &InputSet,
    workspace_file_hashes: &BTreeMap<String, String>,
) -> String {
    let mut hasher = Hasher::new();

    // 1. Hash toolchain manifest (already a Blake3 hash)
    hasher.update(toolchain_manifest.as_bytes());

    // 2. Hash normalized rustc args (sorted for determinism)
    let mut sorted_args = rustc_args.to_vec();
    sorted_args.sort();
    for arg in &sorted_args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0"); // Separator
    }

    // 3. Hash observed workspace files (path + content hash)
    // Sort by path for determinism
    let mut workspace_files: Vec<&WorkspaceFile> = observed_inputs.workspace_files.iter().collect();
    workspace_files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    for file in workspace_files {
        hasher.update(file.rel_path.as_bytes());
        hasher.update(b":");

        // Look up the content hash for this file
        if let Some(content_hash) = workspace_file_hashes.get(&file.rel_path) {
            hasher.update(content_hash.as_bytes());
        } else {
            // File not in workspace_file_hashes - this shouldn't happen
            // but we'll include the path anyway
            hasher.update(b"<missing>");
        }
        hasher.update(b"\0");
    }

    // 4. Hash generated files (by ID)
    let mut generated_files: Vec<_> = observed_inputs
        .generated_files
        .iter()
        .map(|g| &g.id)
        .collect();
    generated_files.sort();

    for id in generated_files {
        hasher.update(b"generated:");
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
    }

    // 5. Hash declared external inputs
    let mut external_inputs: Vec<_> = observed_inputs
        .declared_external
        .iter()
        .map(|e| &e.path)
        .collect();
    external_inputs.sort();

    for path in external_inputs {
        hasher.update(b"external:");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
    }

    // Note: toolchain_files are NOT hashed separately because they're
    // already covered by the toolchain_manifest hash

    hex::encode(hasher.finalize().as_bytes())
}

/// Compute hash of snapshot inputs for transport tracking
///
/// This is NOT used for cache keys, only for tracking what we sent to execd.
/// The snapshot is intentionally a superset (maximalist), so using it for
/// cache keys would cause false misses.
pub fn compute_snapshot_hash(snapshot_files: &[(String, String)]) -> String {
    let mut hasher = Hasher::new();

    // Sort by path for determinism
    let mut sorted: Vec<_> = snapshot_files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, blob_hash) in sorted {
        hasher.update(path.as_bytes());
        hasher.update(b":");
        hasher.update(blob_hash.as_bytes());
        hasher.update(b"\0");
    }

    hex::encode(hasher.finalize().as_bytes())
}

/// Hash the content of a workspace file using Blake3
///
/// This is used to build the workspace_file_hashes map for cache key computation.
pub fn hash_file_content(content: &[u8]) -> String {
    let hash = blake3::hash(content);
    hex::encode(hash.as_bytes())
}

/// Hash a file at a given path
///
/// Convenience wrapper around hash_file_content that reads the file.
pub fn hash_file(path: &Utf8Path) -> std::io::Result<String> {
    let content = std::fs::read(path)?;
    Ok(hash_file_content(&content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_deterministic() {
        let toolchain = "abc123".to_string();
        let args = vec!["--crate-type".to_string(), "lib".to_string()];

        let mut input_set = InputSet::new();
        input_set.add_workspace_file("src/lib.rs".to_string());
        input_set.add_workspace_file("src/mod.rs".to_string());

        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("src/lib.rs".to_string(), "hash1".to_string());
        file_hashes.insert("src/mod.rs".to_string(), "hash2".to_string());

        let key1 = compute_cache_key(&toolchain, &args, &input_set, &file_hashes);
        let key2 = compute_cache_key(&toolchain, &args, &input_set, &file_hashes);

        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 64); // Blake3 hash is 32 bytes = 64 hex chars
    }

    #[test]
    fn test_cache_key_changes_with_toolchain() {
        let args = vec!["--crate-type".to_string(), "lib".to_string()];
        let input_set = InputSet::new();
        let file_hashes = BTreeMap::new();

        let key1 = compute_cache_key("toolchain1", &args, &input_set, &file_hashes);
        let key2 = compute_cache_key("toolchain2", &args, &input_set, &file_hashes);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_changes_with_args() {
        let toolchain = "abc123";
        let input_set = InputSet::new();
        let file_hashes = BTreeMap::new();

        let args1 = vec!["--crate-type".to_string(), "lib".to_string()];
        let args2 = vec!["--crate-type".to_string(), "bin".to_string()];

        let key1 = compute_cache_key(toolchain, &args1, &input_set, &file_hashes);
        let key2 = compute_cache_key(toolchain, &args2, &input_set, &file_hashes);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_changes_with_file_content() {
        let toolchain = "abc123";
        let args = vec!["--crate-type".to_string(), "lib".to_string()];

        let mut input_set = InputSet::new();
        input_set.add_workspace_file("src/lib.rs".to_string());

        let mut file_hashes1 = BTreeMap::new();
        file_hashes1.insert("src/lib.rs".to_string(), "hash1".to_string());

        let mut file_hashes2 = BTreeMap::new();
        file_hashes2.insert("src/lib.rs".to_string(), "hash2".to_string());

        let key1 = compute_cache_key(toolchain, &args, &input_set, &file_hashes1);
        let key2 = compute_cache_key(toolchain, &args, &input_set, &file_hashes2);

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_arg_order_independent() {
        let toolchain = "abc123";
        let input_set = InputSet::new();
        let file_hashes = BTreeMap::new();

        // Args in different order should produce same key (we sort them)
        let args1 = vec!["--crate-type".to_string(), "lib".to_string()];
        let args2 = vec!["lib".to_string(), "--crate-type".to_string()];

        let key1 = compute_cache_key(toolchain, &args1, &input_set, &file_hashes);
        let key2 = compute_cache_key(toolchain, &args2, &input_set, &file_hashes);

        assert_eq!(key1, key2);
    }

    #[test]
    fn test_snapshot_hash_deterministic() {
        let files = vec![
            ("src/lib.rs".to_string(), "hash1".to_string()),
            ("src/mod.rs".to_string(), "hash2".to_string()),
        ];

        let hash1 = compute_snapshot_hash(&files);
        let hash2 = compute_snapshot_hash(&files);

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }

    #[test]
    fn test_snapshot_hash_file_order_independent() {
        let files1 = vec![
            ("src/lib.rs".to_string(), "hash1".to_string()),
            ("src/mod.rs".to_string(), "hash2".to_string()),
        ];

        let files2 = vec![
            ("src/mod.rs".to_string(), "hash2".to_string()),
            ("src/lib.rs".to_string(), "hash1".to_string()),
        ];

        let hash1 = compute_snapshot_hash(&files1);
        let hash2 = compute_snapshot_hash(&files2);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_file_content() {
        let content1 = b"fn main() {}";
        let content2 = b"fn main() { }"; // Different whitespace

        let hash1 = hash_file_content(content1);
        let hash2 = hash_file_content(content2);

        assert_ne!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
        assert_eq!(hash2.len(), 64);
    }
}
