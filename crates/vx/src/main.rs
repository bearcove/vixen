//! vx - Build execution engine with deterministic caching
//!
//! v0: Single-crate Rust projects with no dependencies

use camino::Utf8PathBuf;
use eyre::{Result, WrapErr, bail};
use facet::Facet;
use facet_args as args;
use owo_colors::OwoColorize;
use std::process::Command;

use vx_cas_proto::{Blake3Hash, CACHE_KEY_SCHEMA_VERSION, Cas, NodeId, NodeManifest, OutputEntry};
use vx_casd::CasService;
use vx_manifest::{Edition, Manifest};

/// vx - Build execution engine with deterministic caching
#[derive(Facet, Debug)]
struct Cli {
    /// Show version information
    #[facet(args::named, args::short = 'V')]
    version: bool,

    /// Command to run
    #[facet(args::subcommand)]
    command: CliCommand,
}

#[derive(Facet, Debug)]
#[repr(u8)]
enum CliCommand {
    /// Execute a build
    Build {
        /// Build in release mode
        #[facet(args::named, args::short = 'r')]
        release: bool,
    },

    /// Explain the last build
    Explain,
}

fn main() -> Result<()> {
    let cli: Cli = args::from_std_args()?;

    if cli.version {
        println!("vx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match cli.command {
        CliCommand::Build { release } => cmd_build(release),
        CliCommand::Explain => cmd_explain(),
    }
}

fn cmd_build(release: bool) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;
    let cargo_toml_path = cwd.join("Cargo.toml");

    if !cargo_toml_path.exists() {
        bail!("no Cargo.toml found in {}", cwd);
    }

    // Parse manifest
    let manifest = Manifest::from_path(&cargo_toml_path).wrap_err("failed to parse Cargo.toml")?;

    println!(
        "{} {} v0.1.0 ({})",
        "Building".green().bold(),
        manifest.name,
        cwd
    );

    // Initialize CAS
    let vx_dir = cwd.join(".vx");
    let cas_root = vx_dir.join("cas");
    let cas = CasService::new(cas_root);
    cas.init().wrap_err("failed to initialize CAS")?;

    // Get rustc version
    let rustc_version = get_rustc_version()?;

    // Hash source file
    let main_rs_path = cwd.join(&manifest.bin.path);
    if !main_rs_path.exists() {
        bail!("source file not found: {}", main_rs_path);
    }
    let main_rs_content = std::fs::read(&main_rs_path)
        .wrap_err_with(|| format!("failed to read {}", main_rs_path))?;
    let source_hash = Blake3Hash::from_bytes(&main_rs_content);

    // Hash Cargo.toml
    let cargo_toml_content = std::fs::read(&cargo_toml_path)?;
    let cargo_toml_hash = Blake3Hash::from_bytes(&cargo_toml_content);

    // Compute cache key
    let profile = if release { "release" } else { "debug" };
    let target_triple = get_target_triple()?;

    let cache_key = compute_cache_key(
        &rustc_version,
        &target_triple,
        profile,
        &manifest.name,
        manifest.edition,
        &source_hash,
        &cargo_toml_hash,
    );

    println!("  {} {}", "Cache key:".dimmed(), &cache_key.to_hex()[..16]);

    // Build output directory
    let output_dir = vx_dir.join("build").join(&target_triple).join(profile);
    std::fs::create_dir_all(&output_dir)?;

    // Check cache
    let rt = tokio::runtime::Runtime::new()?;

    if let Some(cached_manifest_hash) = rt.block_on(cas.lookup(cache_key.clone())) {
        // Cache hit - materialize from CAS
        let cached_manifest = rt
            .block_on(cas.get_manifest(cached_manifest_hash))
            .ok_or_else(|| eyre::eyre!("cached manifest not found"))?;

        // Materialize each output (client responsibility)
        for output in &cached_manifest.outputs {
            let blob_data = rt
                .block_on(cas.get_blob(output.blob.clone()))
                .ok_or_else(|| eyre::eyre!("blob {} not found", output.blob.to_hex()))?;

            let dest_path = output_dir.join(&output.filename);
            atomic_write_file(&dest_path, &blob_data, output.executable)?;
        }

        println!("{} {} (cached)", "Finished".green().bold(), profile);
        println!(
            "  {} .vx/build/{}/{}/{}",
            "Binary:".dimmed(),
            target_triple,
            profile,
            manifest.name
        );
        return Ok(());
    }

    // Cache miss - need to build
    println!("  {} cache miss, compiling...", "→".yellow());

    let output_path = output_dir.join(&manifest.name);

    // Build rustc invocation
    let mut rustc_args = vec![
        "--crate-name".to_string(),
        manifest.name.clone(),
        "--crate-type".to_string(),
        "bin".to_string(),
        "--edition".to_string(),
        manifest.edition.as_str().to_string(),
        format!("--remap-path-prefix={}=/vx-workspace", cwd),
        manifest.bin.path.to_string(),
        "-o".to_string(),
        output_path.to_string(),
    ];

    if release {
        rustc_args.push("-C".to_string());
        rustc_args.push("opt-level=3".to_string());
    }

    // Execute rustc
    let start = std::time::Instant::now();
    let output = Command::new("rustc")
        .args(&rustc_args)
        .current_dir(&cwd)
        .output()
        .wrap_err("failed to execute rustc")?;

    let duration = start.elapsed();

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        bail!(
            "rustc failed with exit code {}",
            output.status.code().unwrap_or(-1)
        );
    }

    // Read the output and store in CAS
    let binary_data = std::fs::read(&output_path)
        .wrap_err_with(|| format!("failed to read output {}", output_path))?;
    let blob_hash = rt.block_on(cas.put_blob(binary_data));

    // Create and store manifest
    let node_manifest = NodeManifest {
        node_id: NodeId(format!(
            "compile-bin:{}:{}:{}",
            manifest.name, target_triple, profile
        )),
        cache_key: cache_key.clone(),
        produced_at: unix_timestamp(),
        outputs: vec![OutputEntry {
            logical: "bin".to_string(),
            filename: manifest.name.clone(),
            blob: blob_hash,
            executable: true,
        }],
    };

    let new_manifest_hash = rt.block_on(cas.put_manifest(node_manifest));

    // Publish atomically (validates manifest exists before writing cache mapping)
    let publish_result = rt.block_on(cas.publish(cache_key, new_manifest_hash));
    if !publish_result.success {
        bail!("failed to publish cache entry: {:?}", publish_result.error);
    }

    println!(
        "{} {} in {:.2}s",
        "Finished".green().bold(),
        profile,
        duration.as_secs_f64()
    );
    println!(
        "  {} .vx/build/{}/{}/{}",
        "Binary:".dimmed(),
        target_triple,
        profile,
        manifest.name
    );

    Ok(())
}

