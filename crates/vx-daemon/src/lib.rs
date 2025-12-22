//! vx-daemon: Orchestration daemon with picante incremental queries
//!
//! This daemon:
//! - Owns the picante incremental computation runtime
//! - Parses Cargo.toml and sets up inputs
//! - Computes cache keys via tracked queries
//! - Orchestrates builds via CAS and Exec services

use std::process::Command;
use std::sync::Arc;

use std::collections::HashMap;

use camino::Utf8PathBuf;
use picante::persist::{CacheLoadOptions, OnCorruptCache, load_cache_with_options, save_cache};
use picante::{HasRuntime, PicanteResult};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, CacheKey, Cas, NodeId, NodeManifest, OutputEntry};
use vx_casd::CasService;
use vx_daemon_proto::{BuildRequest, BuildResult, Daemon};
use vx_exec_proto::{Exec, MaterializeStatus, RegistryMaterializeRequest};
use vx_exec_proto::{ExpectedOutput, RustcInvocation};
use vx_execd::ExecService;
use vx_manifest::Edition;
use vx_registry_proto::{CasRegistry, RegistrySpec};
use vx_report::{
    BuildReport, CacheOutcome, DiagnosticsRecord, InputRecord, InvocationRecord, MissReason,
    NodeReport, NodeTiming, OutputRecord, ReportStore, ToolchainRef, ToolchainsUsed,
};
use vx_rs::{CrateGraph, CrateGraphError, CrateId, CrateType, ModuleError};

/// Format a diagnostic error using miette's graphical handler with syntax highlighting.
/// This preserves source spans and produces nice terminal output with colors.
fn format_diagnostic(err: &dyn miette::Diagnostic) -> String {
    let mut buf = String::new();
    let handler = miette::GraphicalReportHandler::new_themed(miette::GraphicalTheme::unicode())
        .with_syntax_highlighting(miette_arborium::MietteHighlighter::new());
    // Ignore rendering errors and fall back to Display
    if handler.render_report(&mut buf, err).is_ok() {
        buf
    } else {
        format!("{}", err)
    }
}
use vx_toolchain_proto::{CasToolchain, RustChannel, RustComponent, RustToolchainSpec};

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

/// The rustc version info (singleton) - DEPRECATED, use RustToolchain instead
// =============================================================================
// HERMETIC TOOLCHAIN INPUTS (per "Toolchains are CAS artifacts" spec)
// =============================================================================

/// Rust toolchain identity (singleton picante input)
///
/// This represents a hermetically acquired Rust toolchain from static.rust-lang.org.
/// The toolchain_id is derived from publisher SHA256s + host/target triples.
///
/// NOTE: Paths are NOT stored here - they are machine-specific and derived
/// at runtime from materialization. Only identity and provenance are recorded.
#[picante::input]
pub struct RustToolchain {
    /// Content-derived toolchain identifier
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
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let closure = SourceClosure::get(db)?.expect("SourceClosure not set");

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Hermetic RustToolchain is required (no fallback to system rustc)
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");
    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
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

    // Hermetic toolchain manifest is required
    let toolchain_manifest = RustToolchainManifest::get(db)?
        .expect("RustToolchainManifest not set - hermetic toolchain required");

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

    // Use hermetic zig linker if available
    if let Some(zig) = ZigToolchain::get(db)? {
        args.push("-C".to_string());
        args.push(format!("linker={}", zig.zig_path));
        args.push("-C".to_string());
        args.push("link-arg=cc".to_string());
    }

    if config.profile == "release" {
        args.push("-C".to_string());
        args.push("opt-level=3".to_string());
    }

    // NOTE: --sysroot will be added by execd during materialization

    let expected_outputs = vec![ExpectedOutput {
        logical: "bin".to_string(),
        path: output_path,
        executable: true,
    }];

