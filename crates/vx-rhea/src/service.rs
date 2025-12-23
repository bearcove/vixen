use camino::{Utf8Path, Utf8PathBuf};
use jiff::Zoned;
use std::process::Command;
use std::time::Instant;
use tracing::{debug, info, warn};
use vx_oort_proto::ServiceVersion;
use vx_oort_proto::{NodeId, NodeManifest, OutputEntry};
use vx_rhea_proto::{
    CcCompileRequest, CcCompileResult, RHEA_PROTOCOL_VERSION, Rhea, RustCompileRequest,
    RustCompileResult,
};

use crate::{RheaResult, RheaService};

/// Information about a crate extracted from its Cargo.toml
struct CrateInfo {
    /// The path to the crate root file (e.g., "src/lib.rs")
    lib_path: Utf8PathBuf,
    /// The Rust edition
    edition: String,
}

impl Rhea for RheaService {
    async fn version(&self) -> ServiceVersion {
        ServiceVersion {
            service: "vx-rhea".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: RHEA_PROTOCOL_VERSION,
        }
    }

    async fn compile_rust(&self, request: RustCompileRequest) -> RustCompileResult {
        let start = Instant::now();

        info!(
            crate_name = %request.crate_name,
            crate_type = %request.crate_type,
            deps_count = request.deps.len(),
            "compiling rust crate"
        );

        // 1. Materialize toolchain
        let toolchain_dir = match self.ensure_materialized(request.toolchain_manifest).await {
            Ok(dir) => dir,
            Err(e) => {
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    output_manifest: None,
                    error: Some(format!("failed to materialize toolchain: {}", e)),
                };
            }
        };

