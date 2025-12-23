//! Picante input definitions for incremental builds.

use vx_manifest::Edition;
use vx_oort_proto::Blake3Hash;
use vx_rs::CrateId;

// =============================================================================
// RUST INPUTS
// =============================================================================

/// The complete source closure for a Rust crate (singleton).
///
/// This represents all source files that make up the crate, discovered by
/// parsing `mod` declarations starting from the crate root.
#[picante::input]
pub struct SourceClosure {
    /// Blake3 hash of all source files (paths + contents).
    /// Computed by vx_rs::hash_source_closure().
    pub closure_hash: Blake3Hash,
    /// Workspace-relative paths of all source files in the closure (sorted).
    pub paths: Vec<String>,
}

/// A Rust crate in the dependency graph (keyed by CrateId hex string).
///
/// This input represents a single crate with its metadata and source closure.
/// Used for multi-crate builds where we need to track each crate separately.
#[picante::input]
pub struct RustCrate {
    /// Unique crate identifier (CrateId hex string)
    #[key]
    pub crate_id: String,
    /// Crate name (with hyphens converted to underscores)
    pub crate_name: String,
    /// Rust edition ("2015", "2018", "2021", "2024")
    pub edition: String,
    /// Crate type: "lib" or "bin"
    pub crate_type: String,
    /// Crate root file path, workspace-relative (e.g., "util/src/lib.rs")
    pub crate_root_rel: String,
    /// Blake3 hash of the source closure (paths + contents)
    pub source_closure_hash: Blake3Hash,
}

/// Output from compiling a library crate.
///
/// This input is set AFTER a lib crate is successfully compiled.
/// Bin crates explicitly depend on these outputs via their cache key.
#[picante::input]
pub struct RlibOutput {
    /// CrateId of the lib crate
    #[key]
    pub crate_id: String,
    /// Blake3 hash of the rlib file content
    pub rlib_hash: Blake3Hash,
    /// Workspace-relative path to the materialized rlib
    pub rlib_path: String,
}

/// A dependency edge for bin/rlib cache key computation and reporting.
///
/// This is used to explicitly pass dep outputs to cache key queries and
/// to populate DependencyRecord in build reports.
/// Sorted by extern_name for determinism.
#[derive(Debug, Clone)]
pub struct DepOutput {
    /// Extern crate name (used in --extern)
    pub extern_name: String,
    /// Unique crate identifier
    pub crate_id: CrateId,
    /// Blake3 hash of the dependency's rlib
    pub rlib_hash: Blake3Hash,
    /// Manifest hash from CAS (set after the dep is published)
    pub manifest_hash: Option<Blake3Hash>,
}

/// The parsed Cargo.toml manifest (singleton)
#[picante::input]
pub struct CargoToml {
    /// Blake3 hash of Cargo.toml content
    pub content_hash: Blake3Hash,
    /// Crate name from [package]
    pub name: String,
    /// Edition from [package]
    pub edition: Edition,
    /// Binary target path (e.g., "src/main.rs")
    pub bin_path: String,
}

// =============================================================================
// HERMETIC TOOLCHAIN INPUTS
// =============================================================================

/// Rust toolchain identity (keyed picante input)
///
/// This represents a hermetically acquired Rust toolchain from static.rust-lang.org.
/// The toolchain_id is derived from publisher SHA256s + host/target triples.
///
/// NOTE: Paths are NOT stored here - they are machine-specific and derived
/// at runtime from materialization. Only identity and provenance are recorded.
#[picante::input]
pub struct RustToolchain {
    /// Content-derived toolchain identifier
    #[key]
    pub toolchain_id: Blake3Hash,
    /// CAS blob hash of the ToolchainManifest (for provenance)
    pub toolchain_manifest: Blake3Hash,
    /// Host triple (e.g., "aarch64-apple-darwin")
    pub host: String,
    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target: String,
}

/// Hermetic Rust toolchain manifest reference (singleton)
///
/// This is the manifest hash for the toolchain stored in CAS.
/// Execd will fetch the manifest and materialize the toolchain on-demand.
#[picante::input]
pub struct RustToolchainManifest {
    /// Hash of the ToolchainManifest in CAS
    pub manifest_hash: Blake3Hash,
}

/// Zig toolchain identity (singleton)
///
/// This represents a hermetically acquired Zig toolchain for use as a linker.
/// Using zig cc as the linker provides hermetic, reproducible linking.
#[picante::input]
pub struct ZigToolchain {
    /// Content-derived toolchain identifier (hash of zig exe + lib)
    pub toolchain_id: Blake3Hash,
    /// Zig version string (e.g., "0.13.0")
    pub zig_version: String,
    /// Path to the materialized zig binary
    pub zig_path: String,
}

/// Build configuration (keyed picante input)
#[picante::input]
pub struct BuildConfig {
    /// Composite key derived from profile, target, and workspace
    #[key]
    pub build_key: String,
    /// Profile: "debug" or "release"
    pub profile: String,
    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target_triple: String,
    /// Workspace root (for --remap-path-prefix)
    pub workspace_root: String,
}

impl BuildConfig {
    /// Compute a unique key for this build configuration
    pub fn compute_key(profile: &str, target_triple: &str, workspace_root: &str) -> String {
        // Use a simple concatenation since this is just for keying
        format!("{}:{}:{}", profile, target_triple, workspace_root)
    }
}

// =============================================================================
// C/C++ INPUTS
// =============================================================================

/// A C/C++ source file with its content hash
#[picante::input]
pub struct CSourceFile {
    /// Path relative to workspace root (e.g., "src/main.c")
    #[key]
    pub path: String,
    /// Blake3 hash of the file content
    pub content_hash: Blake3Hash,
}

/// Discovered dependencies for a translation unit (from depfile)
///
/// This is the keystone input for incremental C/C++ builds.
/// On first compile, this is empty. After compile, we parse the depfile
/// and update this input with the discovered headers.
#[picante::input]
pub struct DiscoveredDeps {
    /// Translation unit key (target:source:profile:triple)
    #[key]
    pub tu_key: String,
    /// Hash of all discovered dependency paths (sorted, deduped)
    /// This is a hash of the *paths*, not the *contents*
    pub deps_hash: Blake3Hash,
    /// Workspace-relative paths of all dependencies
    pub deps_paths: Vec<String>,
}

/// Zig toolchain configuration (singleton for now)
#[picante::input]
pub struct ZigToolchainConfig {
    /// Zig version (e.g., "0.13.0")
    pub version: String,
    /// Toolchain ID (content hash of zig binary + lib)
    pub toolchain_id: String,
}

/// A C target to link (for keyed query)
#[picante::input]
pub struct CTarget {
    /// Target name (e.g., "hello")
    #[key]
    pub name: String,
    /// Object file hashes (sorted by source path for determinism)
    pub object_hashes: Vec<Blake3Hash>,
    /// Source files that produced these objects
    pub source_paths: Vec<String>,
}