    Ok(RustcInvocation {
        toolchain_manifest: toolchain_manifest.manifest_hash,
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

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-rlib-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Hermetic RustToolchain is required (no fallback to system rustc)
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");
    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
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

    // Hermetic toolchain manifest is required
    let toolchain_manifest = RustToolchainManifest::get(db)?
        .expect("RustToolchainManifest not set - hermetic toolchain required");

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

    // NOTE: --sysroot will be added by execd during materialization

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
        toolchain_manifest: toolchain_manifest.manifest_hash,
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

/// Compute the cache key for compiling a library crate with dependencies.
///
/// CRITICAL: The cache key explicitly includes dep_outputs (sorted by extern_name).
/// This ensures that changes to dependencies invalidate the rlib cache key.
#[picante::tracked]
pub async fn cache_key_compile_rlib_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_rlib_hashes: Vec<(String, Blake3Hash)>, // (extern_name, rlib_hash) sorted
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_rlib_with_deps: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-rlib-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Hermetic RustToolchain is required (no fallback to system rustc)
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");
    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
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

    // Include dependency rlib hashes (sorted by extern_name for determinism)
    for (extern_name, rlib_hash) in &dep_rlib_hashes {
        hasher.update(b"dep:");
        hasher.update(extern_name.as_bytes());
        hasher.update(b":");
        hasher.update(&rlib_hash.0);
        hasher.update(b"\n");
    }

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Plan a rustc invocation for compiling a library crate with dependencies.
///
/// All paths are workspace-relative. rustc is invoked with cwd = workspace_root.
#[picante::tracked]
pub async fn plan_compile_rlib_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_extern_paths: Vec<(String, String)>, // (extern_name, workspace-rel rlib path) sorted
) -> PicanteResult<RustcInvocation> {
    debug!("plan_compile_rlib_with_deps: COMPUTING (not memoized)");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    // Hermetic toolchain manifest is required
    let toolchain_manifest = RustToolchainManifest::get(db)?
        .expect("RustToolchainManifest not set - hermetic toolchain required");

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

    // NOTE: --sysroot will be added by execd during materialization

    // Add --extern flags for dependencies
    for (extern_name, rlib_path) in &dep_extern_paths {
        args.push("--extern".to_string());
        args.push(format!("{}={}", extern_name, rlib_path));
    }

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
        toolchain_manifest: toolchain_manifest.manifest_hash,
        args,
        env: vec![],
        cwd: config.workspace_root.to_string(),
        expected_outputs,
    })
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

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-bin-deps-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Hermetic RustToolchain is required (no fallback to system rustc)
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");
    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
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

    // Hermetic toolchain manifest is required
    let toolchain_manifest = RustToolchainManifest::get(db)?
        .expect("RustToolchainManifest not set - hermetic toolchain required");

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

    // NOTE: --sysroot will be added by execd during materialization

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
        toolchain_manifest: toolchain_manifest.manifest_hash,
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
        BuildConfig,
        // Hermetic toolchain inputs (per "Toolchains are CAS artifacts" spec)
        RustToolchain,
        RustToolchainManifest,
        ZigToolchain,
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
        cache_key_compile_rlib_with_deps,
        plan_compile_rlib_with_deps,
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

/// Toolchain information (manifest-only, no materialization)
pub struct ToolchainInfo {
    /// Hash of the ToolchainManifest in CAS
    pub manifest_hash: Blake3Hash,
    /// Content-derived toolchain ID (from manifest)
    pub toolchain_id: Blake3Hash,
    /// Rustc version string (for reports)
    pub version: Option<String>,
    /// Manifest date (for reports)
    pub manifest_date: Option<String>,
}

/// Acquired toolchains (manifest references only)
pub struct AcquiredToolchains {
    /// Rust toolchain (if acquired)
    pub rust: Option<ToolchainInfo>,
    /// Zig toolchain (if acquired)
    pub zig: Option<ToolchainInfo>,
}

/// The daemon service implementation
pub struct DaemonService {
    /// CAS service for content-addressed storage (Arc for sharing with exec)
    cas: Arc<CasService>,
    /// Exec service for materialization and compilation
    exec: ExecService<Arc<CasService>>,
    /// The picante incremental database (shared across builds)
    db: Arc<Mutex<Database>>,
    /// Path to the picante cache file
    cache_path: Utf8PathBuf,
    /// VX_HOME directory
    vx_home: Utf8PathBuf,
    /// Acquired toolchains (manifest references only, no materialization)
    toolchains: Arc<Mutex<AcquiredToolchains>>,
}

impl DaemonService {
    /// Create a new daemon service
    pub fn new(vx_home: Utf8PathBuf) -> std::io::Result<Self> {
        let cas_root = vx_home.clone();
        let cas = CasService::new(cas_root);
        cas.init()?;
        let cas = Arc::new(cas);

        // Set up directories for exec service
        let toolchains_dir = vx_home.join("toolchains");
        let registry_cache_dir = vx_home.join("registry");
        std::fs::create_dir_all(&toolchains_dir)?;
        std::fs::create_dir_all(&registry_cache_dir)?;

        let exec = ExecService::new(Arc::clone(&cas), toolchains_dir, registry_cache_dir);

        let db = Database::new();
        let cache_path = vx_home.join("picante.cache");

        Ok(Self {
            cas,
            exec,
            db: Arc::new(Mutex::new(db)),
            cache_path,
            vx_home,
            toolchains: Arc::new(Mutex::new(AcquiredToolchains {
                rust: None,
                zig: None,
            })),
        })
    }