        // 2. Materialize source tree to a scratch directory
        let scratch_dir = match self.create_scratch_dir().await {
            Ok(dir) => dir,
            Err(e) => {
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    output_manifest: None,
                    error: Some(format!("failed to create scratch directory: {}", e)),
                };
            }
        };

        // Determine effective edition and crate_root based on whether this is a registry crate
        let (effective_edition, effective_crate_root) =
            if let Some(registry_manifest) = request.registry_crate_manifest {
                // Registry crate: materialize via registry_materializer and parse Cargo.toml
                let result = match self
                    .registry_materializer
                    .materialize(registry_manifest, &scratch_dir)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                            warn!(
                                "failed to remove scratch directory after registry crate materialization failure: {cleanup_err}"
                            );
                        }
                        return RustCompileResult {
                            success: false,
                            exit_code: -1,
                            stdout: String::new(),
                            stderr: String::new(),
                            duration_ms: start.elapsed().as_millis() as u64,
                            output_manifest: None,
                            error: Some(format!(
                                "failed to materialize registry crate {}: {}",
                                request.crate_name, e
                            )),
                        };
                    }
                };

                // Parse Cargo.toml to get actual edition and lib_path
                let crate_dir = scratch_dir.join(&result.workspace_rel_path);
                let crate_info = match parse_crate_info(&crate_dir).await {
                    Ok(info) => info,
                    Err(e) => {
                        if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                            warn!(
                                "failed to remove scratch directory after Cargo.toml parse failure: {cleanup_err}"
                            );
                        }
                        return RustCompileResult {
                            success: false,
                            exit_code: -1,
                            stdout: String::new(),
                            stderr: String::new(),
                            duration_ms: start.elapsed().as_millis() as u64,
                            output_manifest: None,
                            error: Some(format!(
                                "failed to parse Cargo.toml for {}: {}",
                                request.crate_name, e
                            )),
                        };
                    }
                };

                // The crate root is relative to scratch_dir
                let crate_root_relative_to_scratch =
                    Utf8PathBuf::from(&result.workspace_rel_path).join(&crate_info.lib_path);

                info!(
                    crate_name = %request.crate_name,
                    edition = %crate_info.edition,
                    crate_root = %crate_root_relative_to_scratch,
                    "parsed registry crate metadata"
                );

                (crate_info.edition, crate_root_relative_to_scratch.to_string())
            } else {
                // Path crate: materialize source tree from CAS, use request values
                if let Err(e) = self
                    .materialize_tree_from_cas(request.source_manifest, &scratch_dir)
                    .await
                {
                    if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after source tree materialization failure: {cleanup_err}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!("failed to materialize source tree: {}", e)),
                    };
                }
                (request.edition.clone(), request.crate_root.clone())
            };

        // 3. Materialize dependencies (rlibs) into scratch_dir/.vx/deps/<extern_name>.rlib
        let deps_dir = scratch_dir.join(".vx/deps");
        if !request.deps.is_empty()
            && let Err(e) = tokio::fs::create_dir_all(&deps_dir).await
        {
            if let Err(e) = tokio::fs::remove_dir_all(&scratch_dir).await {
                warn!(
                    "failed to remove scratch directory after deps directory creation failure: {e}"
                );
            }
            return RustCompileResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                output_manifest: None,
                error: Some(format!("failed to create deps directory: {}", e)),
            };
        }

        let mut extern_args: Vec<(String, Utf8PathBuf)> = Vec::new();
        for dep in &request.deps {
            // If this is a registry dependency, materialize its source first
            if let Some(registry_manifest_hash) = dep.registry_crate_manifest {
                if let Err(e) = self
                    .registry_materializer
                    .materialize(registry_manifest_hash, &scratch_dir)
                    .await
                {
                    if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after registry crate materialization failure: {cleanup_err}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!(
                            "failed to materialize registry crate for {}: {}",
                            dep.extern_name, e
                        )),
                    };
                }
            }

            let rlib_path = deps_dir.join(format!("lib{}.rlib", dep.extern_name));

            // Fetch the dep's manifest to get the rlib blob hash
            let dep_manifest = match self.cas.get_manifest(dep.manifest_hash).await {
                Ok(Some(m)) => m,
                Ok(None) => {
                    if let Err(e) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after dependency manifest not found: {e}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!(
                            "dependency manifest {} not found in CAS",
                            dep.manifest_hash
                        )),
                    };
                }
                Err(e) => {
                    if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after dependency manifest fetch failure: {cleanup_err}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!("failed to fetch dependency manifest: {:?}", e)),
                    };
                }
            };

            // Find the rlib output in the manifest
            let rlib_entry = dep_manifest
                .outputs
                .iter()
                .find(|o| o.logical == "rlib")
                .ok_or_else(|| format!("no rlib output in manifest for {}", dep.extern_name));

            let rlib_entry = match rlib_entry {
                Ok(e) => e,
                Err(e) => {
                    if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after rlib entry not found: {cleanup_err}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(e),
                    };
                }
            };

            // Fetch the rlib blob and write it
            let rlib_data = match self.cas.get_blob(rlib_entry.blob).await {
                Ok(Some(data)) => data,
                Ok(None) => {
                    if let Err(e) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!("failed to remove scratch directory after rlib blob not found: {e}");
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!(
                            "rlib blob {} not found in CAS for {}",
                            rlib_entry.blob, dep.extern_name
                        )),
                    };
                }
                Err(e) => {
                    if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                        warn!(
                            "failed to remove scratch directory after rlib blob fetch failure: {cleanup_err}"
                        );
                    }
                    return RustCompileResult {
                        success: false,
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        output_manifest: None,
                        error: Some(format!("failed to fetch rlib blob: {:?}", e)),
                    };
                }
            };

            if let Err(e) = tokio::fs::write(&rlib_path, &rlib_data).await {
                if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                    warn!(
                        "failed to remove scratch directory after rlib write failure: {cleanup_err}"
                    );
                }
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    output_manifest: None,
                    error: Some(format!("failed to write rlib: {}", e)),
                };
            }

            extern_args.push((dep.extern_name.clone(), rlib_path));
        }

        // 4. Build rustc command
        let rustc_path = toolchain_dir.join("bin/rustc");
        let sysroot_path = &toolchain_dir;

        // Determine output path
        let output_filename = if request.crate_type == "lib" {
            format!("lib{}.rlib", request.crate_name)
        } else {
            request.crate_name.clone()
        };
        let output_dir = scratch_dir.join(".vx/out");
        let output_path = output_dir.join(&output_filename);

        if let Err(e) = tokio::fs::create_dir_all(&output_dir).await {
            if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                warn!(
                    "failed to remove scratch directory after output directory creation failure: {cleanup_err}"
                );
            }
            return RustCompileResult {
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                output_manifest: None,
                error: Some(format!("failed to create output directory: {}", e)),
            };
        }

        let mut cmd = Command::new(&rustc_path);
        cmd.arg("--sysroot").arg(sysroot_path.as_str());
        cmd.arg("--crate-name").arg(&request.crate_name);
        cmd.arg("--crate-type").arg(&request.crate_type);
        cmd.arg("--edition").arg(&effective_edition);
        cmd.arg("-o").arg(&output_path);

        // Path remapping for reproducibility
        cmd.arg(format!("--remap-path-prefix={}=/src", scratch_dir));

        // Add --extern flags for dependencies
        for (extern_name, rlib_path) in &extern_args {
            cmd.arg("--extern");
            cmd.arg(format!("{}={}", extern_name, rlib_path));
        }

        // Profile-specific flags
        if request.profile == "release" {
            cmd.arg("-C").arg("opt-level=3");
        }

        // Crate root (relative to scratch dir)
        cmd.arg(&effective_crate_root);

        cmd.current_dir(&scratch_dir);

        // Clean environment for hermeticity
        cmd.env_clear();
        // Need PATH for linker
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }

        debug!(
            rustc = %rustc_path,
            crate_name = %request.crate_name,
            crate_root = %effective_crate_root,
            "executing rustc"
        );

        // 5. Run rustc
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                    warn!(
                        "failed to remove scratch directory after rustc execution failure: {cleanup_err}"
                    );
                }
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    output_manifest: None,
                    error: Some(format!("failed to execute rustc: {}", e)),
                };
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            if let Err(e) = tokio::fs::remove_dir_all(&scratch_dir).await {
                warn!("failed to remove scratch directory after rustc compilation failure: {e}");
            }
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

        // 6. Ingest output to CAS
        let output_data = match tokio::fs::read(&output_path).await {
            Ok(d) => d,
            Err(e) => {
                if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                    warn!(
                        "failed to remove scratch directory after output read failure: {cleanup_err}"
                    );
                }
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                    duration_ms,
                    output_manifest: None,
                    error: Some(format!("failed to read output: {}", e)),
                };
            }
        };

        let blob_hash = match self.cas.put_blob(output_data).await {
            Ok(h) => h,
            Err(e) => {
                if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                    warn!(
                        "failed to remove scratch directory after CAS blob storage failure: {cleanup_err}"
                    );
                }
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                    duration_ms,
                    output_manifest: None,
                    error: Some(format!("failed to store output in CAS: {:?}", e)),
                };
            }
        };

        // 7. Create and store output manifest
        let logical = if request.crate_type == "lib" {
            "rlib"
        } else {
            "bin"
        };

        let node_id = NodeId(format!(
            "compile-{}:{}:{}:{}",
            request.crate_type, request.crate_name, request.target_triple, request.profile
        ));

        // Compute a cache key from the request (for the manifest)
        let cache_key = vx_cache::rust_compile_cache_key(&request);

        let manifest = NodeManifest {
            node_id,
            cache_key,
            produced_at: Zoned::now().datetime(),
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
                if let Err(cleanup_err) = tokio::fs::remove_dir_all(&scratch_dir).await {
                    warn!(
                        "failed to remove scratch directory after CAS manifest storage failure: {cleanup_err}"
                    );
                }
                return RustCompileResult {
                    success: false,
                    exit_code: -1,
                    stdout,
                    stderr,
                    duration_ms,
                    output_manifest: None,
                    error: Some(format!("failed to store manifest in CAS: {:?}", e)),
                };
            }
        };

        // Cleanup scratch directory
        if let Err(e) = tokio::fs::remove_dir_all(&scratch_dir).await {
            warn!("failed to remove scratch directory after successful compilation: {e}");
        }

        info!(
            crate_name = %request.crate_name,
            manifest_hash = %manifest_hash,
            duration_ms,
            "compilation complete"
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

    async fn compile_cc(&self, _request: CcCompileRequest) -> CcCompileResult {
        let start = Instant::now();

        // TODO: Implement C/C++ compilation with the new API
        // For now, return an error indicating not implemented
        CcCompileResult {
            success: false,
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: start.elapsed().as_millis() as u64,
            output_manifest: None,
            discovered_deps: vec![],
            error: Some("C/C++ compilation not yet implemented with new API".to_string()),
        }
    }
}

