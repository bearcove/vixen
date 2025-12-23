//! Error handling tests
//!
//! These tests verify that vx fails loudly and clearly on unsupported features.

mod harness;
use harness::{
    TestEnv, create_project_with_build_script, create_project_with_deps, create_workspace,
};

#[test_log::test]
fn rejects_registry_deps_without_lockfile() {
    let env = TestEnv::new();
    create_project_with_deps(&env);

    // No Cargo.lock file - should fail with a clear error message
    let result = env.build(false);

    assert!(
        !result.success,
        "build should fail with registry deps but no Cargo.lock"
    );
    assert!(
        result.contains("Cargo.lock") || result.contains("lockfile"),
        "error should mention Cargo.lock requirement: {}\n{}",
        result.stdout,
        result.stderr
    );
}

#[test_log::test]
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

#[test_log::test]
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

#[test_log::test]
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

#[test_log::test]
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
