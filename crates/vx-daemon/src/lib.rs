//! vx-daemon: Orchestration daemon with picante incremental queries
//!
//! This daemon:
//! - Owns the picante incremental computation runtime
//! - Parses Cargo.toml and sets up inputs
//! - Computes cache keys via tracked queries
//! - Orchestrates builds via CAS and Exec services

use std::process::Command;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use picante::persist::{CacheLoadOptions, OnCorruptCache, load_cache_with_options, save_cache};
use picante::{HasRuntime, PicanteResult};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, CacheKey, Cas, NodeId, NodeManifest, OutputEntry};
use vx_casd::CasService;
use vx_daemon_proto::{BuildRequest, BuildResult, Daemon};
use vx_exec_proto::{ExpectedOutput, RustcInvocation};
use vx_manifest::{Edition, Manifest};
use vx_report::{
    BuildReport, CacheOutcome, DiagnosticsRecord, InputRecord, InvocationRecord, MissReason,
    NodeReport, NodeTiming, OutputRecord, ReportStore, ToolchainRef, ToolchainsUsed,
};
use vx_rs::ModuleError;

// =============================================================================
// INPUTS
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

// =============================================================================
// MULTI-CRATE RUST INPUTS
// =============================================================================

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

/// A dependency edge for bin crate cache key computation.
///
/// This is used to explicitly pass dep outputs to the bin cache key query.
/// Sorted by extern_name for determinism.
#[derive(Debug, Clone)]
pub struct DepOutput {
    /// Extern crate name (used in --extern)
    pub extern_name: String,
    /// Blake3 hash of the dependency's rlib
    pub rlib_hash: Blake3Hash,
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

/// The rustc version info (singleton)
#[picante::input]
pub struct RustcVersion {
    /// Full `rustc -vV` output (includes version, commit, LLVM version, host)
    pub version_string: String,
}

/// Build configuration (singleton)
#[picante::input]
pub struct BuildConfig {
    /// Profile: "debug" or "release"
    pub profile: String,
    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target_triple: String,
    /// Workspace root (for --remap-path-prefix)
    pub workspace_root: String,
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

// =============================================================================
// TRACKED QUERIES
// =============================================================================

/// Compute the cache key for compiling a binary crate.
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(db: &DB) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_bin: COMPUTING (not memoized)");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let rustc = RustcVersion::get(db)?.expect("RustcVersion not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let closure = SourceClosure::get(db)?.expect("SourceClosure not set");

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rustc:");
    hasher.update(rustc.version_string.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"name:");
    hasher.update(cargo.name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(cargo.edition.as_str().as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    // Source closure hash includes all source file paths and contents
    hasher.update(b"source_closure:");
    hasher.update(&closure.closure_hash.0);
    hasher.update(b"\n");

    hasher.update(b"manifest:");
    hasher.update(&cargo.content_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Build a rustc invocation for compiling a binary crate.
#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(db: &DB) -> PicanteResult<RustcInvocation> {
    debug!("plan_compile_bin: COMPUTING (not memoized)");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let output_dir = format!(".vx/build/{}/{}", config.target_triple, config.profile);
    let output_path = format!("{}/{}", output_dir, cargo.name);

    let mut args = vec![
        "--crate-name".to_string(),
        cargo.name.clone(),
        "--crate-type".to_string(),
        "bin".to_string(),
        "--edition".to_string(),
        cargo.edition.as_str().to_string(),
        format!(
            "--remap-path-prefix={}=/vx-workspace",
            config.workspace_root
        ),
        cargo.bin_path.clone(),
        "-o".to_string(),
        output_path.clone(),
    ];

    if config.profile == "release" {
        args.push("-C".to_string());
        args.push("opt-level=3".to_string());
    }

    let expected_outputs = vec![ExpectedOutput {
        logical: "bin".to_string(),
        path: output_path,
        executable: true,
    }];

    Ok(RustcInvocation {
        program: "rustc".to_string(),
        args,
        env: vec![],
        cwd: config.workspace_root.clone(),
        expected_outputs,
    })
}

/// Generate a human-readable node ID for a compile-bin node.
#[picante::tracked]
pub async fn node_id_compile_bin<DB: Db>(db: &DB) -> PicanteResult<NodeId> {
    debug!("node_id_compile_bin: COMPUTING (not memoized)");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    Ok(NodeId(format!(
        "compile-bin:{}:{}:{}",
        cargo.name, config.target_triple, config.profile
    )))
}

// =============================================================================
// MULTI-CRATE RUST TRACKED QUERIES
// =============================================================================

/// Compute the cache key for compiling a library crate (rlib).
///
/// The cache key includes:
/// - Toolchain (rustc version)
/// - Target triple and profile
/// - Crate metadata (name, edition)
/// - Source closure hash
///
/// Note: Lib crates don't depend on other crates in v0.3 (no transitive deps).
#[picante::tracked]
pub async fn cache_key_compile_rlib<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_rlib: COMPUTING (not memoized)");

    let rustc = RustcVersion::get(db)?.expect("RustcVersion not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-rlib-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rustc:");
    hasher.update(rustc.version_string.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:lib\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Plan a rustc invocation for compiling a library crate to rlib.
///
/// All paths are workspace-relative. rustc is invoked with cwd = workspace_root.
#[picante::tracked]
pub async fn plan_compile_rlib<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<RustcInvocation> {
    debug!("plan_compile_rlib: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_id = crate_info.crate_id(db)?;
    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let crate_root = crate_info.crate_root_rel(db)?;

    // Output path: .vx/build/{triple}/{profile}/deps/{crate_id}/lib{name}.rlib
    let output_dir = format!(
        ".vx/build/{}/{}/deps/{}",
        &*config.target_triple, &*config.profile, &*crate_id
    );
    let rlib_filename = format!("lib{}.rlib", &*crate_name);
    let output_path = format!("{}/{}", output_dir, rlib_filename);

    let mut args = vec![
        "--crate-name".to_string(),
        crate_name.to_string(),
        "--crate-type".to_string(),
        "lib".to_string(),
        "--edition".to_string(),
        edition.to_string(),
        format!(
            "--remap-path-prefix={}=/vx-workspace",
            &*config.workspace_root
        ),
        crate_root.to_string(),
        "-o".to_string(),
        output_path.clone(),
    ];

    if &*config.profile == "release" {
        args.push("-C".to_string());
        args.push("opt-level=3".to_string());
    }

    let expected_outputs = vec![ExpectedOutput {
        logical: "rlib".to_string(),
        path: output_path,
        executable: false,
    }];

    Ok(RustcInvocation {
        program: "rustc".to_string(),
        args,
        env: vec![],
        cwd: config.workspace_root.to_string(),
        expected_outputs,
    })
}

/// Generate a human-readable node ID for a compile-rlib node.
#[picante::tracked]
pub async fn node_id_compile_rlib<DB: Db>(db: &DB, crate_info: RustCrate) -> PicanteResult<NodeId> {
    debug!("node_id_compile_rlib: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let crate_name = crate_info.crate_name(db)?;

    Ok(NodeId(format!(
        "compile-rlib:{}:{}:{}",
        &*crate_name, &*config.target_triple, &*config.profile
    )))
}

/// Compute the cache key for compiling a binary crate with dependencies.
///
/// CRITICAL: The cache key explicitly includes dep_outputs (sorted by extern_name).
/// This ensures that changes to dependencies invalidate the bin cache key.
///
/// The dep_outputs parameter must be passed explicitly - NOT looked up implicitly.
/// This makes the dependency explicit in the picante query graph.
#[picante::tracked]
pub async fn cache_key_compile_bin_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_rlib_hashes: Vec<(String, Blake3Hash)>, // (extern_name, rlib_hash) sorted
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_bin_with_deps: COMPUTING (not memoized)");

    let rustc = RustcVersion::get(db)?.expect("RustcVersion not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-bin-deps-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rustc:");
    hasher.update(rustc.version_string.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure_hash.0);
    hasher.update(b"\n");

    // CRITICAL: Hash all dependency outputs (sorted by extern_name)
    hasher.update(b"deps:");
    for (extern_name, rlib_hash) in &dep_rlib_hashes {
        hasher.update(extern_name.as_bytes());
        hasher.update(b"=");
        hasher.update(&rlib_hash.0);
        hasher.update(b";");
    }
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Plan a rustc invocation for compiling a binary crate with dependencies.
///
/// All paths are workspace-relative. rustc is invoked with cwd = workspace_root.
/// Dependency rlibs are passed via --extern flags.
#[picante::tracked]
pub async fn plan_compile_bin_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_extern_paths: Vec<(String, String)>, // (extern_name, workspace-rel rlib path) sorted
) -> PicanteResult<RustcInvocation> {
    debug!("plan_compile_bin_with_deps: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let crate_root = crate_info.crate_root_rel(db)?;

    // Output path: .vx/build/{triple}/{profile}/{bin_name}
    let output_dir = format!(".vx/build/{}/{}", &*config.target_triple, &*config.profile);
    let output_path = format!("{}/{}", output_dir, &*crate_name);

    let mut args = vec![
        "--crate-name".to_string(),
        crate_name.to_string(),
        "--crate-type".to_string(),
        "bin".to_string(),
        "--edition".to_string(),
        edition.to_string(),
        format!(
            "--remap-path-prefix={}=/vx-workspace",
            &*config.workspace_root
        ),
        crate_root.to_string(),
        "-o".to_string(),
        output_path.clone(),
    ];

    // Add --extern flags for dependencies (sorted for determinism)
    for (extern_name, rlib_path) in &dep_extern_paths {
        args.push("--extern".to_string());
        args.push(format!("{}={}", extern_name, rlib_path));
    }

    if &*config.profile == "release" {
        args.push("-C".to_string());
        args.push("opt-level=3".to_string());
    }

    let expected_outputs = vec![ExpectedOutput {
        logical: "bin".to_string(),
        path: output_path,
        executable: true,
    }];

    Ok(RustcInvocation {
        program: "rustc".to_string(),
        args,
        env: vec![],
        cwd: config.workspace_root.to_string(),
        expected_outputs,
    })
}

/// Generate a human-readable node ID for a compile-bin-with-deps node.
#[picante::tracked]
pub async fn node_id_compile_bin_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<NodeId> {
    debug!("node_id_compile_bin_with_deps: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let crate_name = crate_info.crate_name(db)?;

    Ok(NodeId(format!(
        "compile-bin:{}:{}:{}",
        &*crate_name, &*config.target_triple, &*config.profile
    )))
}

// =============================================================================
// C/C++ TRACKED QUERIES
// =============================================================================

/// Compute the cache key for compiling a C/C++ translation unit.
///
/// The cache key includes:
/// - Toolchain ID (content-addressed zig binary + lib)
/// - Source file hash
/// - Discovered dependencies hash (from previous compile, or empty sentinel)
/// - Compile flags hash
/// - Target triple and profile
#[picante::tracked]
pub async fn cache_key_cc_compile<DB: Db>(db: &DB, source: CSourceFile) -> PicanteResult<CacheKey> {
    debug!("cache_key_cc_compile: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let zig = ZigToolchainConfig::get(db)?.expect("ZigToolchainConfig not set");
    let source_hash = source.content_hash(db)?;
    let source_path = source.path(db)?;

    // Build TU key for looking up discovered deps
    let tu_key = format!(
        "cc:{}:{}:{}",
        &*source_path, &*config.profile, &*config.target_triple
    );

    // Get discovered deps (may not exist on first compile)
    // First try to intern the key, then look up the data
    let deps_hash = match db.discovered_deps_keys().intern(tu_key.clone()) {
        Ok(intern_id) => {
            // Key exists, look up the data
            if let Some(data) = db.discovered_deps_data().get(db, &intern_id)? {
                data.deps_hash.clone()
            } else {
                // Key interned but no data yet
                Blake3Hash::from_bytes(b"no-deps-yet")
            }
        }
        Err(_) => {
            // First compile - use empty sentinel
            Blake3Hash::from_bytes(b"no-deps-yet")
        }
    };

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cc-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(zig.toolchain_id.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"source:");
    hasher.update(&source_hash.0);
    hasher.update(b"\n");

    hasher.update(b"deps:");
    hasher.update(&deps_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Plan a C/C++ compile invocation.
///
/// Returns the command to run zig cc with appropriate flags.
#[picante::tracked]
pub async fn plan_cc_compile<DB: Db>(
    db: &DB,
    source: CSourceFile,
) -> PicanteResult<vx_cc::CcCompileInvocation> {
    debug!("plan_cc_compile: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let source_path = source.path(db)?;

    // Derive output paths (dereference Arc<String> values)
    let source_path_str: &str = &source_path;
    let source_stem = Utf8PathBuf::from(source_path_str)
        .file_stem()
        .unwrap_or("out")
        .to_string();
    let output_dir = format!(
        ".vx/build/cc/{}/{}",
        &*config.target_triple, &*config.profile
    );
    let object_path = format!("{}/{}.o", output_dir, source_stem);
    let depfile_path = format!("{}/{}.d", output_dir, source_stem);

    // Build arguments
    let mut args = vec![
        "cc".to_string(),
        "-target".to_string(),
        config.target_triple.to_string(),
        format!(
            "-fdebug-prefix-map={}=/vx-workspace",
            &*config.workspace_root
        ),
        "-MMD".to_string(),
        "-MF".to_string(),
        depfile_path.clone(),
        "-c".to_string(),
        source_path_str.to_string(),
        "-o".to_string(),
        object_path.clone(),
    ];

    // Profile-specific flags
    if &*config.profile == "release" {
        args.push("-O2".to_string());
    } else {
        args.push("-O0".to_string());
        args.push("-g".to_string());
    }

    // Standard warnings
    args.push("-Wall".to_string());
    args.push("-Wextra".to_string());

    let expected_outputs = vec![
        vx_cc::ExpectedOutput {
            logical: "obj".to_string(),
            path: Utf8PathBuf::from(&object_path),
            executable: false,
        },
        vx_cc::ExpectedOutput {
            logical: "depfile".to_string(),
            path: Utf8PathBuf::from(&depfile_path),
            executable: false,
        },
    ];

    Ok(vx_cc::CcCompileInvocation {
        program: Utf8PathBuf::from("zig"), // Will be replaced with materialized path
        args,
        env: vec![],
        cwd: Utf8PathBuf::from(&*config.workspace_root),
        expected_outputs,
        depfile: Some(Utf8PathBuf::from(depfile_path)),
    })
}

/// Generate a human-readable node ID for a cc-compile node.
#[picante::tracked]
pub async fn node_id_cc_compile<DB: Db>(db: &DB, source: CSourceFile) -> PicanteResult<NodeId> {
    debug!("node_id_cc_compile: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let source_path = source.path(db)?;

    Ok(NodeId(format!(
        "cc-compile:{}:{}:{}",
        &*source_path, &*config.target_triple, &*config.profile
    )))
}

// =============================================================================
// C/C++ LINK TRACKED QUERIES
// =============================================================================

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

/// Compute the cache key for linking a C/C++ target.
///
/// The cache key includes:
/// - Toolchain ID
/// - All object file hashes (in deterministic order)
/// - Link flags
/// - Target triple and profile
#[picante::tracked]
pub async fn cache_key_cc_link<DB: Db>(db: &DB, target: CTarget) -> PicanteResult<CacheKey> {
    debug!("cache_key_cc_link: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let zig = ZigToolchainConfig::get(db)?.expect("ZigToolchainConfig not set");
    let object_hashes = target.object_hashes(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cc-link-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(zig.toolchain_id.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    // Hash all object files in order
    hasher.update(b"objects:");
    for obj_hash in object_hashes.iter() {
        hasher.update(&obj_hash.0);
    }
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Plan a C/C++ link invocation.
///
/// Returns the command to run zig cc for linking.
#[picante::tracked]
pub async fn plan_cc_link<DB: Db>(
    db: &DB,
    target: CTarget,
) -> PicanteResult<vx_cc::CcLinkInvocation> {
    debug!("plan_cc_link: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let target_name = target.name(db)?;
    let source_paths = target.source_paths(db)?;

    // Derive output paths
    let output_dir = format!(
        ".vx/build/cc/{}/{}",
        &*config.target_triple, &*config.profile
    );
    let exe_path = format!("{}/{}", output_dir, &*target_name);

    // Build object file paths from source paths
    let object_paths: Vec<String> = source_paths
        .iter()
        .map(|src| {
            let stem = Utf8PathBuf::from(src.as_str())
                .file_stem()
                .unwrap_or("out")
                .to_string();
            format!("{}/{}.o", output_dir, stem)
        })
        .collect();

    // Build arguments
    let mut args = vec![
        "cc".to_string(),
        "-target".to_string(),
        config.target_triple.to_string(),
    ];

    // Add object files
    for obj in &object_paths {
        args.push(obj.clone());
    }

    // Output
    args.push("-o".to_string());
    args.push(exe_path.clone());

    // Profile-specific flags
    if &*config.profile == "release" {
        args.push("-s".to_string()); // Strip symbols
    }

    let expected_outputs = vec![vx_cc::ExpectedOutput {
        logical: "exe".to_string(),
        path: Utf8PathBuf::from(&exe_path),
        executable: true,
    }];

    Ok(vx_cc::CcLinkInvocation {
        program: Utf8PathBuf::from("zig"), // Will be replaced with materialized path
        args,
        env: vec![],
        cwd: Utf8PathBuf::from(&*config.workspace_root),
        expected_outputs,
    })
}

/// Generate a human-readable node ID for a cc-link node.
#[picante::tracked]
pub async fn node_id_cc_link<DB: Db>(db: &DB, target: CTarget) -> PicanteResult<NodeId> {
    debug!("node_id_cc_link: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let target_name = target.name(db)?;

    Ok(NodeId(format!(
        "cc-link:{}:{}:{}",
        &*target_name, &*config.target_triple, &*config.profile
    )))
}

// =============================================================================
// DATABASE
// =============================================================================

#[picante::db(
    inputs(
        SourceClosure,
        CargoToml,
        RustcVersion,
        BuildConfig,
        // Multi-crate Rust inputs
        RustCrate,
        RlibOutput,
        // C/C++ inputs
        CSourceFile,
        DiscoveredDeps,
        ZigToolchainConfig,
        CTarget,
    ),
    tracked(
        // Single-crate Rust (legacy, for backward compat)
        cache_key_compile_bin,
        plan_compile_bin,
        node_id_compile_bin,
        // Multi-crate Rust
        cache_key_compile_rlib,
        plan_compile_rlib,
        node_id_compile_rlib,
        cache_key_compile_bin_with_deps,
        plan_compile_bin_with_deps,
        node_id_compile_bin_with_deps,
        // C/C++
        cache_key_cc_compile,
        plan_cc_compile,
        node_id_cc_compile,
        cache_key_cc_link,
        plan_cc_link,
        node_id_cc_link,
    ),
    db_trait(Db)
)]
pub struct Database {}

// =============================================================================
// DAEMON SERVICE
// =============================================================================

/// The daemon service implementation
pub struct DaemonService {
    /// CAS service for content-addressed storage
    cas: CasService,
    /// The picante incremental database (shared across builds)
    db: Arc<Mutex<Database>>,
    /// Path to the picante cache file
    cache_path: Utf8PathBuf,
}

impl DaemonService {
    /// Create a new daemon service
    pub fn new(vx_home: Utf8PathBuf) -> std::io::Result<Self> {
        let cas_root = vx_home.clone();
        let cas = CasService::new(cas_root);
        cas.init()?;

        let db = Database::new();
        let cache_path = vx_home.join("picante.cache");

        Ok(Self {
            cas,
            db: Arc::new(Mutex::new(db)),
            cache_path,
        })
    }

    /// Load picante cache from disk (call once at startup)
    pub async fn load_cache(&self) -> eyre::Result<bool> {
        let db = self.db.lock().await;
        let ingredients = db.ingredient_registry().persistable_ingredients();

        let options = CacheLoadOptions {
            max_bytes: None,
            on_corrupt: OnCorruptCache::Delete, // Delete corrupt caches, don't fail
        };

        match load_cache_with_options(&self.cache_path, db.runtime(), &ingredients, &options).await
        {
            Ok(true) => {
                info!(path = %self.cache_path, "loaded picante cache");
                Ok(true)
            }
            Ok(false) => {
                debug!(path = %self.cache_path, "no picante cache found");
                Ok(false)
            }
            Err(e) => {
                warn!(path = %self.cache_path, error = %e, "failed to load picante cache");
                Err(eyre::eyre!("failed to load picante cache: {}", e))
            }
        }
    }

    /// Save picante cache to disk
    async fn save_cache(&self) -> eyre::Result<()> {
        let db = self.db.lock().await;
        let ingredients = db.ingredient_registry().persistable_ingredients();

        save_cache(&self.cache_path, db.runtime(), &ingredients)
            .await
            .map_err(|e| eyre::eyre!("failed to save picante cache: {}", e))?;

        debug!(path = %self.cache_path, "saved picante cache");
        Ok(())
    }

    /// Internal build implementation
    async fn do_build(&self, request: BuildRequest) -> Result<BuildResult, String> {
        let project_path = &request.project_path;
        let cargo_toml_path = project_path.join("Cargo.toml");

        if !cargo_toml_path.exists() {
            return Ok(BuildResult {
                success: false,
                message: format!("no Cargo.toml found in {}", project_path),
                cached: false,
                duration_ms: 0,
                output_path: None,
                error: Some("Cargo.toml not found".to_string()),
            });
        }

        // Parse manifest
        let manifest = Manifest::from_path(&cargo_toml_path)
            .map_err(|e| format!("failed to parse Cargo.toml: {}", e))?;

        // Get rustc version and target triple
        let rustc_version = get_rustc_version().map_err(|e| e.to_string())?;
        let target_triple = get_target_triple().map_err(|e| e.to_string())?;

        let profile = if request.release { "release" } else { "debug" };

        // Initialize build report
        let mut build_report = BuildReport::new(
            project_path.to_string(),
            profile.to_string(),
            target_triple.clone(),
        );

        // Record toolchain info
        build_report.toolchains = ToolchainsUsed {
            rust: Some(ToolchainRef {
                id: Blake3Hash::from_bytes(rustc_version.as_bytes()).to_hex(),
                version: extract_rustc_version(&rustc_version),
            }),
            zig: None, // Not used for Rust builds
        };

        // Get the binary target (required for single-crate builds)
        let bin_target = manifest.bin.as_ref().ok_or_else(|| {
            "no binary target found (this build path requires a bin crate)".to_string()
        })?;

        // Compute source closure (all .rs files in the crate)
        let crate_root = &bin_target.path;
        let closure_paths = vx_rs::rust_source_closure(crate_root, project_path).map_err(|e| {
            let error_msg = format_module_error(&e);
            build_report.finalize(false, Some(error_msg.clone()));
            self.save_report(project_path, &build_report);
            error_msg
        })?;

        // Hash the closure (paths + contents)
        let closure_hash = vx_rs::hash_source_closure(&closure_paths, project_path)
            .map_err(|e| format!("failed to hash source closure: {}", e))?;

        // Hash Cargo.toml
        let cargo_toml_content = std::fs::read(&cargo_toml_path)
            .map_err(|e| format!("failed to read Cargo.toml: {}", e))?;
        let cargo_toml_hash = Blake3Hash::from_bytes(&cargo_toml_content);

        // Use the shared picante database
        let db = self.db.lock().await;

        // Set inputs
        let closure_paths_strings: Vec<String> =
            closure_paths.iter().map(|p| p.to_string()).collect();
        SourceClosure::set(&*db, closure_hash.clone(), closure_paths_strings)
            .map_err(|e| format!("picante error: {}", e))?;

        // Singletons: Type::set(db, field1, field2, ...)
        CargoToml::set(
            &*db,
            cargo_toml_hash.clone(),
            manifest.name.clone(),
            manifest.edition,
            bin_target.path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        RustcVersion::set(&*db, rustc_version.clone())
            .map_err(|e| format!("picante error: {}", e))?;

        BuildConfig::set(
            &*db,
            profile.to_string(),
            target_triple.clone(),
            project_path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Compute cache key via picante
        let cache_key = cache_key_compile_bin(&*db)
            .await
            .map_err(|e| format!("failed to compute cache key: {}", e))?;

        // Build output directory
        let output_dir = project_path
            .join(".vx/build")
            .join(&target_triple)
            .join(profile);
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| format!("failed to create output dir: {}", e))?;

        let output_path = output_dir.join(&manifest.name);

        // Build the inputs list for the report
        let inputs = vec![
            InputRecord {
                label: "source_closure".to_string(),
                value: closure_hash.to_hex(),
            },
            InputRecord {
                label: "Cargo.toml".to_string(),
                value: cargo_toml_hash.to_hex(),
            },
            InputRecord {
                label: "rustc".to_string(),
                value: Blake3Hash::from_bytes(rustc_version.as_bytes()).to_hex(),
            },
            InputRecord {
                label: "target".to_string(),
                value: target_triple.clone(),
            },
            InputRecord {
                label: "profile".to_string(),
                value: profile.to_string(),
            },
        ];

        // Check cache
        if let Some(cached_manifest_hash) = self.cas.lookup(cache_key.clone()).await
            && let Some(cached_manifest) = self.cas.get_manifest(cached_manifest_hash.clone()).await
        {
            // Cache hit - materialize outputs
            for output in &cached_manifest.outputs {
                if let Some(blob_data) = self.cas.get_blob(output.blob.clone()).await {
                    let dest_path = output_dir.join(&output.filename);
                    atomic_write_file(&dest_path, &blob_data, output.executable)
                        .map_err(|e| format!("failed to write {}: {}", dest_path, e))?;
                } else {
                    return Err(format!("blob {} not found in CAS", output.blob.to_hex()));
                }
            }

            // Record cache hit in report
            let node_report = NodeReport {
                node_id: cached_manifest.node_id.0.clone(),
                kind: "rust.compile_bin".to_string(),
                cache_key: cache_key.to_hex(),
                cache: CacheOutcome::Hit {
                    manifest: cached_manifest_hash.to_hex(),
                },
                timing: NodeTiming {
                    queued_ms: 0,
                    execute_ms: 0,
                },
                inputs,
                outputs: cached_manifest
                    .outputs
                    .iter()
                    .map(|o| OutputRecord {
                        logical: o.logical.clone(),
                        manifest: Some(cached_manifest_hash.to_hex()),
                        blob: Some(o.blob.to_hex()),
                        path: Some(format!(
                            ".vx/build/{}/{}/{}",
                            target_triple, profile, o.filename
                        )),
                    })
                    .collect(),
                invocation: None,
                diagnostics: DiagnosticsRecord::default(),
            };
            build_report.add_node(node_report);
            build_report.finalize(true, None);
            self.save_report(project_path, &build_report);

            return Ok(BuildResult {
                success: true,
                message: format!("{} {} (cached)", manifest.name, profile),
                cached: true,
                duration_ms: 0,
                output_path: Some(output_path),
                error: None,
            });
        }

        // Cache miss - get build plan from picante
        let invocation = plan_compile_bin(&*db)
            .await
            .map_err(|e| format!("failed to plan build: {}", e))?;

        let node_id = node_id_compile_bin(&*db)
            .await
            .map_err(|e| format!("failed to get node id: {}", e))?;

        // Release the db lock before executing rustc (which can take a while)
        drop(db);

        // Execute rustc
        let start = std::time::Instant::now();
        let output = Command::new(&invocation.program)
            .args(&invocation.args)
            .current_dir(&invocation.cwd)
            .output()
            .map_err(|e| format!("failed to execute rustc: {}", e))?;

        let duration = start.elapsed();

        // Store stdout/stderr in CAS (even for failures)
        let stdout_blob = if !output.stdout.is_empty() {
            Some(self.cas.put_blob(output.stdout.clone()).await)
        } else {
            None
        };
        let stderr_blob = if !output.stderr.is_empty() {
            Some(self.cas.put_blob(output.stderr.clone()).await)
        } else {
            None
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Record failed build in report
            let node_report = NodeReport {
                node_id: node_id.0.clone(),
                kind: "rust.compile_bin".to_string(),
                cache_key: cache_key.to_hex(),
                cache: CacheOutcome::Miss {
                    reason: MissReason::KeyNotFound,
                },
                timing: NodeTiming {
                    queued_ms: 0,
                    execute_ms: duration.as_millis() as u64,
                },
                inputs,
                outputs: vec![],
                invocation: Some(InvocationRecord {
                    program: invocation.program.clone(),
                    args: invocation.args.clone(),
                    cwd: invocation.cwd.clone(),
                    exit_code: output.status.code().unwrap_or(-1),
                }),
                diagnostics: DiagnosticsRecord {
                    stdout_blob: stdout_blob.map(|h| h.to_hex()),
                    stderr_blob: stderr_blob.map(|h| h.to_hex()),
                },
            };
            build_report.add_node(node_report);
            build_report.finalize(false, Some(stderr.to_string()));
            self.save_report(project_path, &build_report);

            return Ok(BuildResult {
                success: false,
                message: "rustc failed".to_string(),
                cached: false,
                duration_ms: duration.as_millis() as u64,
                output_path: None,
                error: Some(stderr.to_string()),
            });
        }

        // Store output in CAS
        let binary_data = std::fs::read(&output_path)
            .map_err(|e| format!("failed to read output {}: {}", output_path, e))?;
        let blob_hash = self.cas.put_blob(binary_data).await;

        // Create manifest
        let node_manifest = NodeManifest {
            node_id: node_id.clone(),
            cache_key: cache_key.clone(),
            produced_at: unix_timestamp(),
            outputs: vec![OutputEntry {
                logical: "bin".to_string(),
                filename: manifest.name.clone(),
                blob: blob_hash.clone(),
                executable: true,
            }],
        };

        let manifest_hash = self.cas.put_manifest(node_manifest).await;

        // Publish to cache
        let publish_result = self
            .cas
            .publish(cache_key.clone(), manifest_hash.clone())
            .await;
        if !publish_result.success {
            return Err(format!(
                "failed to publish cache entry: {:?}",
                publish_result.error
            ));
        }

        // Record successful build in report
        let node_report = NodeReport {
            node_id: node_id.0.clone(),
            kind: "rust.compile_bin".to_string(),
            cache_key: cache_key.to_hex(),
            cache: CacheOutcome::Miss {
                reason: MissReason::KeyNotFound,
            },
            timing: NodeTiming {
                queued_ms: 0,
                execute_ms: duration.as_millis() as u64,
            },
            inputs,
            outputs: vec![OutputRecord {
                logical: "bin".to_string(),
                manifest: Some(manifest_hash.to_hex()),
                blob: Some(blob_hash.to_hex()),
                path: Some(format!(
                    ".vx/build/{}/{}/{}",
                    target_triple, profile, manifest.name
                )),
            }],
            invocation: Some(InvocationRecord {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                cwd: invocation.cwd.clone(),
                exit_code: 0,
            }),
            diagnostics: DiagnosticsRecord {
                stdout_blob: stdout_blob.map(|h| h.to_hex()),
                stderr_blob: stderr_blob.map(|h| h.to_hex()),
            },
        };
        build_report.add_node(node_report);
        build_report.finalize(true, None);
        self.save_report(project_path, &build_report);

        // Save picante cache after successful build
        if let Err(e) = self.save_cache().await {
            warn!(error = %e, "failed to save picante cache (non-fatal)");
        }

        Ok(BuildResult {
            success: true,
            message: format!(
                "{} {} in {:.2}s",
                manifest.name,
                profile,
                duration.as_secs_f64()
            ),
            cached: false,
            duration_ms: duration.as_millis() as u64,
            output_path: Some(output_path),
            error: None,
        })
    }

    /// Save a build report to the project's .vx/runs/ directory.
    fn save_report(&self, project_path: &Utf8PathBuf, report: &BuildReport) {
        let store = ReportStore::new(project_path);
        if let Err(e) = store.save(report) {
            warn!(error = %e, "failed to save build report (non-fatal)");
        } else {
            debug!(run_id = %report.run_id, "saved build report");
        }
    }
}

impl Daemon for DaemonService {
    async fn build(&self, request: BuildRequest) -> BuildResult {
        match self.do_build(request).await {
            Ok(result) => result,
            Err(e) => BuildResult {
                success: false,
                message: "internal error".to_string(),
                cached: false,
                duration_ms: 0,
                output_path: None,
                error: Some(e),
            },
        }
    }
}

// =============================================================================
// HELPERS
// =============================================================================

/// Format a module scanner error for user-friendly display.
fn format_module_error(e: &ModuleError) -> String {
    // The error type already has good Display impl with file:line info
    e.to_string()
}

fn get_rustc_version() -> eyre::Result<String> {
    let output = Command::new("rustc").arg("-vV").output()?;

    if !output.status.success() {
        eyre::bail!("rustc -vV failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Extract a human-readable version from `rustc -vV` output.
fn extract_rustc_version(rustc_version: &str) -> Option<String> {
    // Example output:
    // rustc 1.84.0 (9fc6b4312 2024-01-17)
    // binary: rustc
    // commit-hash: 9fc6b43126469e3858e2fe86f5c5d5f8f3e39649
    // ...
    for line in rustc_version.lines() {
        if line.starts_with("rustc ") {
            return Some(line.to_string());
        }
    }
    None
}

fn get_target_triple() -> eyre::Result<String> {
    let output = Command::new("rustc").arg("-vV").output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }

    eyre::bail!("could not determine host triple from rustc -vV")
}

fn atomic_write_file(dest: &Utf8PathBuf, data: &[u8], executable: bool) -> std::io::Result<()> {
    use std::io::Write;

    let tmp_path = dest.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);

    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&tmp_path, perms)?;
    }

    std::fs::rename(&tmp_path, dest)?;
    Ok(())
}

fn unix_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests;
