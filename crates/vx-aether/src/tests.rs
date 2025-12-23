use super::*;

/// Create a fresh database for testing
fn test_db() -> Database {
    Database::new()
}

/// Set up common build configuration for C builds
fn setup_cc_config(db: &Database) {
    BuildConfig::set(
        db,
        "debug".to_string(),
        "x86_64-linux-musl".to_string(),
        "/test/workspace".to_string(),
    )
    .unwrap();

    ZigToolchainConfig::set(db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();
}

#[tokio::test]
async fn test_cache_key_cc_compile_deterministic() {
    let db = test_db();
    setup_cc_config(&db);

    let source_hash = Blake3Hash::from_bytes(b"int main() { return 0; }");

    // Create source file input
    let source = CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();

    // Compute cache key twice
    let key1 = cache_key_cc_compile(&db, source).await.unwrap();
    let key2 = cache_key_cc_compile(&db, source).await.unwrap();

    // Should be identical (deterministic)
    assert_eq!(key1, key2);
}

#[tokio::test]
async fn test_cache_key_cc_compile_changes_with_source() {
    let db = test_db();
    setup_cc_config(&db);

    // First source
    let source1 = CSourceFile::new(
        &db,
        "src/main.c".to_string(),
        Blake3Hash::from_bytes(b"int main() { return 0; }"),
    )
    .unwrap();

    let key1 = cache_key_cc_compile(&db, source1).await.unwrap();

    // Different source content
    let source2 = CSourceFile::new(
        &db,
        "src/main.c".to_string(),
        Blake3Hash::from_bytes(b"int main() { return 1; }"),
    )
    .unwrap();

    let key2 = cache_key_cc_compile(&db, source2).await.unwrap();

    // Keys should be different
    assert_ne!(key1, key2);
}

#[tokio::test]
async fn test_cache_key_cc_compile_changes_with_profile() {
    let db = test_db();

    ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

    let source_hash = Blake3Hash::from_bytes(b"int main() { return 0; }");

    // Debug profile
    BuildConfig::set(
        &db,
        "debug".to_string(),
        "x86_64-linux-musl".to_string(),
        "/test/workspace".to_string(),
    )
    .unwrap();

    let source_debug =
        CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();
    let key_debug = cache_key_cc_compile(&db, source_debug).await.unwrap();

    // Release profile
    BuildConfig::set(
        &db,
        "release".to_string(),
        "x86_64-linux-musl".to_string(),
        "/test/workspace".to_string(),
    )
    .unwrap();

    let source_release = CSourceFile::new(&db, "src/main.c".to_string(), source_hash).unwrap();
    let key_release = cache_key_cc_compile(&db, source_release).await.unwrap();

    // Keys should be different
    assert_ne!(key_debug, key_release);
}

#[tokio::test]
async fn test_plan_cc_compile_generates_correct_args() {
    let db = test_db();
    setup_cc_config(&db);

    let source = CSourceFile::new(
        &db,
        "src/main.c".to_string(),
        Blake3Hash::from_bytes(b"int main() { return 0; }"),
    )
    .unwrap();

    let invocation = plan_cc_compile(&db, source).await.unwrap();

    // Should have "cc" as first arg
    assert_eq!(invocation.args[0], "cc");

    // Should have -target
    assert!(invocation.args.contains(&"-target".to_string()));
    assert!(invocation.args.contains(&"x86_64-linux-musl".to_string()));

    // Should have -c for compile-only
    assert!(invocation.args.contains(&"-c".to_string()));

    // Should have source file
    assert!(invocation.args.contains(&"src/main.c".to_string()));

    // Should have -o with .o output
    assert!(invocation.args.contains(&"-o".to_string()));
    let o_idx = invocation.args.iter().position(|a| a == "-o").unwrap();
    assert!(invocation.args[o_idx + 1].ends_with(".o"));

    // Should have -MMD -MF for depfile generation
    assert!(invocation.args.contains(&"-MMD".to_string()));
    assert!(invocation.args.contains(&"-MF".to_string()));

    // Debug profile should have -O0 and -g
    assert!(invocation.args.contains(&"-O0".to_string()));
    assert!(invocation.args.contains(&"-g".to_string()));

    // Should expect object and depfile outputs
    assert_eq!(invocation.expected_outputs.len(), 2);
    assert!(
        invocation
            .expected_outputs
            .iter()
            .any(|o| o.logical == "obj")
    );
    assert!(
        invocation
            .expected_outputs
            .iter()
            .any(|o| o.logical == "depfile")
    );
}

#[tokio::test]
async fn test_plan_cc_compile_release_flags() {
    let db = test_db();

    ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

    // Release profile
    BuildConfig::set(
        &db,
        "release".to_string(),
        "x86_64-linux-musl".to_string(),
        "/test/workspace".to_string(),
    )
    .unwrap();

    let source = CSourceFile::new(
        &db,
        "src/main.c".to_string(),
        Blake3Hash::from_bytes(b"int main() { return 0; }"),
    )
    .unwrap();

    let invocation = plan_cc_compile(&db, source).await.unwrap();

    // Release should have -O2, not -O0
    assert!(invocation.args.contains(&"-O2".to_string()));
    assert!(!invocation.args.contains(&"-O0".to_string()));

    // Release should NOT have -g
    assert!(!invocation.args.contains(&"-g".to_string()));
}

#[tokio::test]
async fn test_node_id_cc_compile_format() {
    let db = test_db();
    setup_cc_config(&db);

    let source = CSourceFile::new(
        &db,
        "src/main.c".to_string(),
        Blake3Hash::from_bytes(b"int main() { return 0; }"),
    )
    .unwrap();

    let node_id = node_id_cc_compile(&db, source).await.unwrap();

    // Should be formatted as "cc-compile:path:triple:profile"
    assert!(node_id.0.starts_with("cc-compile:"));
    assert!(node_id.0.contains("src/main.c"));
    assert!(node_id.0.contains("x86_64-linux-musl"));
    assert!(node_id.0.contains("debug"));
}

#[tokio::test]
async fn test_cache_key_cc_link_deterministic() {
    let db = test_db();
    setup_cc_config(&db);

    let obj_hashes = vec![
        Blake3Hash::from_bytes(b"main.o contents"),
        Blake3Hash::from_bytes(b"util.o contents"),
    ];

    let target = CTarget::new(
        &db,
        "hello".to_string(),
        obj_hashes.clone(),
        vec!["src/main.c".to_string(), "src/util.c".to_string()],
    )
    .unwrap();

    let key1 = cache_key_cc_link(&db, target).await.unwrap();
    let key2 = cache_key_cc_link(&db, target).await.unwrap();

    assert_eq!(key1, key2);
}

#[tokio::test]
async fn test_cache_key_cc_link_changes_with_objects() {
    let db = test_db();
    setup_cc_config(&db);

    // First set of objects
    let target1 = CTarget::new(
        &db,
        "hello".to_string(),
        vec![Blake3Hash::from_bytes(b"main.o v1")],
        vec!["src/main.c".to_string()],
    )
    .unwrap();

    let key1 = cache_key_cc_link(&db, target1).await.unwrap();

    // Different object contents
    let target2 = CTarget::new(
        &db,
        "hello".to_string(),
        vec![Blake3Hash::from_bytes(b"main.o v2")],
        vec!["src/main.c".to_string()],
    )
    .unwrap();

    let key2 = cache_key_cc_link(&db, target2).await.unwrap();

    assert_ne!(key1, key2);
}

#[tokio::test]
async fn test_plan_cc_link_generates_correct_args() {
    let db = test_db();
    setup_cc_config(&db);

    let target = CTarget::new(
        &db,
        "hello".to_string(),
        vec![
            Blake3Hash::from_bytes(b"main.o"),
            Blake3Hash::from_bytes(b"util.o"),
        ],
        vec!["src/main.c".to_string(), "src/util.c".to_string()],
    )
    .unwrap();

    let invocation = plan_cc_link(&db, target).await.unwrap();

    // Should have "cc" as first arg
    assert_eq!(invocation.args[0], "cc");

    // Should have -target
    assert!(invocation.args.contains(&"-target".to_string()));
    assert!(invocation.args.contains(&"x86_64-linux-musl".to_string()));

    // Should have object files
    assert!(invocation.args.iter().any(|a| a.ends_with("main.o")));
    assert!(invocation.args.iter().any(|a| a.ends_with("util.o")));

    // Should have -o with exe output
    assert!(invocation.args.contains(&"-o".to_string()));
    let o_idx = invocation.args.iter().position(|a| a == "-o").unwrap();
    assert!(invocation.args[o_idx + 1].ends_with("hello"));

    // Debug profile should NOT have -s (strip)
    assert!(!invocation.args.contains(&"-s".to_string()));

    // Should expect executable output
    assert_eq!(invocation.expected_outputs.len(), 1);
    assert_eq!(invocation.expected_outputs[0].logical, "exe");
    assert!(invocation.expected_outputs[0].executable);
}

#[tokio::test]
async fn test_plan_cc_link_release_strips() {
    let db = test_db();

    ZigToolchainConfig::set(&db, "0.13.0".to_string(), "zig:abc123def456".to_string()).unwrap();

    // Release profile
    BuildConfig::set(
        &db,
        "release".to_string(),
        "x86_64-linux-musl".to_string(),
        "/test/workspace".to_string(),
    )
    .unwrap();

    let target = CTarget::new(
        &db,
        "hello".to_string(),
        vec![Blake3Hash::from_bytes(b"main.o")],
        vec!["src/main.c".to_string()],
    )
    .unwrap();

    let invocation = plan_cc_link(&db, target).await.unwrap();

    // Release should have -s for stripping
    assert!(invocation.args.contains(&"-s".to_string()));
}

#[tokio::test]
async fn test_node_id_cc_link_format() {
    let db = test_db();
    setup_cc_config(&db);

    let target = CTarget::new(
        &db,
        "hello".to_string(),
        vec![Blake3Hash::from_bytes(b"main.o")],
        vec!["src/main.c".to_string()],
    )
    .unwrap();

    let node_id = node_id_cc_link(&db, target).await.unwrap();

    // Should be formatted as "cc-link:name:triple:profile"
    assert!(node_id.0.starts_with("cc-link:"));
    assert!(node_id.0.contains("hello"));
    assert!(node_id.0.contains("x86_64-linux-musl"));
    assert!(node_id.0.contains("debug"));
}

#[tokio::test]
async fn test_discovered_deps_affects_cache_key() {
    let db = test_db();
    setup_cc_config(&db);

    let source_hash = Blake3Hash::from_bytes(b"#include \"header.h\"\nint main() {}");

    // First compile: no discovered deps yet
    let source1 = CSourceFile::new(&db, "src/main.c".to_string(), source_hash.clone()).unwrap();
    let key1 = cache_key_cc_compile(&db, source1).await.unwrap();

    // After first compile, we discover header.h dependency
    // Set discovered deps for this translation unit
    let tu_key = "cc:src/main.c:debug:x86_64-linux-musl".to_string();
    let deps_hash = Blake3Hash::from_bytes(b"include/header.h");
    DiscoveredDeps::new(&db, tu_key, deps_hash, vec!["include/header.h".to_string()]).unwrap();

    // Second compile with same source but now with discovered deps
    let source2 = CSourceFile::new(&db, "src/main.c".to_string(), source_hash).unwrap();
    let key2 = cache_key_cc_compile(&db, source2).await.unwrap();

    // Cache keys should be different because discovered deps changed
    assert_ne!(key1, key2);
}
