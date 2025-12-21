//! vx-daemon: Orchestration daemon with picante incremental queries
//!
//! This daemon:
//! - Owns the picante incremental computation runtime
//! - Parses Cargo.toml and sets up inputs
//! - Computes cache keys via tracked queries
//! - Orchestrates builds via CAS and Exec services

use std::process::Command;
use std::sync::Arc;

use camino::Utf8PathBuf;
use picante::persist::{CacheLoadOptions, OnCorruptCache, load_cache_with_options, save_cache};
use picante::{HasRuntime, PicanteResult};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, CacheKey, Cas, NodeId, NodeManifest, OutputEntry};
use vx_casd::CasService;
use vx_daemon_proto::{BuildRequest, BuildResult, Daemon};
use vx_exec_proto::{ExpectedOutput, RustcInvocation};
use vx_manifest::{Edition, Manifest};

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
pub async fn cache_key_compile_bin<DB: Db>(db: &DB, source: SourceFile) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_bin: COMPUTING (not memoized)");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let rustc = RustcVersion::get(db)?.expect("RustcVersion not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let source_hash = source.content_hash(db)?;

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

    hasher.update(b"source:");
    hasher.update(&source_hash.0);
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
        SourceFile,
        CargoToml,
        RustcVersion,
        BuildConfig,
        CSourceFile,
        DiscoveredDeps,
        ZigToolchainConfig,
        CTarget,
    ),
    tracked(
        cache_key_compile_bin,
        plan_compile_bin,
        node_id_compile_bin,
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

        // Hash source file
        let main_rs_path = project_path.join(&manifest.bin.path);
        if !main_rs_path.exists() {
            return Ok(BuildResult {
                success: false,
                message: format!("source file not found: {}", main_rs_path),
                cached: false,
                duration_ms: 0,
                output_path: None,
                error: Some(format!("source file not found: {}", main_rs_path)),
            });
        }
        let main_rs_content = std::fs::read(&main_rs_path)
            .map_err(|e| format!("failed to read {}: {}", main_rs_path, e))?;
        let source_hash = Blake3Hash::from_bytes(&main_rs_content);

        // Hash Cargo.toml
        let cargo_toml_content = std::fs::read(&cargo_toml_path)
            .map_err(|e| format!("failed to read Cargo.toml: {}", e))?;
        let cargo_toml_hash = Blake3Hash::from_bytes(&cargo_toml_content);

        let profile = if request.release { "release" } else { "debug" };

        // Use the shared picante database
        let db = self.db.lock().await;

        // Set inputs
        // Keyed input: SourceFile::new(db, key, data_fields...)
        let source_id = SourceFile::new(&*db, manifest.bin.path.to_string(), source_hash)
            .map_err(|e| format!("picante error: {}", e))?;

        // Singletons: Type::set(db, field1, field2, ...)
        CargoToml::set(
            &*db,
            cargo_toml_hash,
            manifest.name.clone(),
            manifest.edition,
            manifest.bin.path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        RustcVersion::set(&*db, rustc_version).map_err(|e| format!("picante error: {}", e))?;

        BuildConfig::set(
            &*db,
            profile.to_string(),
            target_triple.clone(),
            project_path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Compute cache key via picante
        let cache_key = cache_key_compile_bin(&*db, source_id)
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

        // Check cache
        if let Some(cached_manifest_hash) = self.cas.lookup(cache_key.clone()).await {
            if let Some(cached_manifest) = self.cas.get_manifest(cached_manifest_hash).await {
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

                return Ok(BuildResult {
                    success: true,
                    message: format!("{} {} (cached)", manifest.name, profile),
                    cached: true,
                    duration_ms: 0,
                    output_path: Some(output_path),
                    error: None,
                });
            }
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

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Ok(BuildResult {
                success: false,
                message: format!("rustc failed"),
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
            node_id,
            cache_key: cache_key.clone(),
            produced_at: unix_timestamp(),
            outputs: vec![OutputEntry {
                logical: "bin".to_string(),
                filename: manifest.name.clone(),
                blob: blob_hash,
                executable: true,
            }],
        };

        let manifest_hash = self.cas.put_manifest(node_manifest).await;

        // Publish to cache
        let publish_result = self.cas.publish(cache_key, manifest_hash).await;
        if !publish_result.success {
            return Err(format!(
                "failed to publish cache entry: {:?}",
                publish_result.error
            ));
        }

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

fn get_rustc_version() -> eyre::Result<String> {
    let output = Command::new("rustc").arg("-vV").output()?;

    if !output.status.success() {
        eyre::bail!("rustc -vV failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn get_target_triple() -> eyre::Result<String> {
    let output = Command::new("rustc").arg("-vV").output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.starts_with("host: ") {
            return Ok(line[6..].to_string());
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
mod tests {
    use super::*;

    /// Create a fresh database for testing
    fn test_db() -> Database {
        Database::new()
    }

    /// Set up common build configuration for C builds
    fn setup_cc_config(db: &Database) {
        BuildConfig::set(
            db,
            "debug".to_string(),
            "x86_64-linux-musl".to_string(),
            "/test/workspace".to_string(),
        )
        .unwrap();

        ZigToolchainConfig::set(db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();
    }

    #[tokio::test]
    async fn test_cache_key_cc_compile_deterministic() {
        let db = test_db();
        setup_cc_config(&db);

        let source_hash = Blake3Hash::from_bytes(b"int main() { return 0; }");

        // Create source file input
        let source = CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();

        // Compute cache key twice
        let key1 = cache_key_cc_compile(&db, source.clone()).await.unwrap();
        let key2 = cache_key_cc_compile(&db, source).await.unwrap();

        // Should be identical (deterministic)
        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn test_cache_key_cc_compile_changes_with_source() {
        let db = test_db();
        setup_cc_config(&db);

        // First source
        let source1 = CSourceFile::new(
            &db,
            "src/main.c".to_string(),
            Blake3Hash::from_bytes(b"int main() { return 0; }"),
        )
        .unwrap();

        let key1 = cache_key_cc_compile(&db, source1).await.unwrap();

        // Different source content
        let source2 = CSourceFile::new(
            &db,
            "src/main.c".to_string(),
            Blake3Hash::from_bytes(b"int main() { return 1; }"),
        )
        .unwrap();

        let key2 = cache_key_cc_compile(&db, source2).await.unwrap();

        // Keys should be different
        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn test_cache_key_cc_compile_changes_with_profile() {
        let db = test_db();

        ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

        let source_hash = Blake3Hash::from_bytes(b"int main() { return 0; }");

        // Debug profile
        BuildConfig::set(
            &db,
            "debug".to_string(),
            "x86_64-linux-musl".to_string(),
            "/test/workspace".to_string(),
        )
        .unwrap();

        let source_debug =
            CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();
        let key_debug = cache_key_cc_compile(&db, source_debug).await.unwrap();

        // Release profile
        BuildConfig::set(
            &db,
            "release".to_string(),
            "x86_64-linux-musl".to_string(),
            "/test/workspace".to_string(),
        )
        .unwrap();

        let source_release = CSourceFile::new(&db, "src/main.c".to_string(), source_hash).unwrap();
        let key_release = cache_key_cc_compile(&db, source_release).await.unwrap();

        // Keys should be different
        assert_ne!(key_debug, key_release);
    }

    #[tokio::test]
    async fn test_plan_cc_compile_generates_correct_args() {
        let db = test_db();
        setup_cc_config(&db);

        let source = CSourceFile::new(
            &db,
            "src/main.c".to_string(),
            Blake3Hash::from_bytes(b"int main() { return 0; }"),
        )
        .unwrap();

        let invocation = plan_cc_compile(&db, source).await.unwrap();

        // Should have "cc" as first arg
        assert_eq!(invocation.args[0], "cc");

        // Should have -target
        assert!(invocation.args.contains(&"-target".to_string()));
        assert!(invocation.args.contains(&"x86_64-linux-musl".to_string()));

        // Should have -c for compile-only
        assert!(invocation.args.contains(&"-c".to_string()));

        // Should have source file
        assert!(invocation.args.contains(&"src/main.c".to_string()));

        // Should have -o with .o output
        assert!(invocation.args.contains(&"-o".to_string()));
        let o_idx = invocation.args.iter().position(|a| a == "-o").unwrap();
        assert!(invocation.args[o_idx + 1].ends_with(".o"));

        // Should have -MMD -MF for depfile generation
        assert!(invocation.args.contains(&"-MMD".to_string()));
        assert!(invocation.args.contains(&"-MF".to_string()));

        // Debug profile should have -O0 and -g
        assert!(invocation.args.contains(&"-O0".to_string()));
        assert!(invocation.args.contains(&"-g".to_string()));

        // Should expect object and depfile outputs
        assert_eq!(invocation.expected_outputs.len(), 2);
        assert!(
            invocation
                .expected_outputs
                .iter()
                .any(|o| o.logical == "obj")
        );
        assert!(
            invocation
                .expected_outputs
                .iter()
                .any(|o| o.logical == "depfile")
        );
    }

    #[tokio::test]
    async fn test_plan_cc_compile_release_flags() {
        let db = test_db();

        ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

        // Release profile
        BuildConfig::set(
            &db,
            "release".to_string(),
            "x86_64-linux-musl".to_string(),
            "/test/workspace".to_string(),
        )
        .unwrap();

        let source = CSourceFile::new(
            &db,
            "src/main.c".to_string(),
            Blake3Hash::from_bytes(b"int main() { return 0; }"),
        )
        .unwrap();

        let invocation = plan_cc_compile(&db, source).await.unwrap();

        // Release should have -O2, not -O0
        assert!(invocation.args.contains(&"-O2".to_string()));
        assert!(!invocation.args.contains(&"-O0".to_string()));

        // Release should NOT have -g
        assert!(!invocation.args.contains(&"-g".to_string()));
    }

    #[tokio::test]
    async fn test_node_id_cc_compile_format() {
        let db = test_db();
        setup_cc_config(&db);

        let source = CSourceFile::new(
            &db,
            "src/main.c".to_string(),
            Blake3Hash::from_bytes(b"int main() { return 0; }"),
        )
        .unwrap();

        let node_id = node_id_cc_compile(&db, source).await.unwrap();

        // Should be formatted as "cc-compile:path:triple:profile"
        assert!(node_id.0.starts_with("cc-compile:"));
        assert!(node_id.0.contains("src/main.c"));
        assert!(node_id.0.contains("x86_64-linux-musl"));
        assert!(node_id.0.contains("debug"));
    }

    #[tokio::test]
    async fn test_cache_key_cc_link_deterministic() {
        let db = test_db();
        setup_cc_config(&db);

        let obj_hashes = vec![
            Blake3Hash::from_bytes(b"main.o contents"),
            Blake3Hash::from_bytes(b"util.o contents"),
        ];

        let target = CTarget::new(
            &db,
            "hello".to_string(),
            obj_hashes.clone(),
            vec!["src/main.c".to_string(), "src/util.c".to_string()],
        )
        .unwrap();

        let key1 = cache_key_cc_link(&db, target.clone()).await.unwrap();
        let key2 = cache_key_cc_link(&db, target).await.unwrap();

        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn test_cache_key_cc_link_changes_with_objects() {
        let db = test_db();
        setup_cc_config(&db);

        // First set of objects
        let target1 = CTarget::new(
            &db,
            "hello".to_string(),
            vec![Blake3Hash::from_bytes(b"main.o v1")],
            vec!["src/main.c".to_string()],
        )
        .unwrap();

        let key1 = cache_key_cc_link(&db, target1).await.unwrap();

        // Different object contents
        let target2 = CTarget::new(
            &db,
            "hello".to_string(),
            vec![Blake3Hash::from_bytes(b"main.o v2")],
            vec!["src/main.c".to_string()],
        )
        .unwrap();

        let key2 = cache_key_cc_link(&db, target2).await.unwrap();

        assert_ne!(key1, key2);
    }

    #[tokio::test]
    async fn test_plan_cc_link_generates_correct_args() {
        let db = test_db();
        setup_cc_config(&db);

        let target = CTarget::new(
            &db,
            "hello".to_string(),
            vec![
                Blake3Hash::from_bytes(b"main.o"),
                Blake3Hash::from_bytes(b"util.o"),
            ],
            vec!["src/main.c".to_string(), "src/util.c".to_string()],
        )
        .unwrap();

        let invocation = plan_cc_link(&db, target).await.unwrap();

        // Should have "cc" as first arg
        assert_eq!(invocation.args[0], "cc");

        // Should have -target
        assert!(invocation.args.contains(&"-target".to_string()));
        assert!(invocation.args.contains(&"x86_64-linux-musl".to_string()));

        // Should have object files
        assert!(invocation.args.iter().any(|a| a.ends_with("main.o")));
        assert!(invocation.args.iter().any(|a| a.ends_with("util.o")));

        // Should have -o with exe output
        assert!(invocation.args.contains(&"-o".to_string()));
        let o_idx = invocation.args.iter().position(|a| a == "-o").unwrap();
        assert!(invocation.args[o_idx + 1].ends_with("hello"));

        // Debug profile should NOT have -s (strip)
        assert!(!invocation.args.contains(&"-s".to_string()));

        // Should expect executable output
        assert_eq!(invocation.expected_outputs.len(), 1);
        assert_eq!(invocation.expected_outputs[0].logical, "exe");
        assert!(invocation.expected_outputs[0].executable);
    }

    #[tokio::test]
    async fn test_plan_cc_link_release_strips() {
        let db = test_db();

        ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

        // Release profile
        BuildConfig::set(
            &db,
            "release".to_string(),
            "x86_64-linux-musl".to_string(),
            "/test/workspace".to_string(),
        )
        .unwrap();

        let target = CTarget::new(
            &db,
            "hello".to_string(),
            vec![Blake3Hash::from_bytes(b"main.o")],
            vec!["src/main.c".to_string()],
        )
        .unwrap();

        let invocation = plan_cc_link(&db, target).await.unwrap();

        // Release should have -s for stripping
        assert!(invocation.args.contains(&"-s".to_string()));
    }

    #[tokio::test]
    async fn test_node_id_cc_link_format() {
        let db = test_db();
        setup_cc_config(&db);

        let target = CTarget::new(
            &db,
            "hello".to_string(),
            vec![Blake3Hash::from_bytes(b"main.o")],
            vec!["src/main.c".to_string()],
        )
        .unwrap();

        let node_id = node_id_cc_link(&db, target).await.unwrap();

        // Should be formatted as "cc-link:name:triple:profile"
        assert!(node_id.0.starts_with("cc-link:"));
        assert!(node_id.0.contains("hello"));
        assert!(node_id.0.contains("x86_64-linux-musl"));
        assert!(node_id.0.contains("debug"));
    }

    #[tokio::test]
    async fn test_discovered_deps_affects_cache_key() {
        let db = test_db();
        setup_cc_config(&db);

        let source_hash = Blake3Hash::from_bytes(b"#include \"header.h\"\nint main() {}");

        // First compile: no discovered deps yet
        let source1 = CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();
        let key1 = cache_key_cc_compile(&db, source1).await.unwrap();

        // After first compile, we discover header.h dependency
        // Set discovered deps for this translation unit
        let tu_key = "cc:src/main.c:debug:x86_64-linux-musl".to_string();
        let deps_hash = Blake3Hash::from_bytes(b"include/header.h");
        DiscoveredDeps::new(&db, tu_key, deps_hash, vec!["include/header.h".to_string()]).unwrap();

        // Second compile with same source but now with discovered deps
        let source2 = CSourceFile::new(&db, "src/main.c".to_string(), source_hash).unwrap();
        let key2 = cache_key_cc_compile(&db, source2).await.unwrap();

        // Cache keys should be different because discovered deps changed
        assert_ne!(key1, key2);
    }
}
