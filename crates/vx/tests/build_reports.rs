//! Build report tests
//!
//! These tests verify that vx build reports work correctly:
//! - Reports are written after builds
//! - vx explain shows correct summary
//! - vx explain --diff detects changes
//! - vx explain --last-miss shows first miss

mod harness;
use harness::{TestEnv, create_hello_world};

#[test_log::test]
fn build_writes_report_file() {
    let env = TestEnv::new();
    create_hello_world(&env);

    let result = env.build(false);
    assert!(result.success, "build failed: {}", result.stderr);

    // Check that .vx/runs/ directory exists with a report
    assert!(
        env.file_exists(".vx/runs/latest"),
        "latest run symlink should exist after build"
    );
}

#[test_log::test]
fn explain_shows_build_summary() {
    let env = TestEnv::new();
    create_hello_world(&env);

    let build = env.build(false);
    assert!(build.success, "build failed");

    let explain = env.explain();
    assert!(explain.success, "explain failed: {}", explain.stderr);

    // Should show SUCCESS
    assert!(
        explain.stdout.contains("SUCCESS"),
        "explain should show SUCCESS for successful build: {}",
        explain.stdout
    );

    // Should show cache miss (first build) - either "miss" or "first build"
    assert!(
        explain.stdout.contains("miss") || explain.stdout.contains("first build"),
        "explain should show cache miss for first build: {}",
        explain.stdout
    );

    // Should show the node
    assert!(
        explain.stdout.contains("compile-bin"),
        "explain should show compile-bin node: {}",
        explain.stdout
    );
}

#[test_log::test]
fn explain_shows_cache_hit_after_rebuild() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build (miss)
    let build1 = env.build(false);
    assert!(build1.success, "first build failed: {}", build1.stderr);

    // Second build (hit)
    let build2 = env.build(false);
    assert!(build2.success, "second build failed: {}", build2.stderr);

    let explain = env.explain();
    assert!(explain.success, "explain failed");

    // Should show cache hit
    assert!(
        explain.stdout.contains("1 hit") || explain.stdout.contains("hit"),
        "explain should show cache hit for second build: {}",
        explain.stdout
    );
}

#[test_log::test]
fn explain_diff_detects_source_change() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build - cache hit (if previous cache exists) or miss
    let build1 = env.build(false);
    assert!(build1.success, "first build failed: {}", build1.stderr);

    // Second build - cache hit
    let build2 = env.build(false);
    assert!(build2.success, "second build failed: {}", build2.stderr);
    assert!(build2.was_cached(), "second build should be cached");

    // Modify source
    env.write_file(
        "src/main.rs",
        r#"fn main() {
    println!("Modified source!");
}
"#,
    );

    // Third build - should be cache miss
    let build3 = env.build(false);
    assert!(build3.success, "third build failed");
    assert!(!build3.was_cached(), "third build should not be cached");

    // Explain diff should show the change
    let diff = env.explain_diff();
    assert!(diff.success, "explain --diff failed: {}", diff.stderr);

    // Should show hit → miss flip
    assert!(
        diff.stdout.contains("hit") && diff.stdout.contains("miss"),
        "explain --diff should show cache flip: {}",
        diff.stdout
    );

    // Should mention source changed (either specific file or source_closure)
    assert!(
        diff.stdout.contains("src/main.rs")
            || diff.stdout.contains("main.rs")
            || diff.stdout.contains("source_closure"),
        "explain --diff should show source file changed: {}",
        diff.stdout
    );
}

#[test_log::test]
fn explain_last_miss_shows_inputs() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // Build (first build is always a miss)
    let build = env.build(false);
    assert!(build.success, "build failed");

    // Check last miss
    let last_miss = env.explain_last_miss();
    assert!(
        last_miss.success,
        "explain --last-miss failed: {}",
        last_miss.stderr
    );

    // Should show the node
    assert!(
        last_miss.stdout.contains("compile-bin"),
        "explain --last-miss should show compile-bin node: {}",
        last_miss.stdout
    );

    // Should show inputs
    assert!(
        last_miss.stdout.contains("Inputs"),
        "explain --last-miss should show Inputs section: {}",
        last_miss.stdout
    );

    // Should mention source (either specific file or source_closure)
    assert!(
        last_miss.stdout.contains("main.rs")
            || last_miss.stdout.contains("src/")
            || last_miss.stdout.contains("source_closure"),
        "explain --last-miss should show source file: {}",
        last_miss.stdout
    );
}

#[test_log::test]
fn explain_with_no_builds_shows_error() {
    let env = TestEnv::new();
    create_hello_world(&env);

    // Don't build, just try to explain
    let explain = env.explain();

    // Should indicate no builds found
    assert!(
        explain.stdout.contains("No build reports") || explain.stdout.contains("no build"),
        "explain without builds should show error: {}",
        explain.stdout
    );
}