    /// Ensure Rust toolchain exists in CAS. Returns info for cache keys.
    /// Does NOT materialize - that's execd's job.
    pub async fn ensure_rust_toolchain(
        &self,
        channel: vx_toolchain::Channel,
    ) -> Result<ToolchainInfo, String> {
        // Check if already acquired
        {
            let toolchains = self.toolchains.lock().await;
            if let Some(ref rust) = toolchains.rust {
                return Ok(ToolchainInfo {
                    manifest_hash: rust.manifest_hash,
                    toolchain_id: rust.toolchain_id,
                    version: rust.version.clone(),
                    manifest_date: rust.manifest_date.clone(),
                });
            }
        }

        // Detect host triple
        let host = vx_toolchain::detect_host_triple()
            .map_err(|e| format!("failed to detect host: {}", e))?;

        // Build RustToolchainSpec for CAS
        let spec = RustToolchainSpec {
            channel: match channel {
                vx_toolchain::Channel::Stable => RustChannel::Stable,
                vx_toolchain::Channel::Beta => RustChannel::Beta,
                vx_toolchain::Channel::Nightly { date } => RustChannel::Nightly { date },
            },
            host: host.clone(),
            target: host.clone(),
            components: vec![RustComponent::Rustc, RustComponent::RustStd],
        };

        // Ensure toolchain exists in CAS (downloads if needed)
        let result = self.cas.ensure_rust_toolchain(spec).await;

        if result.status == vx_toolchain_proto::EnsureStatus::Failed {
            return Err(result.error.unwrap_or_else(|| "unknown error".into()));
        }

        let manifest_hash = result
            .manifest_hash
            .ok_or("ensure succeeded but no manifest_hash")?;

        // Get manifest to extract version info for reports
        let manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .ok_or("failed to get toolchain manifest")?;

        let toolchain_info = ToolchainInfo {
            manifest_hash,
            toolchain_id: manifest.toolchain_id,
            version: manifest.rust_version.clone(),
            manifest_date: manifest.rust_manifest_date.clone(),
        };

        info!(
            toolchain_id = %toolchain_info.toolchain_id.short_hex(),
            manifest_hash = %toolchain_info.manifest_hash.short_hex(),
            version = ?toolchain_info.version,
            "Rust toolchain ready in CAS"
        );

        // Store it
        {
            let mut toolchains = self.toolchains.lock().await;
            toolchains.rust = Some(ToolchainInfo {
                manifest_hash: toolchain_info.manifest_hash,
                toolchain_id: toolchain_info.toolchain_id,
                version: toolchain_info.version.clone(),
                manifest_date: toolchain_info.manifest_date.clone(),
            });
        }

        Ok(toolchain_info)
    }

