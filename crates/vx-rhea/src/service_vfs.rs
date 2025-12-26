//! VFS-based action execution
//!
//! This module implements compilation using the VFS instead of disk materialization.
//! Files are served lazily from CAS via the fs-kitty mount at ~/.rhea/.

use camino::Utf8PathBuf;
use jiff::Zoned;
use std::process::Command;
use std::time::Instant;
use tracing::{debug, info};
use vx_cass_proto::{NodeId, NodeManifest, OutputEntry};
use vx_rhea_proto::{CcCompileRequest, CcCompileResult, RustCompileRequest, RustCompileResult};

use crate::RheaService;

/// VFS mount point base path
/// This is where fs-kitty mounts the virtual filesystem
fn rhea_mount_base() -> Utf8PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    Utf8PathBuf::from(format!("{}/.rhea", home))
}

impl RheaService {
    /// Compile a Rust crate using VFS-based execution
    pub async fn compile_rust_vfs(&self, request: RustCompileRequest) -> RustCompileResult {
        let start = Instant::now();

        info!(
            crate_name = %request.crate_name,
            crate_type = %request.crate_type,
            deps_count = request.deps.len(),
            "compiling rust crate (VFS mode)"
        );

        // 1. Create action prefix
        let prefix_id = self.create_action_prefix().await;
        let prefix_path = rhea_mount_base().join(&prefix_id);

        debug!(prefix_id = %prefix_id, prefix_path = %prefix_path, "created action prefix");

        // Helper to return an error result (cleanup is done before calling this)
        let cleanup_and_error = |_prefix_id: &str, error: String, start: Instant| {
            RustCompileResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                output_manifest: None,
                error: Some(error),
            }
        };

        // 2. Add toolchain to prefix
        if let Err(e) = self
            .add_toolchain_to_prefix(&prefix_id, request.toolchain_manifest)
            .await
        {
            self.remove_action_prefix(&prefix_id).await;
            return cleanup_and_error(&prefix_id, format!("failed to add toolchain: {}", e), start);
        }

        // 3. Add source tree to prefix
        if let Err(e) = self
            .add_source_tree_to_prefix(&prefix_id, request.source_manifest)
            .await
        {
            self.remove_action_prefix(&prefix_id).await;
            return cleanup_and_error(&prefix_id, format!("failed to add sources: {}", e), start);
        }

        // 4. Add dependencies to prefix
        for dep in &request.deps {
            if let Err(e) = self
                .add_dep_to_prefix(&prefix_id, &dep.extern_name, dep.manifest_hash)
                .await
            {
                self.remove_action_prefix(&prefix_id).await;
                return cleanup_and_error(
                    &prefix_id,
                    format!("failed to add dep {}: {}", dep.extern_name, e),
                    start,
                );
            }
        }

        // 5. Create output directory
        self.create_output_dir_in_prefix(&prefix_id).await;

        // 6. Build rustc command
        let rustc_path = prefix_path.join("toolchain/bin/rustc");
        let sysroot_path = prefix_path.join("toolchain/sysroot");
        let src_path = prefix_path.join("src");
        let deps_path = prefix_path.join("deps");
        let out_path = prefix_path.join("out");

        let output_filename = if request.crate_type == "lib" {
            format!("lib{}.rlib", request.crate_name)
        } else {
            request.crate_name.clone()
        };
        let output_file = out_path.join(&output_filename);

        let crate_root = src_path.join(&request.crate_root);

        let mut cmd = Command::new(rustc_path.as_str());
        cmd.arg("--sysroot").arg(sysroot_path.as_str());
        cmd.arg("--crate-name").arg(&request.crate_name);
        cmd.arg("--crate-type").arg(&request.crate_type);
        cmd.arg("--edition").arg(&request.edition);
        cmd.arg("-o").arg(output_file.as_str());

        // Path remapping for reproducibility
        cmd.arg(format!("--remap-path-prefix={}=/src", src_path));

        // Add --extern flags for dependencies
        for dep in &request.deps {
            let rlib_path = deps_path.join(format!("lib{}.rlib", dep.extern_name));
            cmd.arg("--extern");
            cmd.arg(format!("{}={}", dep.extern_name, rlib_path));
        }

