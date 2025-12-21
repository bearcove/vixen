//! vx-execd: Execution service
//!
//! Implements the Exec rapace service trait.
//! Runs rustc and zig cc, streams outputs to CAS.

use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;
use std::time::Instant;
use tracing::{debug, warn};
use vx_cas_proto::Cas;
use vx_cc::depfile::{canonicalize_deps, parse_depfile_path};
use vx_exec_proto::*;

/// Exec service implementation
pub struct ExecService<C: Cas> {
    /// CAS client for storing outputs
    cas: C,
}

impl<C: Cas> ExecService<C> {
    pub fn new(cas: C) -> Self {
        Self { cas }
    }
}

impl<C: Cas + Send + Sync> Exec for ExecService<C> {
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult {
        let start = Instant::now();

        // Build the command
        let mut cmd = Command::new(&invocation.program);
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
                    stderr: format!("failed to execute {}: {}", invocation.program, e),
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

        debug!(
            zig_path = %invocation.zig_path,
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

        // Build the command
        let mut cmd = Command::new(&invocation.zig_path);
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
                    stderr: format!("failed to execute {}: {}", invocation.zig_path, e),
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