    /// Ensure Zig toolchain exists in CAS (NOT YET IMPLEMENTED)
    /// TODO: Implement Phase 7 - Zig toolchain via CAS
    pub async fn ensure_zig_toolchain(&self, _version: &str) -> Result<ToolchainInfo, String> {
        Err("Zig toolchain acquisition via CAS not yet implemented (Phase 7)".to_string())
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

    /// Materialize all registry crates needed by the build.
    ///
    /// This method:
    /// 1. Ensures each registry crate exists in CAS (downloads if needed)
    /// 2. Materializes to workspace-local staging
    /// 3. Returns a map of (name, version) -> workspace-relative path
    async fn materialize_registry_crates(
        &self,
        graph: &CrateGraph,
    ) -> Result<HashMap<(String, String), Utf8PathBuf>, String> {
        let mut materialized_paths: HashMap<(String, String), Utf8PathBuf> = HashMap::new();

        for info in graph.iter_registry_crates() {
            // Build RegistrySpec for CAS lookup
            let spec = RegistrySpec::crates_io(&info.name, &info.version, &info.checksum);

            // Ensure the crate exists in CAS (downloads tarball if needed)
            let ensure_result = self.cas.ensure_registry_crate(spec).await;

            if ensure_result.status == vx_registry_proto::EnsureStatus::Failed {
                return Err(format!(
                    "failed to acquire registry crate {} {}: {}",
                    info.name,
                    info.version,
                    ensure_result
                        .error
                        .unwrap_or_else(|| "unknown error".into())
                ));
            }

            let manifest_hash = ensure_result.manifest_hash.ok_or_else(|| {
                format!(
                    "registry crate {} {} acquired but no manifest hash",
                    info.name, info.version
                )
            })?;

            // Materialize to workspace via execd
            let request = RegistryMaterializeRequest {
                manifest_hash,
                workspace_root: graph.workspace_root.to_string(),
            };
            let mat_result = self.exec.materialize_registry_crate(request).await;

            if mat_result.status == MaterializeStatus::Failed {
                return Err(format!(
                    "failed to materialize {} {}: {}",
                    info.name,
                    info.version,
                    mat_result.error.unwrap_or_else(|| "unknown error".into())
                ));
            }

            info!(
                name = %info.name,
                version = %info.version,
                path = %mat_result.workspace_rel_path,
                cached = mat_result.was_cached,
                "materialized registry crate"
            );

            materialized_paths.insert(
                (info.name.clone(), info.version.clone()),
                Utf8PathBuf::from(mat_result.workspace_rel_path),
            );
        }

        Ok(materialized_paths)
    }

    /// Build a Rust project (single or multi-crate with path dependencies).
    ///
    /// This method:
    /// 1. Acquires a hermetic Rust toolchain
    /// 2. Builds the CrateGraph from the invocation directory
    /// 3. Computes source closures for all crates
    /// 4. Builds lib crates in topological order
    /// 5. Builds the root bin crate with --extern flags for deps
    ///
    /// Single-crate projects are a degenerate case: a graph with one node and no edges.
    async fn do_build(&self, request: BuildRequest) -> Result<BuildResult, String> {
        let project_path = &request.project_path;

        // Reject ambient RUSTFLAGS - vx uses a clean environment for hermeticity.
        reject_ambient_rustflags()?;

        // Acquire hermetic Rust toolchain (stable channel for now)
        let rust_toolchain = self
            .ensure_rust_toolchain(vx_toolchain::Channel::Stable)
            .await?;

        // Target triple comes from the toolchain spec (host = target for native builds)
        let target_triple = vx_toolchain::detect_host_triple()
            .map_err(|e| format!("failed to detect host triple: {}", e))?;

        // Build the crate graph (this parses all Cargo.toml files and computes LCA)
        // Use build_with_lockfile to detect and handle registry dependencies
        let mut graph = CrateGraph::build_with_lockfile(project_path)
            .map_err(|e: CrateGraphError| format_diagnostic(&e))?;

        // If there are registry dependencies, materialize them and finalize the graph
        if graph.has_registry_deps() {
            info!(
                registry_crate_count = graph.registry_crates.len(),
                "materializing registry crates"
            );

            let materialized_paths = self.materialize_registry_crates(&graph).await?;

            graph
                .finalize_with_registry(&materialized_paths)
                .map_err(|e: CrateGraphError| format_diagnostic(&e))?;
        }

        info!(
            workspace_root = %graph.workspace_root,
            crate_count = graph.nodes.len(),
            registry_crate_count = graph.registry_crates.len(),
            toolchain_id = %rust_toolchain.toolchain_id.short_hex(),
            "resolved crate graph"
        );

        let profile = if request.release { "release" } else { "debug" };

        // Initialize build report
        let mut build_report = BuildReport::new(
            project_path.to_string(),
            profile.to_string(),
            target_triple.clone(),
        );

        build_report.toolchains = ToolchainsUsed {
            rust: Some(ToolchainRef {
                id: rust_toolchain.toolchain_id.to_hex(),
                version: rust_toolchain.version.clone(),
            }),
            zig: None,
        };

        // Set up shared picante inputs
        let db = self.db.lock().await;

        // Set hermetic toolchain identity (for cache keys)
        RustToolchain::set(
            &*db,
            rust_toolchain.toolchain_id,
            rust_toolchain.manifest_hash,
            target_triple.clone(),
            target_triple.clone(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Set hermetic toolchain manifest (for plan queries)
        RustToolchainManifest::set(&*db, rust_toolchain.manifest_hash)
            .map_err(|e| format!("picante error: {}", e))?;

        BuildConfig::set(
            &*db,
            profile.to_string(),
            target_triple.clone(),
            graph.workspace_root.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Track rlib outputs for bin dependencies
        // Maps CrateId -> (rlib_hash, rlib_path)
        let mut rlib_outputs: HashMap<CrateId, (Blake3Hash, String)> = HashMap::new();

        // Build output base directory (used by build_rlib and build_bin_with_deps)
        let _output_base = graph
            .workspace_root
            .join(".vx/build")
            .join(&target_triple)
            .join(profile);

        // Process crates in topological order (deps before dependents)
        let total_start = std::time::Instant::now();
        let mut any_rebuilt = false;
        let mut final_output_path: Option<Utf8PathBuf> = None;

        for crate_node in graph.iter_topo() {
            debug!(
                crate_name = %crate_node.crate_name,
                crate_type = ?crate_node.crate_type,
                "processing crate"
            );

            // Compute source closure for this crate
            let crate_root_abs = graph.workspace_root.join(&crate_node.crate_root_rel);
            let closure_paths = vx_rs::rust_source_closure(&crate_root_abs, &graph.workspace_root)
                .map_err(|e| {
                    let error_msg = format_module_error(&e);
                    build_report.finalize(false, Some(error_msg.clone()));
                    self.save_report(project_path, &build_report);
                    error_msg
                })?;

            let closure_hash = vx_rs::hash_source_closure(&closure_paths, &graph.workspace_root)
                .map_err(|e| {
                    format!(
                        "failed to hash source closure for {}: {}",
                        crate_node.crate_name, e
                    )
                })?;

            // Create RustCrate input (keyed by crate_id_hex)
            let crate_id_hex = crate_node.id.short_hex();
            let rust_crate = RustCrate::new(
                &*db,
                crate_id_hex.clone(),
                crate_node.crate_name.clone(),
                crate_node.edition.as_str().to_string(),
                crate_node.crate_type.as_str().to_string(),
                crate_node.crate_root_rel.to_string(),
                closure_hash.clone(),
            )
            .map_err(|e| format!("picante error: {}", e))?;

            match crate_node.crate_type {
                CrateType::Lib => {
                    // Collect dependency outputs for this lib crate (sorted by extern_name)
                    let mut deps: Vec<DepOutput> = Vec::new();
                    let mut dep_extern_paths: Vec<(String, String)> = Vec::new();

                    for dep in &crate_node.deps {
                        let (rlib_hash, rlib_path) =
                            rlib_outputs.get(&dep.crate_id).ok_or_else(|| {
                                format!(
                                    "rlib output not found for dependency {} of {}",
                                    dep.extern_name, crate_node.crate_name
                                )
                            })?;
                        deps.push(DepOutput {
                            extern_name: dep.extern_name.clone(),
                            crate_id: dep.crate_id,
                            rlib_hash: *rlib_hash,
                            manifest_hash: None, // Will be populated after build
                        });
                        dep_extern_paths.push((dep.extern_name.clone(), rlib_path.clone()));
                    }

                    // Sort for determinism
                    deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
                    dep_extern_paths.sort_by(|a, b| a.0.cmp(&b.0));

                    // Extract cfg flags from build script output
                    let cfg_flags = crate_node
                        .build_script_output
                        .as_ref()
                        .map(|o| o.cfgs.clone())
                        .unwrap_or_default();

                    // Build library crate
                    let (rlib_hash, rlib_path, node_report) = self
                        .build_rlib(
                            &*db,
                            rust_crate.clone(),
                            deps,
                            dep_extern_paths,
                            cfg_flags,
                            &graph.workspace_root,
                            &target_triple,
                            profile,
                        )
                        .await?;

                    if !matches!(node_report.cache, CacheOutcome::Hit { .. }) {
                        any_rebuilt = true;
                    }
                    build_report.add_node(node_report);

                    // Record output for dependents
                    rlib_outputs.insert(crate_node.id, (rlib_hash.clone(), rlib_path.clone()));

                    // Create RlibOutput input for this crate
                    let _rlib_output =
                        RlibOutput::new(&*db, crate_id_hex.clone(), rlib_hash, rlib_path)
                            .map_err(|e| format!("picante error: {}", e))?;
                }

                CrateType::Bin => {
                    // Collect dependency outputs (sorted by extern_name)
                    let mut deps: Vec<DepOutput> = Vec::new();
                    let mut dep_extern_paths: Vec<(String, String)> = Vec::new();

                    for dep in &crate_node.deps {
                        let (rlib_hash, rlib_path) =
                            rlib_outputs.get(&dep.crate_id).ok_or_else(|| {
                                format!(
                                    "rlib output not found for dependency {} of {}",
                                    dep.extern_name, crate_node.crate_name
                                )
                            })?;
                        deps.push(DepOutput {
                            extern_name: dep.extern_name.clone(),
                            crate_id: dep.crate_id,
                            rlib_hash: *rlib_hash,
                            manifest_hash: None, // Will be populated after build
                        });
                        dep_extern_paths.push((dep.extern_name.clone(), rlib_path.clone()));
                    }

                    // Sort for determinism (should already be sorted, but be safe)
                    deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
                    dep_extern_paths.sort_by(|a, b| a.0.cmp(&b.0));

                    // Extract cfg flags from build script output
                    let cfg_flags = crate_node
                        .build_script_output
                        .as_ref()
                        .map(|o| o.cfgs.clone())
                        .unwrap_or_default();

                    // Build binary crate
                    let (output_path, node_report) = self
                        .build_bin_with_deps(
                            &*db,
                            rust_crate.clone(),
                            deps,
                            dep_extern_paths,
                            cfg_flags,
                            &graph.workspace_root,
                            &target_triple,
                            profile,
                        )
                        .await?;

                    if !matches!(node_report.cache, CacheOutcome::Hit { .. }) {
                        any_rebuilt = true;
                    }
                    build_report.add_node(node_report);
                    final_output_path = Some(output_path);
                }
            }
        }

        // Release db lock
        drop(db);

        let total_duration = total_start.elapsed();

        // Save picante cache if anything was rebuilt
        if any_rebuilt {
            if let Err(e) = self.save_cache().await {
                warn!(error = %e, "failed to save picante cache (non-fatal)");
            }
        }

        // Finalize report
        let root_name = &graph.root().crate_name;
        build_report.finalize(true, None);
        self.save_report(project_path, &build_report);

        let cached = !any_rebuilt;
        let message = if cached {
            format!("{} {} (cached)", root_name, profile)
        } else {
            format!(
                "{} {} in {:.2}s",
                root_name,
                profile,
                total_duration.as_secs_f64()
            )
        };

        Ok(BuildResult {
            success: true,
            message,
            cached,
            duration_ms: total_duration.as_millis() as u64,
            output_path: final_output_path,
            error: None,
        })
    }

    /// Build a library crate to rlib.
    ///
    /// Returns (rlib_hash, rlib_path, node_report).
    async fn build_rlib(
        &self,
        db: &Database,
        rust_crate: RustCrate,
        deps: Vec<DepOutput>,
        dep_extern_paths: Vec<(String, String)>,
        cfg_flags: Vec<String>, // --cfg flags from build script
        workspace_root: &Utf8PathBuf,
        target_triple: &str,
        profile: &str,
    ) -> Result<(Blake3Hash, String, NodeReport), String> {
        let crate_name = rust_crate
            .crate_name(db)
            .map_err(|e| format!("picante error: {}", e))?;
        let crate_id = rust_crate
            .crate_id(db)
            .map_err(|e| format!("picante error: {}", e))?;
        let closure_hash = rust_crate
            .source_closure_hash(db)
            .map_err(|e| format!("picante error: {}", e))?;

        // Extract (extern_name, rlib_hash) tuples for cache key computation
        let dep_rlib_hashes: Vec<(String, Blake3Hash)> = deps
            .iter()
            .map(|d| (d.extern_name.clone(), d.rlib_hash))
            .collect();

        // Compute cache key (use _with_deps version if there are dependencies)
        let base_cache_key = if dep_rlib_hashes.is_empty() {
            cache_key_compile_rlib(db, rust_crate.clone())
                .await
                .map_err(|e| format!("failed to compute rlib cache key: {}", e))?
        } else {
            cache_key_compile_rlib_with_deps(db, rust_crate.clone(), dep_rlib_hashes.clone())
                .await
                .map_err(|e| format!("failed to compute rlib cache key: {}", e))?
        };

        // If there are cfg flags from build script, mix them into the cache key
        let cache_key = if cfg_flags.is_empty() {
            base_cache_key
        } else {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&base_cache_key.0);
            hasher.update(b"\ncfg_flags:");
            for flag in &cfg_flags {
                hasher.update(flag.as_bytes());
                hasher.update(b"\n");
            }
            Blake3Hash(*hasher.finalize().as_bytes())
        };

        let node_id = node_id_compile_rlib(db, rust_crate.clone())
            .await
            .map_err(|e| format!("failed to get rlib node id: {}", e))?;

        // Output path
        let output_dir = format!(
            ".vx/build/{}/{}/deps/{}",
            target_triple, profile, &*crate_id
        );
        let rlib_filename = format!("lib{}.rlib", &*crate_name);
        let rlib_path = format!("{}/{}", output_dir, rlib_filename);

        // Create output directory
        let output_dir_abs = workspace_root.join(&output_dir);
        std::fs::create_dir_all(&output_dir_abs)
            .map_err(|e| format!("failed to create output dir: {}", e))?;

        let mut inputs = vec![
            InputRecord {
                label: "source_closure".to_string(),
                value: closure_hash.to_hex(),
            },
            InputRecord {
                label: "crate_name".to_string(),
                value: crate_name.to_string(),
            },
        ];

        // Add dep hashes to inputs (for backward compat with RunDiff)
        for (extern_name, rlib_hash) in &dep_rlib_hashes {
            inputs.push(InputRecord {
                label: format!("dep:{}", extern_name),
                value: rlib_hash.to_hex(),
            });
        }

        // Create DependencyRecords for the report
        let dep_records: Vec<vx_report::DependencyRecord> = deps
            .iter()
            .map(|d| vx_report::DependencyRecord {
                extern_name: d.extern_name.clone(),
                crate_id: d.crate_id.0.to_hex(),
                rlib_hash: d.rlib_hash.to_hex(),
                manifest_hash: d
                    .manifest_hash
                    .as_ref()
                    .map(|h| h.to_hex())
                    .unwrap_or_else(|| "pending".to_string()),
            })
            .collect();

        // Check cache
        if let Some(cached_manifest_hash) = self.cas.lookup(cache_key).await {
            if let Some(cached_manifest) = self.cas.get_manifest(cached_manifest_hash.clone()).await
            {
                // Cache hit - materialize rlib
                for output in &cached_manifest.outputs {
                    if output.logical == "rlib" {
                        if let Some(blob_data) = self.cas.get_blob(output.blob).await {
                            let dest_path = workspace_root.join(&rlib_path);
                            atomic_write_file(&dest_path, &blob_data, false)
                                .map_err(|e| format!("failed to write {}: {}", dest_path, e))?;

                            let node_report = NodeReport {
                                node_id: node_id.0.clone(),
                                kind: "rust.compile_rlib".to_string(),
                                cache_key: cache_key.to_hex(),
                                cache: CacheOutcome::Hit {
                                    manifest: cached_manifest_hash.to_hex(),
                                },
                                timing: NodeTiming {
                                    queued_ms: 0,
                                    execute_ms: 0,
                                },
                                inputs,
                                deps: dep_records.clone(),
                                outputs: vec![OutputRecord {
                                    logical: "rlib".to_string(),
                                    manifest: Some(cached_manifest_hash.to_hex()),
                                    blob: Some(output.blob.to_hex()),
                                    path: Some(rlib_path.clone()),
                                }],
                                invocation: None,
                                diagnostics: DiagnosticsRecord::default(),
                            };

                            return Ok((output.blob, rlib_path, node_report));
                        }
                    }
                }
            }
        }

        // Cache miss - build (use _with_deps version if there are dependencies)
        let mut invocation = if dep_extern_paths.is_empty() {
            plan_compile_rlib(db, rust_crate)
                .await
                .map_err(|e| format!("failed to plan rlib build: {}", e))?
        } else {
            plan_compile_rlib_with_deps(db, rust_crate, dep_extern_paths)
                .await
                .map_err(|e| format!("failed to plan rlib build: {}", e))?
        };

        // Add cfg flags from build script
        for cfg in &cfg_flags {
            invocation.args.push("--cfg".to_string());
            invocation.args.push(cfg.clone());
        }

        let start = std::time::Instant::now();
        let output = Command::new("rustc")
            .args(&invocation.args)
            .current_dir(&invocation.cwd)
            .output()
            .map_err(|e| format!("failed to execute rustc: {}", e))?;
        let duration = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("rustc failed for {}: {}", crate_name, stderr));
        }

        // Store output in CAS
        let rlib_abs = workspace_root.join(&rlib_path);
        let rlib_data = std::fs::read(&rlib_abs)
            .map_err(|e| format!("failed to read rlib {}: {}", rlib_abs, e))?;
        let blob_hash = self.cas.put_blob(rlib_data).await;

        // Create manifest
        let node_manifest = NodeManifest {
            node_id: node_id.clone(),
            cache_key,
            produced_at: unix_timestamp(),
            outputs: vec![OutputEntry {
                logical: "rlib".to_string(),
                filename: rlib_filename,
                blob: blob_hash,
                executable: false,
            }],
        };

        let manifest_hash = self.cas.put_manifest(node_manifest).await;
        self.cas.publish(cache_key, manifest_hash.clone()).await;

        let node_report = NodeReport {
            node_id: node_id.0.clone(),
            kind: "rust.compile_rlib".to_string(),
            cache_key: cache_key.to_hex(),
            cache: CacheOutcome::Miss {
                reason: MissReason::KeyNotFound,
            },
            timing: NodeTiming {
                queued_ms: 0,
                execute_ms: duration.as_millis() as u64,
            },
            inputs,
            deps: dep_records,
            outputs: vec![OutputRecord {
                logical: "rlib".to_string(),
                manifest: Some(manifest_hash.to_hex()),
                blob: Some(blob_hash.to_hex()),
                path: Some(rlib_path.clone()),
            }],
            invocation: Some(InvocationRecord {
                program: "rustc".to_string(),
                args: invocation.args,
                cwd: invocation.cwd,
                exit_code: 0,
            }),
            diagnostics: DiagnosticsRecord::default(),
        };

        Ok((blob_hash, rlib_path, node_report))
    }

    /// Build a binary crate with dependencies.
    ///
    /// Returns (output_path, node_report).
    async fn build_bin_with_deps(
        &self,
        db: &Database,
        rust_crate: RustCrate,
        deps: Vec<DepOutput>,
        dep_extern_paths: Vec<(String, String)>,
        cfg_flags: Vec<String>, // --cfg flags from build script
        workspace_root: &Utf8PathBuf,
        target_triple: &str,
        profile: &str,
    ) -> Result<(Utf8PathBuf, NodeReport), String> {
        let crate_name = rust_crate
            .crate_name(db)
            .map_err(|e| format!("picante error: {}", e))?;
        let closure_hash = rust_crate
            .source_closure_hash(db)
            .map_err(|e| format!("picante error: {}", e))?;

        // Extract (extern_name, rlib_hash) tuples for cache key computation
        let dep_rlib_hashes: Vec<(String, Blake3Hash)> = deps
            .iter()
            .map(|d| (d.extern_name.clone(), d.rlib_hash))
            .collect();

        // Compute cache key (includes dep hashes!)
        let base_cache_key =
            cache_key_compile_bin_with_deps(db, rust_crate.clone(), dep_rlib_hashes.clone())
                .await
                .map_err(|e| format!("failed to compute bin cache key: {}", e))?;

        // If there are cfg flags from build script, mix them into the cache key
        let cache_key = if cfg_flags.is_empty() {
            base_cache_key
        } else {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&base_cache_key.0);
            hasher.update(b"\ncfg_flags:");
            for flag in &cfg_flags {
                hasher.update(flag.as_bytes());
                hasher.update(b"\n");
            }
            Blake3Hash(*hasher.finalize().as_bytes())
        };

        let node_id = node_id_compile_bin_with_deps(db, rust_crate.clone())
            .await
            .map_err(|e| format!("failed to get bin node id: {}", e))?;

        // Output path
        let output_dir = format!(".vx/build/{}/{}", target_triple, profile);
        let output_path = workspace_root.join(&output_dir).join(&*crate_name);

        // Create output directory
        std::fs::create_dir_all(workspace_root.join(&output_dir))
            .map_err(|e| format!("failed to create output dir: {}", e))?;

        let mut inputs = vec![
            InputRecord {
                label: "source_closure".to_string(),
                value: closure_hash.to_hex(),
            },
            InputRecord {
                label: "crate_name".to_string(),
                value: crate_name.to_string(),
            },
        ];

        // Add dep hashes to inputs (for backward compat with RunDiff)
        for (extern_name, rlib_hash) in &dep_rlib_hashes {
            inputs.push(InputRecord {
                label: format!("dep:{}", extern_name),
                value: rlib_hash.to_hex(),
            });
        }

        // Create DependencyRecords for the report
        let dep_records: Vec<vx_report::DependencyRecord> = deps
            .iter()
            .map(|d| vx_report::DependencyRecord {
                extern_name: d.extern_name.clone(),
                crate_id: d.crate_id.0.to_hex(),
                rlib_hash: d.rlib_hash.to_hex(),
                manifest_hash: d
                    .manifest_hash
                    .as_ref()
                    .map(|h| h.to_hex())
                    .unwrap_or_else(|| "pending".to_string()),
            })
            .collect();

        // Check cache
        if let Some(cached_manifest_hash) = self.cas.lookup(cache_key).await {
            if let Some(cached_manifest) = self.cas.get_manifest(cached_manifest_hash.clone()).await
            {
                // Cache hit - materialize binary
                for output in &cached_manifest.outputs {
                    if output.logical == "bin" {
                        if let Some(blob_data) = self.cas.get_blob(output.blob).await {
                            atomic_write_file(&output_path, &blob_data, true)
                                .map_err(|e| format!("failed to write {}: {}", output_path, e))?;

                            let node_report = NodeReport {
                                node_id: node_id.0.clone(),
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
                                deps: dep_records.clone(),
                                outputs: vec![OutputRecord {
                                    logical: "bin".to_string(),
                                    manifest: Some(cached_manifest_hash.to_hex()),
                                    blob: Some(output.blob.to_hex()),
                                    path: Some(output_path.to_string()),
                                }],
                                invocation: None,
                                diagnostics: DiagnosticsRecord::default(),
                            };

                            return Ok((output_path, node_report));
                        }
                    }
                }
            }
        }

        // Cache miss - build
        let mut invocation = plan_compile_bin_with_deps(db, rust_crate, dep_extern_paths)
            .await
            .map_err(|e| format!("failed to plan bin build: {}", e))?;

        // Add cfg flags from build script
        for cfg in &cfg_flags {
            invocation.args.push("--cfg".to_string());
            invocation.args.push(cfg.clone());
        }

        let start = std::time::Instant::now();
        let output = Command::new("rustc")
            .args(&invocation.args)
            .current_dir(&invocation.cwd)
            .output()
            .map_err(|e| format!("failed to execute rustc: {}", e))?;
        let duration = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("rustc failed for {}: {}", crate_name, stderr));
        }

