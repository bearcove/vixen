//! vx-execd: Execution service
//!
//! Implements the Exec rapace service trait.
//! Runs rustc and streams outputs to CAS.

use camino::Utf8PathBuf;
use std::process::Command;
use std::time::Instant;
use vx_cas_proto::Cas;
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
}
