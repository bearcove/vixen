//! Basic cache correctness tests
//!
//! These tests verify fundamental caching behavior:
//! - Fresh builds work
//! - Second build is a cache hit
//! - Source/profile/edition changes cause cache miss

mod harness;
use harness::{TestEnv, create_hello_world};

#[test]
fn fresh_build_succeeds() {
    let env = TestEnv::new();
    create_hello_world(&env);

    let result = env.build(false);

    assert!(
        result.success,
        "build failed: {}\n{}",
        result.stdout, result.stderr
    );
    assert!(!result.was_cached(), "fresh build should not be cached");
}

#[test]
fn second_build_is_cache_hit() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build
    let result1 = env.build(false);
    assert!(result1.success, "first build failed");
    assert!(!result1.was_cached(), "first build should not be cached");

    // Second build — should be cached
    let result2 = env.build(false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");
}

#[test]
fn source_change_causes_cache_miss() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build
    let result1 = env.build(false);
    assert!(result1.success, "first build failed");

    // Modify source
    env.write_file(
        "src/main.rs",
        r#"fn main() {
    println!("Hello, modified world!");
}
"#,
    );

    // Second build — should NOT be cached
    let result2 = env.build(false);
    assert!(result2.success, "second build failed");
    assert!(
        !result2.was_cached(),
        "build after source change should not be cached"
    );
}

#[test]
fn profile_change_causes_cache_miss() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // Debug build
    let result1 = env.build(false);
    assert!(result1.success, "debug build failed");
    assert!(!result1.was_cached(), "first build should not be cached");

    // Release build — should NOT be cached (different profile)
    let result2 = env.build(true);
    assert!(result2.success, "release build failed");
    assert!(
        !result2.was_cached(),
        "release build should not be cached after debug"
    );

    // Debug build again — should still be cached from first build
    let result3 = env.build(false);
    assert!(result3.success, "third build failed");
    assert!(result3.was_cached(), "debug build should be cached");
}

#[test]
fn edition_change_causes_cache_miss() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // Build with 2021 edition
    let result1 = env.build(false);
    assert!(result1.success, "first build failed");

    // Change edition to 2018
    env.write_file(
        "Cargo.toml",
        r#"[package]
name = "hello"
version = "0.1.0"
edition = "2018"
"#,
    );

    // Second build — should NOT be cached
    let result2 = env.build(false);
    assert!(result2.success, "second build failed");
    assert!(
        !result2.was_cached(),
        "build after edition change should not be cached"
    );
}

#[test]
fn clean_removes_project_local_vx_dir() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // Build first
    let result = env.build(false);
    assert!(result.success, "build failed");

    // Check .vx/build exists
    assert!(
        env.file_exists(".vx/build"),
        ".vx/build should exist after build"
    );

    // Clean
    let clean_result = env.clean();
    assert!(clean_result.success, "clean failed");

    // Check .vx is gone
    assert!(!env.file_exists(".vx"), ".vx should not exist after clean");
}

#[test]
fn different_checkout_path_is_cache_hit() {
    // This test verifies that the same project checked out in different locations
    // produces a cache hit (path normalization via --remap-path-prefix)

    // Create two separate project directories with identical content
    let env1 = TestEnv::new();
    let env2 = TestEnv::new();

    // Use a shared VX_HOME for both
    let shared_home = tempfile::TempDir::new().unwrap();

    create_hello_world(&env1);
    create_hello_world(&env2);

    // Build in first location
    let result1 = env1.build_with_home(shared_home.path(), false);
    assert!(
        result1.success,
        "first build failed: {}\n{}",
        result1.stdout, result1.stderr
    );
    assert!(!result1.was_cached(), "first build should not be cached");

    // Build in second location — should be cache hit
    // because content is identical and paths are remapped
    let result2 = env2.build_with_home(shared_home.path(), false);
    assert!(
        result2.success,
        "second build failed: {}\n{}",
        result2.stdout, result2.stderr
    );
    assert!(
        result2.was_cached(),
        "second build should be cached (same content, different path)"
    );
}
