//! Error handling tests
//!
//! These tests verify that vx fails loudly and clearly on unsupported features.

mod harness;
use harness::{
    TestEnv, create_project_with_build_script, create_project_with_deps, create_workspace,
};

#[test]
fn rejects_dependencies() {
    let env = TestEnv::new();
    create_project_with_deps(&env);

    let result = env.build(false);

    assert!(!result.success, "build should fail with dependencies");
    assert!(
        result.contains("dependencies") || result.contains("not supported"),
        "error should mention dependencies: {}\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn rejects_workspace() {
    let env = TestEnv::new();
    create_workspace(&env);

    let result = env.build(false);

    assert!(!result.success, "build should fail for workspace");
    assert!(
        result.contains("workspace") || result.contains("not supported"),
        "error should mention workspace: {}\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn rejects_build_script() {
    let env = TestEnv::new();
    create_project_with_build_script(&env);

    let result = env.build(false);

    assert!(!result.success, "build should fail with build script");
    assert!(
        result.contains("build") || result.contains("not supported"),
        "error should mention build script: {}\n{}",
        result.stdout,
        result.stderr
    );
}

#[test]
#[ignore = "not yet implemented"]
fn rejects_features() {
    let env = TestEnv::new();
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "withfeatures"
version = "0.1.0"
edition = "2021"

[features]
default = []
foo = []
"#,
    );
    env.write_file("src/main.rs", "fn main() {}\n");

    let result = env.build(false);

    assert!(!result.success, "build should fail with features");
    assert!(
        result.contains("features") || result.contains("not supported"),
        "error should mention features"
    );
}

#[test]
#[ignore = "not yet implemented"]
fn rejects_proc_macros() {
    let env = TestEnv::new();
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "procmacro"
version = "0.1.0"
edition = "2021"

[lib]
proc-macro = true
"#,
    );
    env.write_file("src/lib.rs", "");

    let result = env.build(false);

    assert!(!result.success, "build should fail with proc-macro");
}

#[test]
#[ignore = "not yet implemented - requires mod parsing"]
fn rejects_multi_file_crates() {
    let env = TestEnv::new();
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "multifile"
version = "0.1.0"
edition = "2021"
"#,
    );
    env.write_file(
        "src/main.rs",
        r#"mod other;
fn main() { other::hello(); }
"#,
    );
    env.write_file(
        "src/other.rs",
        r#"pub fn hello() { println!("hello"); }
"#,
    );

    let result = env.build(false);

    assert!(!result.success, "build should fail with multi-file crate");
    assert!(
        result.contains("mod") || result.contains("single-file"),
        "error should mention single-file restriction"
    );
}
