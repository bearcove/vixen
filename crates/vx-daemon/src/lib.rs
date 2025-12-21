//! vx-daemon: Orchestration daemon with picante incremental queries
//!
//! This daemon:
//! - Owns the picante incremental computation runtime
//! - Parses Cargo.toml and sets up inputs
//! - Computes cache keys via tracked queries
//! - Orchestrates builds via CAS and Exec services

use std::process::Command;

use camino::Utf8PathBuf;
use picante::PicanteResult;
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
// TRACKED QUERIES
// =============================================================================

/// Compute the cache key for compiling a binary crate.
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(db: &DB, source: SourceFile) -> PicanteResult<CacheKey> {
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

// =============================================================================
// DAEMON SERVICE
// =============================================================================

/// The daemon service implementation
pub struct DaemonService {
    /// CAS service for content-addressed storage
    cas: CasService,
}

impl DaemonService {
    /// Create a new daemon service
    pub fn new(cas_root: Utf8PathBuf) -> std::io::Result<Self> {
        let cas = CasService::new(cas_root);
        cas.init()?;
        Ok(Self { cas })
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

        // Set up picante database
        let db = Database::new();

        // Set inputs
        // Keyed input: SourceFile::new(db, key, data_fields...)
        let source_id = SourceFile::new(&db, manifest.bin.path.to_string(), source_hash)
            .map_err(|e| format!("picante error: {}", e))?;

        // Singletons: Type::set(db, field1, field2, ...)
        CargoToml::set(
            &db,
            cargo_toml_hash,
            manifest.name.clone(),
            manifest.edition,
            manifest.bin.path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        RustcVersion::set(&db, rustc_version).map_err(|e| format!("picante error: {}", e))?;

        BuildConfig::set(
            &db,
            profile.to_string(),
            target_triple.clone(),
            project_path.to_string(),
        )
        .map_err(|e| format!("picante error: {}", e))?;

        // Compute cache key via picante
        let cache_key = cache_key_compile_bin(&db, source_id)
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
        let invocation = plan_compile_bin(&db)
            .await
            .map_err(|e| format!("failed to plan build: {}", e))?;

        let node_id = node_id_compile_bin(&db)
            .await
            .map_err(|e| format!("failed to get node id: {}", e))?;

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