        // Store output in CAS
        let bin_data = std::fs::read(&output_path)
            .map_err(|e| format!("failed to read binary {}: {}", output_path, e))?;
        let blob_hash = self.cas.put_blob(bin_data).await;

        // Create manifest
        let node_manifest = NodeManifest {
            node_id: node_id.clone(),
            cache_key,
            produced_at: unix_timestamp(),
            outputs: vec![OutputEntry {
                logical: "bin".to_string(),
                filename: crate_name.to_string(),
                blob: blob_hash,
                executable: true,
            }],
        };

        let manifest_hash = self.cas.put_manifest(node_manifest).await;
        self.cas.publish(cache_key, manifest_hash.clone()).await;

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
            deps: dep_records,
            outputs: vec![OutputRecord {
                logical: "bin".to_string(),
                manifest: Some(manifest_hash.to_hex()),
                blob: Some(blob_hash.to_hex()),
                path: Some(output_path.to_string()),
            }],
            invocation: Some(InvocationRecord {
                program: "rustc".to_string(),
                args: invocation.args,
                cwd: invocation.cwd,
                exit_code: 0,
            }),
            diagnostics: DiagnosticsRecord::default(),
        };

        Ok((output_path, node_report))
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

/// Reject ambient RUSTFLAGS environment variables.
///
/// vx runs rustc in a clean environment for hermeticity. If the user has set
/// RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS, we reject loudly rather than silently
/// ignoring them. This ensures users understand that vx does not inherit these
/// environment variables.
///
/// In the future, vx may support explicit rustflags configuration through
/// manifest or config files.
fn reject_ambient_rustflags() -> Result<(), String> {
    let rustflags = std::env::var("RUSTFLAGS").ok();
    let cargo_rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS").ok();

    match (rustflags, cargo_rustflags) {
        (Some(rf), Some(crf)) => Err(format!(
            "vx does not support ambient RUSTFLAGS for hermeticity.\n\
             Found RUSTFLAGS={:?} and CARGO_ENCODED_RUSTFLAGS={:?}\n\
             Please unset these environment variables.\n\
             (Future: configure flags via vx manifest or config)",
            rf, crf
        )),
        (Some(rf), None) => Err(format!(
            "vx does not support ambient RUSTFLAGS for hermeticity.\n\
             Found RUSTFLAGS={:?}\n\
             Please unset this environment variable.\n\
             (Future: configure flags via vx manifest or config)",
            rf
        )),
        (None, Some(crf)) => Err(format!(
            "vx does not support ambient CARGO_ENCODED_RUSTFLAGS for hermeticity.\n\
             Found CARGO_ENCODED_RUSTFLAGS={:?}\n\
             Please unset this environment variable.\n\
             (Future: configure flags via vx manifest or config)",
            crf
        )),
        (None, None) => Ok(()),
    }
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
