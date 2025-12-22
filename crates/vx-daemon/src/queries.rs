//! Picante tracked queries for cache key computation and node IDs.
//!
//! These queries compute cache keys based on inputs (source closures, toolchains, etc.)
//! but do NOT plan invocations - that's execd's responsibility now.

use crate::db::Db;
use crate::inputs::*;
use picante::PicanteResult;
use tracing::debug;
use vx_cas_proto::{Blake3Hash, CacheKey, NodeId};

// =============================================================================
// RUST CACHE KEYS
// =============================================================================

/// Compute the cache key for compiling a binary crate (single-crate, no deps).
#[picante::tracked]
pub async fn cache_key_compile_bin<DB: Db>(db: &DB) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_bin: COMPUTING");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let closure = SourceClosure::get(db)?.expect("SourceClosure not set");
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"name:");
    hasher.update(cargo.name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(cargo.edition.as_str().as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure.closure_hash.0);
    hasher.update(b"\n");

    hasher.update(b"manifest:");
    hasher.update(&cargo.content_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Generate a human-readable node ID for a compile-bin node.
#[picante::tracked]
pub async fn node_id_compile_bin<DB: Db>(db: &DB) -> PicanteResult<NodeId> {
    debug!("node_id_compile_bin: COMPUTING");

    let cargo = CargoToml::get(db)?.expect("CargoToml not set");
    let config = BuildConfig::get(db)?.expect("BuildConfig not set");

    Ok(NodeId(format!(
        "compile-bin:{}:{}:{}",
        cargo.name, config.target_triple, config.profile
    )))
}

/// Compute the cache key for compiling a library crate (rlib, no deps).
#[picante::tracked]
pub async fn cache_key_compile_rlib<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_rlib: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-rlib-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:lib\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Generate a human-readable node ID for a compile-rlib node.
#[picante::tracked]
pub async fn node_id_compile_rlib<DB: Db>(db: &DB, crate_info: RustCrate) -> PicanteResult<NodeId> {
    debug!("node_id_compile_rlib: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let crate_name = crate_info.crate_name(db)?;

    Ok(NodeId(format!(
        "compile-rlib:{}:{}:{}",
        &*crate_name, &*config.target_triple, &*config.profile
    )))
}

/// Compute the cache key for compiling a library crate with dependencies.
#[picante::tracked]
pub async fn cache_key_compile_rlib_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_rlib_hashes: Vec<(String, Blake3Hash)>, // (extern_name, rlib_hash) sorted
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_rlib_with_deps: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-rlib-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:lib\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure_hash.0);
    hasher.update(b"\n");

    // Include dependency rlib hashes (sorted by extern_name for determinism)
    for (extern_name, rlib_hash) in &dep_rlib_hashes {
        hasher.update(b"dep:");
        hasher.update(extern_name.as_bytes());
        hasher.update(b":");
        hasher.update(&rlib_hash.0);
        hasher.update(b"\n");
    }

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Compute the cache key for compiling a binary crate with dependencies.
#[picante::tracked]
pub async fn cache_key_compile_bin_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
    dep_rlib_hashes: Vec<(String, Blake3Hash)>, // (extern_name, rlib_hash) sorted
) -> PicanteResult<CacheKey> {
    debug!("cache_key_compile_bin_with_deps: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let toolchain =
        RustToolchain::get(db)?.expect("RustToolchain not set - hermetic toolchain required");

    let crate_name = crate_info.crate_name(db)?;
    let edition = crate_info.edition(db)?;
    let closure_hash = crate_info.source_closure_hash(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-bin-deps-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"rust_toolchain:");
    hasher.update(&toolchain.toolchain_id.0);
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:bin\n");

    hasher.update(b"source_closure:");
    hasher.update(&closure_hash.0);
    hasher.update(b"\n");

    // Include dependency hashes
    hasher.update(b"deps:");
    for (extern_name, rlib_hash) in &dep_rlib_hashes {
        hasher.update(extern_name.as_bytes());
        hasher.update(b"=");
        hasher.update(&rlib_hash.0);
        hasher.update(b";");
    }
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Generate a human-readable node ID for a compile-bin-with-deps node.
#[picante::tracked]
pub async fn node_id_compile_bin_with_deps<DB: Db>(
    db: &DB,
    crate_info: RustCrate,
) -> PicanteResult<NodeId> {
    debug!("node_id_compile_bin_with_deps: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let crate_name = crate_info.crate_name(db)?;

    Ok(NodeId(format!(
        "compile-bin:{}:{}:{}",
        &*crate_name, &*config.target_triple, &*config.profile
    )))
}

// =============================================================================
// C/C++ CACHE KEYS
// =============================================================================

/// Compute the cache key for compiling a C/C++ translation unit.
#[picante::tracked]
pub async fn cache_key_cc_compile<DB: Db>(db: &DB, source: CSourceFile) -> PicanteResult<CacheKey> {
    debug!("cache_key_cc_compile: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let zig = ZigToolchainConfig::get(db)?.expect("ZigToolchainConfig not set");
    let source_hash = source.content_hash(db)?;
    let source_path = source.path(db)?;

    // Build TU key for looking up discovered deps
    let tu_key = format!(
        "cc:{}:{}:{}",
        &*source_path, &*config.profile, &*config.target_triple
    );

    // Get discovered deps (may not exist on first compile)
    let deps_hash = match db.discovered_deps_keys().intern(tu_key.clone()) {
        Ok(intern_id) => {
            if let Some(data) = db.discovered_deps_data().get(db, &intern_id)? {
                data.deps_hash.clone()
            } else {
                Blake3Hash::from_bytes(b"no-deps-yet")
            }
        }
        Err(_) => Blake3Hash::from_bytes(b"no-deps-yet"),
    };

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cc-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(zig.toolchain_id.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"source:");
    hasher.update(&source_hash.0);
    hasher.update(b"\n");

    hasher.update(b"deps:");
    hasher.update(&deps_hash.0);
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Generate a human-readable node ID for a cc-compile node.
#[picante::tracked]
pub async fn node_id_cc_compile<DB: Db>(db: &DB, source: CSourceFile) -> PicanteResult<NodeId> {
    debug!("node_id_cc_compile: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let source_path = source.path(db)?;

    Ok(NodeId(format!(
        "cc-compile:{}:{}:{}",
        &*source_path, &*config.target_triple, &*config.profile
    )))
}

/// Compute the cache key for linking a C/C++ target.
#[picante::tracked]
pub async fn cache_key_cc_link<DB: Db>(db: &DB, target: CTarget) -> PicanteResult<CacheKey> {
    debug!("cache_key_cc_link: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let zig = ZigToolchainConfig::get(db)?.expect("ZigToolchainConfig not set");
    let object_hashes = target.object_hashes(db)?;

    let mut hasher = blake3::Hasher::new();

    hasher.update(b"vx-cc-link-cache-key-v");
    hasher.update(&vx_cas_proto::CACHE_KEY_SCHEMA_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(zig.toolchain_id.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(config.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(config.profile.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"objects:");
    for obj_hash in object_hashes.iter() {
        hasher.update(&obj_hash.0);
    }
    hasher.update(b"\n");

    Ok(Blake3Hash(*hasher.finalize().as_bytes()))
}

/// Generate a human-readable node ID for a cc-link node.
#[picante::tracked]
pub async fn node_id_cc_link<DB: Db>(db: &DB, target: CTarget) -> PicanteResult<NodeId> {
    debug!("node_id_cc_link: COMPUTING");

    let config = BuildConfig::get(db)?.expect("BuildConfig not set");
    let target_name = target.name(db)?;

    Ok(NodeId(format!(
        "cc-link:{}:{}:{}",
        &*target_name, &*config.target_triple, &*config.profile
    )))
}
