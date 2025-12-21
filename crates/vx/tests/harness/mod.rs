//! Test harness for vx integration tests
//!
//! Provides utilities for creating isolated test environments with temp directories
//! for both the global CAS (~/.vx/) and project-local build outputs (.vx/).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// A test environment with isolated CAS and project directories
pub struct TestEnv {
    /// Temp dir for global CAS (simulates ~/.vx/)
    pub vx_home: TempDir,
    /// Temp dir for the test project
    pub project: TempDir,
}

impl TestEnv {
    /// Create a new isolated test environment
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

    /// Run `vx build` in this environment
    pub fn build(&self, release: bool) -> VxOutput {
        let mut cmd = Command::new(Self::vx_binary());
        cmd.current_dir(self.project.path());
        cmd.env("VX_HOME", self.vx_home.path());
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
        cmd.env("VX_HOME", self.vx_home.path());
        cmd.arg("clean");

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