impl RheaService {
    /// Create a scratch directory for compilation
    async fn create_scratch_dir(&self) -> RheaResult<Utf8PathBuf> {
        let scratch_base = self.toolchains_dir.parent().unwrap_or(&self.toolchains_dir);
        vx_io::create_scratch_dir(scratch_base)
            .await
            .map_err(|e| crate::RheaError::ScratchDir(e.to_string()))
    }
}

/// Parse a Cargo.toml to extract lib_path and edition.
///
/// This is used when compiling registry crates, where the request contains
/// placeholder values and we need to determine the actual values from the crate.
async fn parse_crate_info(crate_dir: &Utf8Path) -> Result<CrateInfo, String> {
    let cargo_toml_path = crate_dir.join("Cargo.toml");

    // Use blocking task to read and parse the file synchronously (facet_cargo_toml uses std::fs)
    let cargo_toml_path_clone = cargo_toml_path.clone();
    let manifest = tokio::task::spawn_blocking(move || {
        facet_cargo_toml::CargoToml::from_path(&cargo_toml_path_clone)
    })
    .await
    .map_err(|e| format!("task join error: {}", e))?
    .map_err(|e| format!("failed to parse {}: {}", cargo_toml_path, e))?;

    // Determine lib_path
    let lib_path = if let Some(ref lib) = manifest.lib {
        if let Some(ref path) = lib.path {
            Utf8PathBuf::from(path)
        } else {
            Utf8PathBuf::from("src/lib.rs")
        }
    } else {
        Utf8PathBuf::from("src/lib.rs")
    };

    // Determine edition
    let edition = if let Some(ref package) = manifest.package {
        match &package.edition {
            Some(facet_cargo_toml::EditionOrWorkspace::Edition(e)) => match e {
                facet_cargo_toml::Edition::E2015 => "2015".to_string(),
                facet_cargo_toml::Edition::E2018 => "2018".to_string(),
                facet_cargo_toml::Edition::E2021 => "2021".to_string(),
                facet_cargo_toml::Edition::E2024 => "2024".to_string(),
            },
            _ => "2021".to_string(), // Default
        }
    } else {
        "2021".to_string() // Default
    };

    Ok(CrateInfo { lib_path, edition })
}
