//! Test harness for vx integration tests
//!
//! Provides utilities for creating isolated test environments with temp directories
//! for both the global CAS (~/.vx/) and project-local build outputs (.vx/).
//!
//! Each test uses Unix sockets in its unique VX_HOME directory, avoiding any port
//! conflicts when tests run in parallel.

// Each integration test file compiles this module separately, so functions
// used by one test file appear "unused" when compiling another.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// A test environment with isolated CAS, project directories, and Unix sockets
pub struct TestEnv {
    /// Temp dir for global CAS (simulates ~/.vx/)
    pub vx_home: TempDir,
    /// Temp dir for the test project
    pub project: TempDir,
}

impl TestEnv {
    /// Create a new isolated test environment
    ///
    /// Each test gets its own unique VX_HOME with Unix sockets for IPC.
    /// No ports are used, so tests can run in parallel without conflicts.
    pub fn new() -> Self {
        Self {
            vx_home: TempDir::new().expect("failed to create vx_home temp dir"),
            project: TempDir::new().expect("failed to create project temp dir"),
        }
    }

    /// Path to the vx binary (built by cargo)
    fn vx_binary() -> PathBuf {
        // Find the binary in target/debug or target/release
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // vx/
        path.push("target");
        path.push("debug");
        path.push("vx");
        path
    }

    /// Set up common environment variables for all vx commands
    ///
    /// Uses Unix sockets in VX_HOME for all services (no TCP ports needed).
    fn setup_env(&self, cmd: &mut Command, vx_home: &Path) {
        // Set VX_HOME - services will default to Unix sockets in this directory
        cmd.env("VX_HOME", vx_home);
        // Don't set VX_AETHER/VX_CASS/VX_RHEA - let them default to Unix sockets
    }

    /// Run `vx build` in this environment
    pub fn build(&self, release: bool) -> VxOutput {
        self.build_with_home(self.vx_home.path(), release)
    }

    /// Run `vx build` from a subdirectory within the project
    pub fn build_in_subdir(&self, subdir: &str, release: bool) -> VxOutput {
        self.build_in_subdir_with_home(subdir, self.vx_home.path(), release)
    }

    /// Run `vx build` from a subdirectory with a custom VX_HOME
    pub fn build_in_subdir_with_home(
        &self,
        subdir: &str,
        vx_home: &Path,
        release: bool,
    ) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path().join(subdir));
        self.setup_env(&mut cmd, vx_home);
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Run `vx build` with a custom VX_HOME (for testing shared cache)
    pub fn build_with_home(&self, vx_home: &Path, release: bool) -> VxOutput {
        self.build_with_home_and_env(vx_home, release, &[])
    }

    /// Run `vx build` with a custom VX_HOME and additional environment variables
    pub fn build_with_home_and_env(
        &self,
        vx_home: &Path,
        release: bool,
        env_vars: &[(&str, &str)],
    ) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        self.setup_env(&mut cmd, vx_home);
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        cmd.arg("build");
        if release {
            cmd.arg("--release");
        }

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Run `vx clean` in this environment
    pub fn clean(&self) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        self.setup_env(&mut cmd, self.vx_home.path());
        cmd.arg("clean");

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Run `vx explain` in this environment
    pub fn explain(&self) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        self.setup_env(&mut cmd, self.vx_home.path());
        cmd.arg("explain");

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Run `vx explain --diff` in this environment
    pub fn explain_diff(&self) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        self.setup_env(&mut cmd, self.vx_home.path());
        cmd.args(["explain", "--diff"]);

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Run `vx explain --last-miss` in this environment
    pub fn explain_last_miss(&self) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        self.setup_env(&mut cmd, self.vx_home.path());
        cmd.args(["explain", "--last-miss"]);

        let output = cmd.output().expect("failed to run vx");
        VxOutput::from(output)
    }

    /// Write a file to the project directory
    pub fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.project.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create parent dirs");
        }
        std::fs::write(&path, contents).expect("failed to write file");
    }

    /// Read a file from the project directory
    pub fn read_file(&self, relative_path: &str) -> String {
        let path = self.project.path().join(relative_path);
        std::fs::read_to_string(&path).expect("failed to read file")
    }

    /// Check if a file exists in the project directory
    pub fn file_exists(&self, relative_path: &str) -> bool {
        self.project.path().join(relative_path).exists()
    }

    /// Check if a file exists in the CAS directory
    pub fn cas_file_exists(&self, relative_path: &str) -> bool {
        self.vx_home.path().join(relative_path).exists()
    }

    /// Get the project path
    pub fn project_path(&self) -> &Path {
        self.project.path()
    }

    /// Get the VX_HOME path
    pub fn vx_home_path(&self) -> &Path {
        self.vx_home.path()
    }
}

