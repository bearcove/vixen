//! Cache key computation for vixen build artifacts.
//!
//! This crate centralizes all cache key computation logic to ensure
//! consistency between daemon (for cache lookups) and execd (for cache storage).
//!
//! Cache keys are blake3 hashes of all inputs that affect the build output.

use vx_cass_proto::Blake3Hash;
use vx_rhea_proto::{CcCompileRequest, CcLinkRequest, RustCompileRequest};

/// Current cache key schema version for Rust compilation.
/// Bump this when the cache key format changes.
pub const RUST_CACHE_KEY_VERSION: u32 = 1;

/// Current cache key schema version for C/C++ compilation.
pub const CC_CACHE_KEY_VERSION: u32 = 1;

/// Compute a cache key for a Rust compile request.
///
/// The cache key includes all inputs that affect the build output:
/// - Toolchain manifest
/// - Source tree manifest
/// - Crate metadata (name, type, edition, root)
/// - Target triple and profile
/// - Dependencies (sorted by extern_name for determinism)
pub fn rust_compile_cache_key(request: &RustCompileRequest) -> Blake3Hash {
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"rust-compile-v");
    hasher.update(&RUST_CACHE_KEY_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(&request.toolchain_manifest.0);
    hasher.update(b"\n");

    hasher.update(b"source:");
    hasher.update(&request.source_manifest.0);
    hasher.update(b"\n");

    hasher.update(b"crate_root:");
    hasher.update(request.crate_root.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_name:");
    hasher.update(request.crate_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"crate_type:");
    hasher.update(request.crate_type.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"edition:");
    hasher.update(request.edition.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(request.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(request.profile.as_bytes());
    hasher.update(b"\n");

    // Hash deps (sorted by extern_name for determinism)
    let mut deps: Vec<_> = request.deps.iter().collect();
    deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));

    for dep in deps {
        hasher.update(b"dep:");
        hasher.update(dep.extern_name.as_bytes());
        hasher.update(b":");
        hasher.update(&dep.manifest_hash.0);
        hasher.update(b"\n");
    }

    Blake3Hash(*hasher.finalize().as_bytes())
}

/// Compute a cache key for a C/C++ compile request.
///
/// The cache key includes all inputs that affect the build output:
/// - Toolchain manifest (Zig version)
/// - Source tree manifest
/// - Source file path
/// - Target triple and profile
/// - Include paths (sorted for determinism)
/// - Defines (sorted for determinism)
pub fn cc_compile_cache_key(request: &CcCompileRequest) -> Blake3Hash {
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"cc-compile-v");
    hasher.update(&CC_CACHE_KEY_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(&request.toolchain_manifest.0);
    hasher.update(b"\n");

    hasher.update(b"source:");
    hasher.update(&request.source_manifest.0);
    hasher.update(b"\n");

    hasher.update(b"source_path:");
    hasher.update(request.source_path.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(request.target_triple.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"profile:");
    hasher.update(request.profile.as_bytes());
    hasher.update(b"\n");

    // Hash include paths (sorted for determinism)
    let mut includes: Vec<_> = request.include_paths.iter().collect();
    includes.sort();
    for include in includes {
        hasher.update(b"include:");
        hasher.update(include.as_bytes());
        hasher.update(b"\n");
    }

    // Hash defines (sorted for determinism)
    let mut defines: Vec<_> = request.defines.iter().collect();
    defines.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in defines {
        hasher.update(b"define:");
        hasher.update(key.as_bytes());
        if let Some(v) = value {
            hasher.update(b"=");
            hasher.update(v.as_bytes());
        }
        hasher.update(b"\n");
    }

    Blake3Hash(*hasher.finalize().as_bytes())
}

/// Compute a cache key for a C/C++ link request.
///
/// The cache key includes all inputs that affect the build output:
/// - Toolchain manifest (Zig version)
/// - Object file manifests (sorted for determinism)
/// - Binary name
/// - Target triple
/// - Libraries (sorted for determinism)
pub fn cc_link_cache_key(request: &CcLinkRequest) -> Blake3Hash {
    let mut hasher = blake3::Hasher::new();

    hasher.update(b"cc-link-v");
    hasher.update(&CC_CACHE_KEY_VERSION.to_le_bytes());
    hasher.update(b"\n");

    hasher.update(b"toolchain:");
    hasher.update(&request.toolchain_manifest.0);
    hasher.update(b"\n");

    hasher.update(b"binary:");
    hasher.update(request.binary_name.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"target:");
    hasher.update(request.target_triple.as_bytes());
    hasher.update(b"\n");

    // Hash object manifests (sorted for determinism)
    let mut objects: Vec<_> = request.object_manifests.iter().collect();
    objects.sort_by_key(|h| h.0);
    for obj in objects {
        hasher.update(b"object:");
        hasher.update(&obj.0);
        hasher.update(b"\n");
    }

    // Hash libraries (sorted for determinism)
    let mut libs: Vec<_> = request.libs.iter().collect();
    libs.sort();
    for lib in libs {
        hasher.update(b"lib:");
        hasher.update(lib.as_bytes());
        hasher.update(b"\n");
    }

    Blake3Hash(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vx_rhea_proto::RustDep;

    #[test]
    fn test_cache_key_determinism() {
        let request = RustCompileRequest {
            toolchain_manifest: Blake3Hash([1; 32]),
            source_manifest: Blake3Hash([2; 32]),
            crate_root: "src/lib.rs".to_string(),
            crate_name: "mylib".to_string(),
            crate_type: "lib".to_string(),
            edition: "2021".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            profile: "release".to_string(),
            deps: vec![],
            registry_crate_manifest: None,
        };

        let key1 = rust_compile_cache_key(&request);
        let key2 = rust_compile_cache_key(&request);

        assert_eq!(key1, key2, "cache key should be deterministic");
    }

    #[test]
    fn test_cache_key_deps_order_independent() {
        let dep_a = RustDep {
            extern_name: "aaa".to_string(),
            manifest_hash: Blake3Hash([10; 32]),
            registry_crate_manifest: None,
        };
        let dep_b = RustDep {
            extern_name: "bbb".to_string(),
            manifest_hash: Blake3Hash([20; 32]),
            registry_crate_manifest: None,
        };

        let request1 = RustCompileRequest {
            toolchain_manifest: Blake3Hash([1; 32]),
            source_manifest: Blake3Hash([2; 32]),
            crate_root: "src/lib.rs".to_string(),
            crate_name: "mylib".to_string(),
            crate_type: "lib".to_string(),
            edition: "2021".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            profile: "release".to_string(),
            deps: vec![dep_a.clone(), dep_b.clone()],
            registry_crate_manifest: None,
        };

        let request2 = RustCompileRequest {
            toolchain_manifest: Blake3Hash([1; 32]),
            source_manifest: Blake3Hash([2; 32]),
            crate_root: "src/lib.rs".to_string(),
            crate_name: "mylib".to_string(),
            crate_type: "lib".to_string(),
            edition: "2021".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            profile: "release".to_string(),
            deps: vec![dep_b, dep_a], // reversed order
            registry_crate_manifest: None,
        };

        let key1 = rust_compile_cache_key(&request1);
        let key2 = rust_compile_cache_key(&request2);

        assert_eq!(key1, key2, "cache key should be independent of dep order");
    }
}
