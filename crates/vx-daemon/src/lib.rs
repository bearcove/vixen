//! vx-daemon: Orchestration daemon with picante incremental queries
//!
//! This daemon:
//! - Owns the picante incremental computation runtime
//! - Parses Cargo.toml and sets up inputs
//! - Computes cache keys via tracked queries
//! - Orchestrates builds via CAS and Exec services

use picante::PicanteResult;
use vx_cas_proto::{Blake3Hash, CacheKey, NodeId};
use vx_exec_proto::{ExpectedOutput, RustcInvocation};
use vx_manifest::Edition;

// =============================================================================
// INPUTS
// =============================================================================

/// A source file with its content hash (keyed by path)
#[picante::input]
pub struct SourceFile {
    /// Path relative to project root (e.g., "src/main.rs")
    #[key]
    pub path: String,
    /// Blake3 hash of the file content
    pub content_hash: Blake3Hash,
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
// TRACKED QUERIES
// =============================================================================

/// Compute the cache key for compiling a binary crate.
///
/// The cache key is a blake3 hash of all inputs that can affect the output:
/// - rustc version (full -vV output)
/// - target triple
/// - profile (debug/release)
/// - crate name
/// - edition
/// - source file content hash
/// - Cargo.toml content hash
///
/// The `source` parameter is the cache key for memoization.
/// Singletons (CargoToml, RustcVersion, BuildConfig) are read from db.
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(db: &DB, source: SourceFile) -> PicanteResult<CacheKey> {
    // Read singletons
    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let rustc = RustcVersion::get(db)?.expect("RustcVersion not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    // Read keyed input
    let source_hash = source.content_hash(db)?;

    // Build deterministic cache key
    let mut hasher = blake3::Hasher::new();

    // Schema version - bump this when cache key canonicalization changes
    hasher.update(b"vx-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Toolchain identity
    hasher.update(b"rustc:");
    hasher.update(rustc.version_string.as_bytes());
    hasher.update(b"\n");

    // Build configuration
    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    // Crate identity
    hasher.update(b"name:");
    hasher.update(cargo.name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(cargo.edition.as_str().as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    // Source inputs (content hashes)
    hasher.update(b"source:");
    hasher.update(&source_hash.0);
    hasher.update(b"\n");

    hasher.update(b"manifest:");
    hasher.update(&cargo.content_hash.0);
    hasher.update(b"\n");

    let hash = hasher.finalize();
    Ok(Blake3Hash(*hash.as_bytes()))
}

/// Build a rustc invocation for compiling a binary crate.
/// No parameters - reads all config from singletons.
#[picante::tracked]
pub async fn plan_compile_bin<DB: Db>(db: &DB) -> PicanteResult<RustcInvocation> {
    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    // Build output path: .vx/build/<target>/<profile>/<name>
    let output_dir = format!(".vx/build/{}/{}", config.target_triple, config.profile);
    let output_path = format!("{}/{}", output_dir, cargo.name);

    let mut args = vec![
        "--crate-name".to_string(),
        cargo.name.clone(),
        "--crate-type".to_string(),
        "bin".to_string(),
        "--edition".to_string(),
        cargo.edition.as_str().to_string(),
        // Remap paths for reproducibility
        format!(
            "--remap-path-prefix={}=/vx-workspace",
            config.workspace_root
        ),
        cargo.bin_path.clone(),
        "-o".to_string(),
        output_path.clone(),
    ];

    // Add optimization flags based on profile
    if config.profile == "release" {
        args.push("-C".to_string());
        args.push("opt-level=3".to_string());
    }

    // Expected output
    let expected_outputs = vec![ExpectedOutput {
        logical: "bin".to_string(),
        path: output_path,
        executable: true,
    }];

    Ok(RustcInvocation {
        program: "rustc".to_string(),
        args,
        env: vec![], // Clean environment
        cwd: config.workspace_root.clone(),
        expected_outputs,
    })
}

/// Generate a human-readable node ID for a compile-bin node.
#[picante::tracked]
pub async fn node_id_compile_bin<DB: Db>(db: &DB) -> PicanteResult<NodeId> {
    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    Ok(NodeId(format!(
        "compile-bin:{}:{}:{}",
        cargo.name, config.target_triple, config.profile
    )))
}

// =============================================================================
// DATABASE
// =============================================================================

#[picante::db(
    inputs(SourceFile, CargoToml, RustcVersion, BuildConfig),
    tracked(cache_key_compile_bin, plan_compile_bin, node_id_compile_bin,),
    db_trait(Db)
)]
pub struct Database {}
