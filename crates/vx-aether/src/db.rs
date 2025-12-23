//! Picante database definition for incremental builds.

use crate::inputs::*;
use crate::queries::*;

#[picante::db(
    inputs(
        SourceClosure,
        CargoToml,
        BuildConfig,
        // Hermetic toolchain inputs
        RustToolchain,
        RustToolchainManifest,
        ZigToolchain,
        // Multi-crate Rust inputs
        RustCrate,
        RlibOutput,
        // C/C++ inputs
        CSourceFile,
        DiscoveredDeps,
        ZigToolchainConfig,
        CTarget,
    ),
    tracked(
        // Single-crate Rust (legacy)
        cache_key_compile_bin,
        node_id_compile_bin,
        // Multi-crate Rust
        cache_key_compile_rlib,
        node_id_compile_rlib,
        cache_key_compile_rlib_with_deps,
        cache_key_compile_bin_with_deps,
        node_id_compile_bin_with_deps,
        // C/C++
        cache_key_cc_compile,
        node_id_cc_compile,
        cache_key_cc_link,
        node_id_cc_link,
    ),
    db_trait(Db)
)]
pub struct Database {}
