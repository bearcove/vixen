use camino::{Utf8Path, Utf8PathBuf};
use eyre::Result;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io::Read as _;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, Cas};
use vx_cas_proto::{CasClient, MaterializeStep};
use vx_cc::depfile::{canonicalize_deps, parse_depfile_path};
use vx_exec_proto::{
    CcExecuteResult, CcInvocation, Exec, ExecuteResult, MaterializeStatus, ProducedOutput,
    RegistryMaterializeRequest, RegistryMaterializeResult, RustcInvocation,
};

use crate::{ExecService, parse_and_canonicalize_depfile};

impl Exec for ExecService {
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

            let data = match tokio::fs::read(&full_path).await {
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
            if let Some(parent) = full_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
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

            let data = match tokio::fs::read(&full_path).await {
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

    async fn materialize_registry_crate(
        &self,
        request: RegistryMaterializeRequest,
    ) -> RegistryMaterializeResult {
        // Validate workspace_root: must be non-empty and absolute
        if request.workspace_root.is_empty() {
            return RegistryMaterializeResult {
                workspace_rel_path: String::new(),
                was_cached: false,
                status: MaterializeStatus::Failed,
                error: Some("workspace_root cannot be empty".to_string()),
            };
        }

        let workspace_root = Utf8Path::new(&request.workspace_root);

        if !workspace_root.is_absolute() {
            return RegistryMaterializeResult {
                workspace_rel_path: String::new(),
                was_cached: false,
                status: MaterializeStatus::Failed,
                error: Some(format!(
                    "workspace_root must be absolute, got: {}",
                    workspace_root
                )),
            };
        }

        match self
            .registry_materializer
            .materialize(request.manifest_hash, workspace_root)
            .await
        {
            Ok(result) => RegistryMaterializeResult {
                workspace_rel_path: result.workspace_rel_path,
                was_cached: result.was_cached,
                status: MaterializeStatus::Ok,
                error: None,
            },
            Err(e) => RegistryMaterializeResult {
                workspace_rel_path: String::new(),
                was_cached: false,
                status: MaterializeStatus::Failed,
                error: Some(e),
            },
        }
    }
}