        // Profile-specific flags
        if request.profile == "release" {
            cmd.arg("-C").arg("opt-level=3");
        }

        // Crate root
        cmd.arg(crate_root.as_str());

        // Working directory
        cmd.current_dir(prefix_path.as_str());

        // Clean environment for hermeticity
        cmd.env_clear();
        // Need PATH for linker (for now - later sandbox will provide this)
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }

        debug!(
            rustc = %rustc_path,
            crate_name = %request.crate_name,
            crate_root = %crate_root,
            "executing rustc via VFS"
        );

        // 7. Run rustc
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                self.remove_action_prefix(&prefix_id).await;
                return cleanup_and_error(&prefix_id, format!("failed to execute rustc: {}", e), start);
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            self.remove_action_prefix(&prefix_id).await;
            return RustCompileResult {
                success: false,
                exit_code,
                stdout,
                stderr,
                duration_ms,
                output_manifest: None,
                error: None,
            };
        }

        // 8. Flush outputs from VFS to CAS
        let flushed = self.flush_prefix_outputs(&prefix_id).await;

        // Find the output blob hash
        let output_rel_path = format!("out/{}", output_filename);
        let blob_hash = match flushed.get(&output_rel_path) {
            Some(hash) => *hash,
            None => {
                self.remove_action_prefix(&prefix_id).await;
                return cleanup_and_error(
                    &prefix_id,
                    format!("output {} not found in VFS after compilation", output_rel_path),
                    start,
                );
            }
        };

        // 9. Create and store output manifest
        let logical = if request.crate_type == "lib" {
            "rlib"
        } else {
            "bin"
        };

        let node_id = NodeId(format!(
            "compile-{}:{}:{}:{}",
            request.crate_type, request.crate_name, request.target_triple, request.profile
        ));

        let cache_key = vx_cache::rust_compile_cache_key(&request);

        let manifest = NodeManifest {
            node_id,
            cache_key,
            produced_at: Zoned::now().to_string(),
            outputs: vec![OutputEntry {
                logical: logical.to_string(),
                filename: output_filename,
                blob: blob_hash,
                executable: request.crate_type == "bin",
            }],
        };

        let manifest_hash = match self.cas.put_manifest(manifest).await {
            Ok(h) => h,
            Err(e) => {
                self.remove_action_prefix(&prefix_id).await;
                return cleanup_and_error(
                    &prefix_id,
                    format!("failed to store manifest: {:?}", e),
                    start,
                );
            }
        };

        // 10. Cleanup
        self.remove_action_prefix(&prefix_id).await;

        info!(
            crate_name = %request.crate_name,
            manifest_hash = %manifest_hash,
            duration_ms,
            "compilation complete (VFS mode)"
        );

        RustCompileResult {
            success: true,
            exit_code,
            stdout,
            stderr,
            duration_ms,
            output_manifest: Some(manifest_hash),
            error: None,
        }
    }

    /// Compile a C source file using VFS-based execution
    pub async fn compile_cc_vfs(&self, request: CcCompileRequest) -> CcCompileResult {
        let start = Instant::now();

        info!(
            source = %request.source_path,
            target = %request.target_triple,
            "compiling C source (VFS mode)"
        );

        // 1. Create action prefix
        let prefix_id = self.create_action_prefix().await;
        let prefix_path = rhea_mount_base().join(&prefix_id);

        // 2. Add Zig toolchain to prefix
        if let Err(e) = self
            .add_toolchain_to_prefix(&prefix_id, request.toolchain_manifest)
            .await
        {
            self.remove_action_prefix(&prefix_id).await;
            return CcCompileResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                output_manifest: None,
                discovered_deps: vec![],
                error: Some(format!("failed to add toolchain: {}", e)),
            };
        }

        // 3. Add source tree to prefix
        if let Err(e) = self
            .add_source_tree_to_prefix(&prefix_id, request.source_manifest)
            .await
        {
            self.remove_action_prefix(&prefix_id).await;
            return CcCompileResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                output_manifest: None,
                discovered_deps: vec![],
                error: Some(format!("failed to add sources: {}", e)),
            };
        }

        // 4. Create output directory
        self.create_output_dir_in_prefix(&prefix_id).await;

        // 5. Build zig cc command
        let zig_path = prefix_path.join("toolchain/zig");
        let src_path = prefix_path.join("src");
        let out_path = prefix_path.join("out");

        let source_file = src_path.join(&request.source_path);
        let object_filename = format!(
            "{}.o",
            request
                .source_path
                .strip_suffix(".c")
                .unwrap_or(&request.source_path)
        );
        let object_file = out_path.join(&object_filename);

        let mut cmd = Command::new(zig_path.as_str());
        cmd.arg("cc");
        cmd.arg("-c");
        cmd.arg(source_file.as_str());
        cmd.arg("-o").arg(object_file.as_str());

        // Profile-specific flags
        if request.profile == "release" {
            cmd.arg("-O3");
        } else {
            cmd.arg("-O0");
            cmd.arg("-g");
        }

        // Add include paths (relative to src/)
        for include_path in &request.include_paths {
            cmd.arg("-I").arg(src_path.join(include_path).as_str());
        }

        // Add defines
        for (key, value) in &request.defines {
            if let Some(v) = value {
                cmd.arg(format!("-D{}={}", key, v));
            } else {
                cmd.arg(format!("-D{}", key));
            }
        }

        cmd.current_dir(prefix_path.as_str());

        // Clean environment
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
        }
        cmd.env("ZIG_LIB_DIR", prefix_path.join("toolchain/lib").as_str());

        debug!(
            zig = %zig_path,
            source = %request.source_path,
            output = %object_file,
            "executing zig cc via VFS"
        );

        // 6. Run zig cc
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                self.remove_action_prefix(&prefix_id).await;
                return CcCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    output_manifest: None,
                    discovered_deps: vec![],
                    error: Some(format!("failed to execute zig cc: {}", e)),
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            self.remove_action_prefix(&prefix_id).await;
            return CcCompileResult {
                success: false,
                exit_code,
                stdout,
                stderr,
                duration_ms,
                output_manifest: None,
                discovered_deps: vec![],
                error: None,
            };
        }

        // 7. Flush outputs from VFS to CAS
        let flushed = self.flush_prefix_outputs(&prefix_id).await;

        let output_rel_path = format!("out/{}", object_filename);
        let blob_hash = match flushed.get(&output_rel_path) {
            Some(hash) => *hash,
            None => {
                self.remove_action_prefix(&prefix_id).await;
                return CcCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                    duration_ms,
                    output_manifest: None,
                    discovered_deps: vec![],
                    error: Some(format!("output {} not found in VFS", output_rel_path)),
                };
            }
        };

        // 8. Create manifest
        let node_id = NodeId(format!(
            "compile-c:{}:{}:{}",
            request.source_path, request.target_triple, request.profile
        ));

        let cache_key = vx_cache::cc_compile_cache_key(&request);

        let manifest = NodeManifest {
            node_id,
            cache_key,
            produced_at: Zoned::now().to_string(),
            outputs: vec![OutputEntry {
                logical: "object".to_string(),
                filename: object_filename,
                blob: blob_hash,
                executable: false,
            }],
        };

        let manifest_hash = match self.cas.put_manifest(manifest).await {
            Ok(h) => h,
            Err(e) => {
                self.remove_action_prefix(&prefix_id).await;
                return CcCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                    duration_ms,
                    output_manifest: None,
                    discovered_deps: vec![],
                    error: Some(format!("failed to store manifest: {:?}", e)),
                };
            }
        };

        // 9. Cleanup
        self.remove_action_prefix(&prefix_id).await;

        info!(
            source = %request.source_path,
            manifest_hash = %manifest_hash,
            duration_ms,
            "C compilation complete (VFS mode)"
        );

        CcCompileResult {
            success: true,
            exit_code,
            stdout,
            stderr,
            duration_ms,
            output_manifest: Some(manifest_hash),
            discovered_deps: vec![], // TODO: parse depfile if generated
            error: None,
        }
    }
}
