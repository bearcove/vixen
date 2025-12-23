//! Build script (build.rs) execution support
//!
//! This module handles compiling and running build scripts for crates,
//! parsing their output according to the Cargo build script protocol.
//!
//! ## Protocol
//!
//! Build scripts communicate via stdout with lines like:
//! - `cargo:rustc-cfg=FEATURE` - Add a `--cfg FEATURE` flag
//! - `cargo:rustc-env=NAME=VALUE` - Set an environment variable
//! - `cargo:rustc-link-lib=LIB` - Link a library
//! - `cargo:rustc-link-search=PATH` - Add library search path
//! - `cargo:rerun-if-changed=PATH` - Rebuild if path changes (ignored for now)
//! - `cargo:rerun-if-env-changed=VAR` - Rebuild if env var changes (ignored for now)
//! - `cargo:warning=MESSAGE` - Print a warning (we'll log it)

use std::collections::HashMap;
use std::process::Command;

use camino::Utf8Path;
use miette::Diagnostic;
use thiserror::Error;

use crate::crate_graph::BuildScriptOutput;

/// Errors that can occur when running build scripts
#[derive(Debug, Error, Diagnostic)]
pub enum BuildScriptError {
    #[error("failed to compile build script for {crate_name}: {message}")]
    #[diagnostic(code(vx_rs::build_script_compile_error))]
    CompileError { crate_name: String, message: String },

    #[error("failed to run build script for {crate_name}: {message}")]
    #[diagnostic(code(vx_rs::build_script_run_error))]
    RunError { crate_name: String, message: String },

    #[error("build script for {crate_name} exited with code {code}:\n{stderr}")]
    #[diagnostic(code(vx_rs::build_script_failed))]
    ExitError {
        crate_name: String,
        code: i32,
        stderr: String,
    },

    #[error("I/O error: {0}")]
    #[diagnostic(code(vx_rs::build_script_io_error))]
    IoError(#[from] std::io::Error),
}

/// Context for running a build script
pub struct BuildScriptContext<'a> {
    /// Path to rustc
    pub rustc: &'a Utf8Path,
    /// Workspace root (absolute path)
    pub workspace_root: &'a Utf8Path,
    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target: &'a str,
    /// Host triple
    pub host: &'a str,
    /// Output directory for build script artifacts
    pub out_dir: &'a Utf8Path,
    /// Extra environment variables to set
    pub env: HashMap<String, String>,
}

/// Run a build script and return its output
///
/// This compiles `build.rs` to a binary, runs it with the appropriate
/// environment variables, and parses its stdout for cargo: directives.
pub fn run_build_script(
    ctx: &BuildScriptContext<'_>,
    crate_name: &str,
    build_script_path: &Utf8Path,
    manifest_dir: &Utf8Path,
) -> Result<BuildScriptOutput, BuildScriptError> {
    // Ensure out_dir exists
    std::fs::create_dir_all(ctx.out_dir)?;

    // Compile build.rs to a binary
    let build_script_bin = ctx.out_dir.join("build_script");
    let compile_status = Command::new(ctx.rustc.as_str())
        .arg(build_script_path.as_str())
        .arg("--crate-type=bin")
        .arg("--edition=2021") // Build scripts typically use modern edition
        .arg("-o")
        .arg(build_script_bin.as_str())
        .output()?;

    if !compile_status.status.success() {
        return Err(BuildScriptError::CompileError {
            crate_name: crate_name.to_string(),
            message: String::from_utf8_lossy(&compile_status.stderr).to_string(),
        });
    }

    // Run the build script with required environment variables
    let output = Command::new(build_script_bin.as_str())
        .env("CARGO_MANIFEST_DIR", manifest_dir.as_str())
        .env("OUT_DIR", ctx.out_dir.as_str())
        .env("TARGET", ctx.target)
        .env("HOST", ctx.host)
        .env("RUSTC", ctx.rustc.as_str())
        .env("CARGO_PKG_NAME", crate_name)
        // Add common cargo env vars that build scripts expect
        .env("CARGO_PKG_VERSION", "0.0.0") // TODO: get from manifest
        .env("CARGO_PKG_VERSION_MAJOR", "0")
        .env("CARGO_PKG_VERSION_MINOR", "0")
        .env("CARGO_PKG_VERSION_PATCH", "0")
        .env("PROFILE", "release") // vx always builds in release-like mode
        .env("DEBUG", "false")
        .env("OPT_LEVEL", "3")
        .envs(ctx.env.iter())
        .current_dir(manifest_dir)
        .output()?;

    if !output.status.success() {
        return Err(BuildScriptError::ExitError {
            crate_name: crate_name.to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    // Parse the output
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_build_script_output(&stdout))
}

/// Parse build script stdout for cargo: directives
pub fn parse_build_script_output(stdout: &str) -> BuildScriptOutput {
    let mut output = BuildScriptOutput::default();

    for line in stdout.lines() {
        if let Some(directive) = line.strip_prefix("cargo:") {
            if let Some(cfg) = directive.strip_prefix("rustc-cfg=") {
                output.cfgs.push(cfg.to_string());
            } else if let Some(env) = directive.strip_prefix("rustc-env=") {
                if let Some((name, value)) = env.split_once('=') {
                    output.envs.push((name.to_string(), value.to_string()));
                }
            } else if let Some(lib) = directive.strip_prefix("rustc-link-lib=") {
                output.link_libs.push(lib.to_string());
            } else if let Some(search) = directive.strip_prefix("rustc-link-search=") {
                output.link_search.push(search.to_string());
            } else if let Some(warning) = directive.strip_prefix("warning=") {
                // Log warnings but don't fail
                tracing::warn!(crate_build_script = true, "{}", warning);
            }
            // Ignore rerun-if-changed, rerun-if-env-changed, and other directives for now
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_output() {
        let output = parse_build_script_output("");
        assert!(output.cfgs.is_empty());
        assert!(output.envs.is_empty());
    }

    #[test]
    fn parse_cfg_flags() {
        let stdout = r#"
cargo:rerun-if-changed=build.rs
cargo:rustc-cfg=feature_a
cargo:rustc-cfg=feature_b
cargo:rustc-check-cfg=cfg(feature_a)
"#;
        let output = parse_build_script_output(stdout);
        assert_eq!(output.cfgs, vec!["feature_a", "feature_b"]);
    }

    #[test]
    fn parse_env_vars() {
        let stdout = "cargo:rustc-env=FOO=bar\ncargo:rustc-env=BAZ=qux";
        let output = parse_build_script_output(stdout);
        assert_eq!(
            output.envs,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string())
            ]
        );
    }

    #[test]
    fn parse_link_directives() {
        let stdout = "cargo:rustc-link-lib=ssl\ncargo:rustc-link-search=/usr/lib";
        let output = parse_build_script_output(stdout);
        assert_eq!(output.link_libs, vec!["ssl"]);
        assert_eq!(output.link_search, vec!["/usr/lib"]);
    }
}