/// Output from running vx
#[derive(Debug)]
pub struct VxOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl VxOutput {
    /// Check if the build was a cache hit
    pub fn was_cached(&self) -> bool {
        self.stdout.contains("(cached)")
    }

    /// Check if output contains a specific string
    pub fn contains(&self, s: &str) -> bool {
        self.stdout.contains(s) || self.stderr.contains(s)
    }
}

impl From<Output> for VxOutput {
    fn from(output: Output) -> Self {
        Self {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Create a minimal hello world project
pub fn create_hello_world(env: &TestEnv) {
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "hello"
version = "0.1.0"
edition = "2021"
"#,
    );

    env.write_file(
        "src/main.rs",
        r#"fn main() {
    println!("Hello, world!");
}
"#,
    );
}

/// Create a project with dependencies (should fail)
pub fn create_project_with_deps(env: &TestEnv) {
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "withdeps"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
"#,
    );

    env.write_file("src/main.rs", "fn main() {}\n");
}

/// Create a workspace project (should fail)
pub fn create_workspace(env: &TestEnv) {
    env.write_file(
        "Cargo.toml",
        r#"[workspace]
members = ["crates/*"]
"#,
    );
}

/// Create a project with build script (should fail)
pub fn create_project_with_build_script(env: &TestEnv) {
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "withbuild"
version = "0.1.0"
edition = "2021"
build = "build.rs"
"#,
    );

    env.write_file("src/main.rs", "fn main() {}\n");
    env.write_file("build.rs", "fn main() {}\n");
}

/// Create a multi-crate project with app (bin) depending on util (lib)
///
/// Layout:
/// ```
/// project/
/// ├── app/
/// │   ├── Cargo.toml    # [dependencies] util = { path = "../util" }
/// │   └── src/main.rs   # use util::greet;
/// └── util/
///     ├── Cargo.toml
///     └── src/lib.rs    # pub fn greet() -> &'static str
/// ```
pub fn create_multi_crate_project(env: &TestEnv) {
    // util library crate
    env.write_file(
        "util/Cargo.toml",
        r#"[package]
name = "util"
version = "0.1.0"
edition = "2021"
"#,
    );
    env.write_file(
        "util/src/lib.rs",
        r#"pub fn greet() -> &'static str {
    "Hello from util!"
}
"#,
    );

    // app binary crate that depends on util
    env.write_file(
        "app/Cargo.toml",
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
util = { path = "../util" }
"#,
    );
    env.write_file(
        "app/src/main.rs",
        r#"fn main() {
    println!("{}", util::greet());
}
"#,
    );
}

/// Create a diamond dependency project: app -> {common, helper}, helper -> common
///
/// Layout:
/// ```
/// project/
/// ├── app/         # bin, depends on common and helper
/// ├── helper/      # lib, depends on common
/// └── common/      # lib, no deps
/// ```
pub fn create_diamond_dependency_project(env: &TestEnv) {
    // common library (no deps)
    env.write_file(
        "common/Cargo.toml",
        r#"[package]
name = "common"
version = "0.1.0"
edition = "2021"
"#,
    );
    env.write_file(
        "common/src/lib.rs",
        r#"pub fn version() -> &'static str {
    "1.0.0"
}
"#,
    );

    // helper library (depends on common)
    env.write_file(
        "helper/Cargo.toml",
        r#"[package]
name = "helper"
version = "0.1.0"
edition = "2021"

[dependencies]
common = { path = "../common" }
"#,
    );
    env.write_file(
        "helper/src/lib.rs",
        r#"pub fn describe() -> String {
    format!("Helper v{}", common::version())
}
"#,
    );

    // app binary (depends on both common and helper)
    env.write_file(
        "app/Cargo.toml",
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
common = { path = "../common" }
helper = { path = "../helper" }
"#,
    );
    env.write_file(
        "app/src/main.rs",
        r#"fn main() {
    println!("Version: {}", common::version());
    println!("{}", helper::describe());
}
"#,
    );
}
