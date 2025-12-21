//! vx-execd: Execution service
//!
//! Implements the Exec rapace service trait.
//! Runs rustc and zig cc, streams outputs to CAS.
//! Materializes toolchains and registry crates on-demand from CAS.

mod registry;

pub use registry::RegistryMaterializer;

use camino::{Utf8Path, Utf8PathBuf};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::Read as _;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, Cas};
use vx_cc::depfile::{canonicalize_deps, parse_depfile_path};
use vx_exec_proto::*;
use vx_toolchain_proto::{CasToolchain, MaterializeStep};

/// Exec service implementation
pub struct ExecService<C: Cas + CasToolchain> {
    /// CAS client for storing outputs and fetching toolchains
    cas: C,
    /// Toolchain materialization directory
    toolchains_dir: Utf8PathBuf,
    /// In-flight materializations (keyed by manifest_hash)
    /// Uses Arc<tokio::sync::Mutex> for async locking
    materializing: Arc<
        tokio::sync::Mutex<
            HashMap<Blake3Hash, Arc<tokio::sync::OnceCell<Result<Utf8PathBuf, String>>>>,
        >,
    >,
}

impl<C: Cas + CasToolchain> ExecService<C> {
    pub fn new(cas: C, toolchains_dir: Utf8PathBuf) -> Self {
        Self {
            cas,
            toolchains_dir,
            materializing: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Ensures a toolchain is materialized locally, returns the materialized directory.
    /// Uses file locking to prevent concurrent materializations.
    async fn ensure_materialized(&self, manifest_hash: Blake3Hash) -> Result<Utf8PathBuf, String> {
        // Check if already materializing
        let cell = {
            let mut map = self.materializing.lock().await;
            map.entry(manifest_hash)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        // Wait for or perform materialization
        cell.get_or_init(|| async { self.materialize_toolchain(manifest_hash).await })
            .await
            .clone()
    }

    /// Materialize a toolchain from CAS to local directory
    async fn materialize_toolchain(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
        info!(manifest_hash = %manifest_hash, "materializing toolchain");

        // Fetch manifest from CAS
        let _manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .ok_or_else(|| format!("toolchain manifest {} not found in CAS", manifest_hash))?;

        // Fetch materialization plan
        let plan = self
            .cas
            .get_materialization_plan(manifest_hash)
            .await
            .ok_or_else(|| {
                format!(
                    "materialization plan for {} not found in CAS",
                    manifest_hash
                )
            })?;

        // Target directory: toolchains/<manifest_hash_hex>
        let target_dir = self.toolchains_dir.join(manifest_hash.to_hex());

        // Check if already materialized
        if target_dir.exists() {
            debug!(target_dir = %target_dir, "toolchain already materialized");
            return Ok(target_dir);
        }

        // Create temp directory for atomic materialization
        let temp_dir = self
            .toolchains_dir
            .join(format!("{}.tmp", manifest_hash.to_hex()));
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir)
                .map_err(|e| format!("failed to remove stale temp dir {}: {}", temp_dir, e))?;
        }
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("failed to create temp dir {}: {}", temp_dir, e))?;

        // Execute materialization plan
        for step in &plan.steps {
            match step {
                MaterializeStep::ExtractTarXz {
                    blob,
                    dest_subdir,
                    strip_components,
                } => {
                    let dest = temp_dir.join(dest_subdir);
                    std::fs::create_dir_all(&dest)
                        .map_err(|e| format!("failed to create dest dir {}: {}", dest, e))?;

                    self.extract_tar_xz_from_cas(*blob, &dest, *strip_components)
                        .await?;
                }
                MaterializeStep::EnsureDir { relpath } => {
                    let dest = temp_dir.join(relpath);
                    std::fs::create_dir_all(&dest)
                        .map_err(|e| format!("failed to create directory {}: {}", dest, e))?;
                }
                MaterializeStep::WriteFile {
                    relpath,
                    blob,
                    mode,
                } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }

                    // Fetch blob from CAS
                    let mut stream = self.cas.stream_blob(*blob).await;
                    let mut data = Vec::new();
                    while let Some(chunk_result) = stream.next().await {
                        let chunk =
                            chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
                        data.extend_from_slice(&chunk);
                    }

                    std::fs::write(&dest, data)
                        .map_err(|e| format!("failed to write file {}: {}", dest, e))?;

                    // Set permissions
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(*mode);
                        std::fs::set_permissions(&dest, perms)
                            .map_err(|e| format!("failed to set permissions on {}: {}", dest, e))?;
                    }
                }
                MaterializeStep::Symlink { relpath, target } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(target, &dest)
                            .map_err(|e| format!("failed to create symlink {}: {}", dest, e))?;
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(format!("symlinks not supported on this platform"));
                    }
                }
            }
        }

        // Atomic rename to final location
        std::fs::rename(&temp_dir, &target_dir)
            .map_err(|e| format!("failed to rename {} to {}: {}", temp_dir, target_dir, e))?;

        info!(target_dir = %target_dir, "toolchain materialized successfully");
        Ok(target_dir)
    }

    /// Extract a tar.xz blob from CAS to a destination directory
    async fn extract_tar_xz_from_cas(
        &self,
        blob_hash: Blake3Hash,
        dest: &Utf8Path,
        strip_components: u32,
    ) -> Result<(), String> {
        debug!(blob = %blob_hash, dest = %dest, strip = strip_components, "extracting tar.xz");

        // Stream blob from CAS
        let mut stream = self.cas.stream_blob(blob_hash).await;
        let mut compressed_data = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
            compressed_data.extend_from_slice(&chunk);
        }

        // Decompress with xz2
        let mut decompressor = xz2::read::XzDecoder::new(&compressed_data[..]);
        let mut tarball_data = Vec::new();
        decompressor
            .read_to_end(&mut tarball_data)
            .map_err(|e| format!("failed to decompress tar.xz: {}", e))?;

        // Extract tar
        let mut archive = tar::Archive::new(&tarball_data[..]);
        for entry in archive
            .entries()
            .map_err(|e| format!("failed to read tar entries: {}", e))?
        {
            let mut entry = entry.map_err(|e| format!("failed to read tar entry: {}", e))?;
            let path = entry
                .path()
                .map_err(|e| format!("failed to get entry path: {}", e))?;

            // Strip components
            let components: Vec<_> = path.components().collect();
            if components.len() <= strip_components as usize {
                continue;
            }
            let stripped_path = Utf8PathBuf::from_path_buf(
                components[strip_components as usize..]
                    .iter()
                    .collect::<std::path::PathBuf>(),
            )
            .map_err(|_| "non-UTF8 path in tarball".to_string())?;

            // Security: validate no path traversal (use component-based check to avoid
            // false positives on valid filenames like "foo..rs")
            for component in stripped_path.components() {
                if matches!(component, camino::Utf8Component::ParentDir) {
                    return Err(format!("path traversal in tarball: {}", stripped_path));
                }
            }

            let target_path = dest.join(&stripped_path);

            // Create parent directories
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create directory {}: {}", parent, e))?;
            }

            // Extract file
            let mut output_file = std::fs::File::create(&target_path)
                .map_err(|e| format!("failed to create file {}: {}", target_path, e))?;
            std::io::copy(&mut entry, &mut output_file)
                .map_err(|e| format!("failed to write file {}: {}", target_path, e))?;

            // Set executable bit if needed
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(mode) = entry.header().mode() {
                    if mode & 0o111 != 0 {
                        let perms = std::fs::Permissions::from_mode(mode);
                        std::fs::set_permissions(&target_path, perms).map_err(|e| {
                            format!("failed to set permissions on {}: {}", target_path, e)
                        })?;
                    }
                }
            }
        }

        Ok(())
    }
}