/// Atomically write a file with optional executable permission
fn atomic_write_file(dest: &Utf8PathBuf, data: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;

    // Write to temp file first
    let tmp_path = dest.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);

    // Set executable permission on Unix
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&tmp_path, perms)?;
    }

    // Atomic rename
    std::fs::rename(&tmp_path, dest)?;

    Ok(())
}

fn cmd_explain() -> Result<()> {
    println!("explain command not yet implemented for v0");
    Ok(())
}

fn get_rustc_version() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .wrap_err("failed to run rustc -vV")?;

    if !output.status.success() {
        bail!("rustc -vV failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn get_target_triple() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .wrap_err("failed to run rustc -vV")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.starts_with("host: ") {
            return Ok(line[6..].to_string());
        }
    }

    bail!("could not determine host triple from rustc -vV")
}

fn compute_cache_key(
    rustc_version: &str,
    target_triple: &str,
    profile: &str,
    name: &str,
    edition: Edition,
    source_hash: &Blake3Hash,
    manifest_hash: &Blake3Hash,
) -> Blake3Hash {
    let mut hasher = blake3::Hasher::new();

    // Schema version - bump CACHE_KEY_SCHEMA_VERSION when canonicalization changes
    hasher.update(b"vx-cache-key-v");
    hasher.update(&CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    // Toolchain identity
    hasher.update(b"rustc:");
    hasher.update(rustc_version.as_bytes());
    hasher.update(b"\n");

    // Build configuration
    hasher.update(b"target:");
    hasher.update(target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(profile.as_bytes());
    hasher.update(b"\n");

    // Crate identity
    hasher.update(b"name:");
    hasher.update(name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_str().as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    // Source inputs (content hashes)
    hasher.update(b"source:");
    hasher.update(&source_hash.0);
    hasher.update(b"\n");

    hasher.update(b"manifest:");
    hasher.update(&manifest_hash.0);
    hasher.update(b"\n");

    Blake3Hash(*hasher.finalize().as_bytes())
}

fn unix_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}
