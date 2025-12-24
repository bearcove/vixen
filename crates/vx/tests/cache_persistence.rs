//! Cache persistence tests
//!
//! These tests verify that picante's incremental database persists correctly:
//! - Cache file is created and persisted
//! - Cache survives across sessions
//! - Memoization skips query recomputation

mod harness;
use harness::{TestEnv, create_hello_world};

#[test_log::test]
fn cache_persists_across_sessions() {
    // This test verifies that cache hits work across separate vx invocations
    let env = TestEnv::new();
    create_hello_world(&env);

    // First build
    let result1 = env.build(false);
    assert!(result1.success, "first build failed");

    // Clean project-local .vx/ but keep global CAS
    // Note: clean kills the daemon, so we need to respawn it
    env.clean();

    // Respawn daemon after clean
    let _daemon = TestEnv::spawn_daemon(env.vx_home_path());

    // Second build — should still be a cache hit from global CAS
    let result2 = env.build(false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "build should hit global CAS cache");
}

#[test_log::test]
fn picante_cache_is_persisted() {
    // This test verifies that picante's incremental database is persisted to disk.
    // Picante uses a WAL (Write-Ahead Log) pattern:
    // - During operation: writes go to picante.wal
    // - On shutdown: WAL is compacted to picante.cache
    // The WAL is created when the daemon starts and grows during builds.

    let env = TestEnv::new();
    let shared_home = tempfile::TempDir::new().unwrap();
    let _shared_daemon = TestEnv::spawn_daemon(shared_home.path());
    let picante_wal_path = shared_home.path().join("picante.wal");

    create_hello_world(&env);

    // Get initial WAL size (daemon creates empty/minimal WAL on startup)
    let initial_size = std::fs::metadata(&picante_wal_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // First build
    let result1 = env.build_with_home(shared_home.path(), false);
    assert!(result1.success, "first build failed");

    // After build: picante WAL should exist and have grown
    assert!(
        picante_wal_path.exists(),
        "picante.wal should exist after build"
    );

    // Get the WAL file size after build
    let size1 = std::fs::metadata(&picante_wal_path).unwrap().len();

    // The WAL should have grown after the build
    assert!(
        size1 > initial_size,
        "picante.wal should grow after build: initial {} bytes, after {} bytes",
        initial_size,
        size1
    );

    // Second build with same inputs — should still hit cache
    let result2 = env.build_with_home(shared_home.path(), false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");

    // WAL file should still exist
    assert!(
        picante_wal_path.exists(),
        "picante.wal should still exist after second build"
    );
}

#[test_log::test]
fn picante_memoization_skips_query_recomputation() {
    // This test verifies that picante's memoization is actually working:
    // - First build: queries are computed
    // - Second build: queries are memoized (cache hit)
    //
    // We verify by checking that the second build is a cache hit and
    // that the WAL file exists (indicating picante is persisting state).

    let env = TestEnv::new();
    let shared_home = tempfile::TempDir::new().unwrap();
    let _shared_daemon = TestEnv::spawn_daemon(shared_home.path());
    let wal_path = shared_home.path().join("picante.wal");

    create_hello_world(&env);

    // First build - computes queries, writes to WAL
    let result1 = env.build_with_home(shared_home.path(), false);
    assert!(result1.success, "first build failed");
    assert!(!result1.was_cached(), "first build should not be cached");

    // WAL should exist after first build
    assert!(
        wal_path.exists(),
        "picante WAL should exist after first build"
    );

    // Second build - should use memoized queries (cache hit)
    let result2 = env.build_with_home(shared_home.path(), false);
    assert!(result2.success, "second build failed");
    assert!(result2.was_cached(), "second build should be cached");

    // WAL should still exist
    assert!(
        wal_path.exists(),
        "picante WAL should still exist after second build"
    );
}