impl<C: Cas + CasToolchain + Send + Sync> Exec for ExecService<C> {
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult {
        let start = Instant::now();

        // Materialize toolchain
        let toolchain_dir = match self
            .ensure_materialized(invocation.toolchain_manifest)
            .await
        {
            Ok(dir) => dir,
            Err(e) => {
                return ExecuteResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to materialize toolchain: {}", e),
                    duration_ms: start.elapsed().as_millis() as u64,
                    outputs: vec![],
                    manifest_hash: None,
                };
            }
        };

        // Construct paths
        let rustc_path = toolchain_dir.join("bin/rustc");
        let sysroot_path = &toolchain_dir;

        // Build the command with materialized paths
        let mut cmd = Command::new(&rustc_path);

        // Add --sysroot argument (execd's responsibility now)
        cmd.arg("--sysroot");
        cmd.arg(sysroot_path.as_str());

        // Add rest of arguments
        cmd.args(&invocation.args);
        cmd.current_dir(&invocation.cwd);

        // Clean environment, only set what's explicitly provided
        cmd.env_clear();
        for (key, value) in &invocation.env {
            cmd.env(key, value);
        }
        // Always need PATH for the linker
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }

        // Run it
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return ExecuteResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to execute rustc: {}", e),
                    duration_ms: start.elapsed().as_millis() as u64,
                    outputs: vec![],
                    manifest_hash: None,
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // If failed, return early
        if !output.status.success() {
            return ExecuteResult {
                exit_code,
                stdout,
                stderr,
                duration_ms,
                outputs: vec![],
                manifest_hash: None,
            };
        }

        // Ingest outputs to CAS
        let mut outputs = Vec::new();
        for expected in &invocation.expected_outputs {
            let full_path = Utf8PathBuf::from(&invocation.cwd).join(&expected.path);

            let data = match std::fs::read(&full_path) {
                Ok(d) => d,
                Err(e) => {
                    return ExecuteResult {
                        exit_code: -1,
                        stdout,
                        stderr: format!("failed to read output {}: {}", expected.path, e),
                        duration_ms,
                        outputs: vec![],
                        manifest_hash: None,
                    };
                }
            };

            let blob_hash = self.cas.put_blob(data).await;

            outputs.push(ProducedOutput {
                logical: expected.logical.clone(),
                path: expected.path.clone(),
                blob_hash,
                executable: expected.executable,
            });
        }

        ExecuteResult {
            exit_code,
            stdout,
            stderr,
            duration_ms,
            outputs,
            manifest_hash: None, // Daemon will create and store the manifest
        }
    }

    async fn execute_cc(&self, invocation: CcInvocation) -> CcExecuteResult {
        let start = Instant::now();

        // Materialize toolchain
        let toolchain_dir = match self
            .ensure_materialized(invocation.toolchain_manifest)
            .await
        {
            Ok(dir) => dir,
            Err(e) => {
                return CcExecuteResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to materialize toolchain: {}", e),
                    duration_ms: start.elapsed().as_millis() as u64,
                    outputs: vec![],
                    discovered_deps: vec![],
                    manifest_hash: None,
                };
            }
        };

        // Construct zig path
        let zig_path = toolchain_dir.join("zig");

        debug!(
            zig_path = %zig_path,
            args = ?invocation.args,
            cwd = %invocation.cwd,
            "executing zig cc"
        );

        // Ensure output directories exist
        for expected in &invocation.expected_outputs {
            let full_path = Utf8PathBuf::from(&invocation.cwd).join(&expected.path);
            if let Some(parent) = full_path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return CcExecuteResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to create output directory {}: {}", parent, e),
                    duration_ms: start.elapsed().as_millis() as u64,
                    outputs: vec![],
                    discovered_deps: vec![],
                    manifest_hash: None,
                };
            }
        }

        // Build the command with materialized path
        let mut cmd = Command::new(&zig_path);
        cmd.args(&invocation.args);
        cmd.current_dir(&invocation.cwd);

        // Clean environment for hermeticity
        cmd.env_clear();
        for (key, value) in &invocation.env {
            cmd.env(key, value);
        }

        // Run zig cc
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return CcExecuteResult {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!("failed to execute zig cc: {}", e),
                    duration_ms: start.elapsed().as_millis() as u64,
                    outputs: vec![],
                    discovered_deps: vec![],
                    manifest_hash: None,
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        debug!(
            exit_code,
            duration_ms,
            stderr_len = stderr.len(),
            "zig cc completed"
        );

        // If failed, return early
        if !output.status.success() {
            return CcExecuteResult {
                exit_code,
                stdout,
                stderr,
                duration_ms,
                outputs: vec![],
                discovered_deps: vec![],
                manifest_hash: None,
            };
        }

        // Parse depfile if present
        let discovered_deps = if let Some(ref depfile_path) = invocation.depfile_path {
            let full_depfile_path = Utf8PathBuf::from(&invocation.cwd).join(depfile_path);
            parse_and_canonicalize_depfile(
                &full_depfile_path,
                Utf8Path::new(&invocation.cwd),
                Utf8Path::new(&invocation.workspace_root),
            )
        } else {
            vec![]
        };

        // Ingest outputs to CAS
        let mut outputs = Vec::new();
        for expected in &invocation.expected_outputs {
            let full_path = Utf8PathBuf::from(&invocation.cwd).join(&expected.path);

            let data = match std::fs::read(&full_path) {
                Ok(d) => d,
                Err(e) => {
                    // Depfile might not exist if compilation failed early
                    // (though we already checked exit code above)
                    if expected.logical == "depfile" {
                        warn!(path = %full_path, error = %e, "depfile not found, skipping");
                        continue;
                    }
                    return CcExecuteResult {
                        exit_code: -1,
                        stdout,
                        stderr: format!("failed to read output {}: {}", expected.path, e),
                        duration_ms,
                        outputs: vec![],
                        discovered_deps: vec![],
                        manifest_hash: None,
                    };
                }
            };

            let blob_hash = self.cas.put_blob(data).await;

            outputs.push(ProducedOutput {
                logical: expected.logical.clone(),
                path: expected.path.clone(),
                blob_hash,
                executable: expected.executable,
            });
        }

        debug!(
            outputs_count = outputs.len(),
            deps_count = discovered_deps.len(),
            "cc execution complete"
        );

        CcExecuteResult {
            exit_code,
            stdout,
            stderr,
            duration_ms,
            outputs,
            discovered_deps,
            manifest_hash: None, // Daemon will create and store the manifest
        }
    }
}

/// Parse a depfile and canonicalize its dependencies to workspace-relative paths
fn parse_and_canonicalize_depfile(
    depfile_path: &Utf8Path,
    base_dir: &Utf8Path,
    workspace_root: &Utf8Path,
) -> Vec<String> {
    match parse_depfile_path(depfile_path) {
        Ok(parsed) => match canonicalize_deps(&parsed.deps, base_dir, workspace_root) {
            Ok(canonical) => canonical.into_iter().map(|p| p.to_string()).collect(),
            Err(e) => {
                warn!(
                    depfile = %depfile_path,
                    error = %e,
                    "failed to canonicalize depfile dependencies"
                );
                vec![]
            }
        },
        Err(e) => {
            warn!(
                depfile = %depfile_path,
                error = %e,
                "failed to parse depfile"
            );
            vec![]
        }
    }
}
