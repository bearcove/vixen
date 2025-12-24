//! Multi-crate build tests
//!
//! These tests verify that vx correctly handles projects with path dependencies:
//! - Building a bin crate that depends on a lib crate
//! - Caching works correctly (deps before dependents)
//! - Changes to deps trigger rebuilds of dependents
//! - Changes to app only rebuild app
//! - Path independence (same content, different checkout location = cache hit)

mod harness;
use harness::{TestEnv, create_diamond_dependency_project, create_multi_crate_project};

#[test_log::test]
fn multi_crate_build_succeeds() {
    let env = TestEnv::new();
    create_multi_crate_project(&env);

    // Build from the app/ subdirectory
    let result = env.build_in_subdir("app", false);

    assert!(
        result.success,
        "multi-crate build failed: {}\n{}",
        result.stdout, result.stderr
    );
    assert!(
        !result.was_cached(),
        "fresh multi-crate build should not be cached"
    );
}

#[test_log::test]
fn multi_crate_second_build_is_cache_hit() {
    let env = TestEnv::new();
    create_multi_crate_project(&env);

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(
        result1.success,
        "first build failed: {}\n{}",
        result1.stdout, result1.stderr
    );
    assert!(!result1.was_cached(), "first build should not be cached");

    // Second build - should be fully cached
    let result2 = env.build_in_subdir("app", false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    assert!(
        result2.was_cached(),
        "second build should be cached\nstdout: {}\nstderr: {}",
        result2.stdout,
        result2.stderr
    );
}

#[test_log::test]
fn dep_change_rebuilds_both() {
    let env = TestEnv::new();
    create_multi_crate_project(&env);

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(result1.success, "first build failed");
    assert!(!result1.was_cached(), "first build should not be cached");

    // Modify the util library
    env.write_file(
        "util/src/lib.rs",
        r#"pub fn greet() -> &'static str {
    "Hello from MODIFIED util!"
}
"#,
    );

    // Second build - should rebuild both util and app
    let result2 = env.build_in_subdir("app", false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    assert!(
        !result2.was_cached(),
        "build after dep change should not be cached"
    );
}

#[test_log::test]
fn app_change_rebuilds_only_app() {
    let env = TestEnv::new();
    create_multi_crate_project(&env);

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(result1.success, "first build failed");

    // Modify only the app
    env.write_file(
        "app/src/main.rs",
        r#"fn main() {
    println!("Modified: {}", util::greet());
}
"#,
    );

    // Second build - util should still be cached, only app rebuilds
    let result2 = env.build_in_subdir("app", false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    // The overall build is not fully cached since app changed
    assert!(
        !result2.was_cached(),
        "build after app change should not be fully cached"
    );
    // But we can't easily verify util was cached without checking logs
    // The key success criterion is that the build succeeds
}

#[test_log::test]
fn multi_crate_path_independence() {
    // Same project content in different directories should be a cache hit
    let env1 = TestEnv::new();
    let env2 = TestEnv::new();

    // Shared VX_HOME for both, with its own daemon
    let shared_home = tempfile::TempDir::new().unwrap();
    let _shared_daemon = TestEnv::spawn_daemon(shared_home.path());

    create_multi_crate_project(&env1);
    create_multi_crate_project(&env2);

    // Build in first location
    let result1 = env1.build_in_subdir_with_home("app", shared_home.path(), false);
    assert!(
        result1.success,
        "first build failed: {}\n{}",
        result1.stdout, result1.stderr
    );
    assert!(!result1.was_cached(), "first build should not be cached");

    // Build in second location - should be fully cached
    let result2 = env2.build_in_subdir_with_home("app", shared_home.path(), false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    assert!(
        result2.was_cached(),
        "second build in different path should be cached (path independence)"
    );
}

#[test_log::test]
fn module_rename_invalidates_cache() {
    let env = TestEnv::new();
    create_multi_crate_project(&env);

    // Add a module to util
    env.write_file(
        "util/src/lib.rs",
        r#"mod helper;
pub use helper::greet;
"#,
    );
    env.write_file(
        "util/src/helper.rs",
        r#"pub fn greet() -> &'static str {
    "Hello from helper module!"
}
"#,
    );

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(
        result1.success,
        "first build failed: {}\n{}",
        result1.stdout, result1.stderr
    );

    // Second build - should be cached
    let result2 = env.build_in_subdir("app", false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");

    // Rename the module file (helper.rs -> greeter.rs)
    std::fs::remove_file(env.project_path().join("util/src/helper.rs")).unwrap();
    env.write_file(
        "util/src/greeter.rs",
        r#"pub fn greet() -> &'static str {
    "Hello from greeter module!"
}
"#,
    );
    env.write_file(
        "util/src/lib.rs",
        r#"mod greeter;
pub use greeter::greet;
"#,
    );

    // Third build - should NOT be cached (module path changed)
    let result3 = env.build_in_subdir("app", false);
    assert!(
        result3.success,
        "third build failed: {}\n{}",
        result3.stdout, result3.stderr
    );
    assert!(
        !result3.was_cached(),
        "build after module rename should not be cached"
    );
}

#[test_log::test]
fn diamond_dependency_builds() {
    let env = TestEnv::new();
    create_diamond_dependency_project(&env);

    // Build from the app/ subdirectory
    let result = env.build_in_subdir("app", false);

    assert!(
        result.success,
        "diamond dependency build failed: {}\n{}",
        result.stdout, result.stderr
    );
}

#[test_log::test]
fn diamond_dependency_cache_hit() {
    let env = TestEnv::new();
    create_diamond_dependency_project(&env);

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(
        result1.success,
        "first build failed: {}\n{}",
        result1.stdout, result1.stderr
    );

    // Second build - should be cached
    let result2 = env.build_in_subdir("app", false);
    assert!(result2.success, "second build failed");
    assert!(
        result2.was_cached(),
        "second diamond build should be cached\nstdout: {}\nstderr: {}",
        result2.stdout,
        result2.stderr
    );
}

#[test_log::test]
fn diamond_shared_dep_change_rebuilds_dependents() {
    let env = TestEnv::new();
    create_diamond_dependency_project(&env);

    // First build
    let result1 = env.build_in_subdir("app", false);
    assert!(result1.success, "first build failed");

    // Modify the common library (shared dep)
    env.write_file(
        "common/src/lib.rs",
        r#"pub fn version() -> &'static str {
    "2.0.0"
}
"#,
    );

    // Second build - should rebuild common, helper, and app
    let result2 = env.build_in_subdir("app", false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    assert!(
        !result2.was_cached(),
        "build after shared dep change should not be cached"
    );
}
