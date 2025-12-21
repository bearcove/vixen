//! Cache persistence tests
//!
//! These tests verify that picante's incremental database persists correctly:
//! - Cache file is created and persisted
//! - Cache survives across sessions
//! - Memoization skips query recomputation

mod harness;
use harness::{TestEnv, create_hello_world};

#[test]
fn cache_persists_across_sessions() {
    // This test verifies that cache hits work across separate vx invocations
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build
    let result1 = env.build(false);
    assert!(result1.success, "first build failed");

    // Clean project-local .vx/ but keep global CAS
    env.clean();

    // Second build — should still be a cache hit from global CAS
    let result2 = env.build(false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "build should hit global CAS cache");
}

#[test]
fn picante_cache_is_persisted() {
    // This test verifies that picante's incremental database is persisted to disk.
    // The picante.cache file should be created after a build and contain
    // memoized query results that can be loaded on subsequent runs.

    let env = TestEnv::new();
    let shared_home = tempfile::TempDir::new().unwrap();
    let picante_cache_path = shared_home.path().join("picante.cache");

    create_hello_world(&env);

    // Before build: no picante cache
    assert!(
        !picante_cache_path.exists(),
        "picante.cache should not exist before first build"
    );

    // First build
    let result1 = env.build_with_home(shared_home.path(), false);
    assert!(result1.success, "first build failed");

    // After build: picante cache should exist
    assert!(
        picante_cache_path.exists(),
        "picante.cache should exist after build"
    );

    // Get the cache file size/mtime for comparison
    let metadata1 = std::fs::metadata(&picante_cache_path).unwrap();
    let size1 = metadata1.len();

    // The cache should have non-trivial content (inputs + tracked queries)
    assert!(
        size1 > 100,
        "picante.cache should have meaningful content, got {} bytes",
        size1
    );

    // Second build with same inputs — cache should still exist
    // (and potentially be updated with same content)
    let result2 = env.build_with_home(shared_home.path(), false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");

    // Cache file should still exist
    assert!(
        picante_cache_path.exists(),
        "picante.cache should still exist after second build"
    );
}

#[test]
fn picante_memoization_skips_query_recomputation() {
    // This test verifies that picante's memoization is actually working:
    // - First build: queries are computed (we see "COMPUTING" in logs)
    // - Second build: queries are memoized (no "COMPUTING" messages)
    //
    // We enable RUST_LOG=vx_daemon=debug to capture the trace output.

    let env = TestEnv::new();
    let shared_home = tempfile::TempDir::new().unwrap();

    create_hello_world(&env);

    // First build with tracing enabled
    let result1 = env.build_with_home_and_env(
        shared_home.path(),
        false,
        &[("RUST_LOG", "vx_daemon=debug")],
    );
    assert!(result1.success, "first build failed");

    // First build should show "COMPUTING" messages (queries being computed)
    assert!(
        result1.stderr.contains("COMPUTING"),
        "first build should compute queries, stderr: {}",
        result1.stderr
    );

    // Second build with tracing enabled
    let result2 = env.build_with_home_and_env(
        shared_home.path(),
        false,
        &[("RUST_LOG", "vx_daemon=debug")],
    );
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");

    // Second build should NOT show "COMPUTING" messages (queries memoized)
    assert!(
        !result2.stderr.contains("COMPUTING"),
        "second build should use memoized queries, but found COMPUTING in stderr: {}",
        result2.stderr
    );

    // Should show that cache was loaded
    assert!(
        result2.stderr.contains("loaded picante cache"),
        "second build should load picante cache, stderr: {}",
        result2.stderr
    );
}
