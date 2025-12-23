//! Rhea service protocol definitions
//!
//! The rhea service is a remote-capable worker that compiles code.
//! All inputs and outputs go through CAS - no filesystem paths are shared.
//!
//! Rhea responsibilities:
//! - Materialize toolchains from CAS (cached locally)
//! - Materialize source trees from CAS
//! - Materialize dependency artifacts from CAS
//! - Run rustc/zig
//! - Ingest outputs to CAS
//! - Return output manifest hashes
//!
//! Daemon responsibilities:
//! - Ingest source files to CAS
//! - Orchestrate build order
//! - Compute cache keys
//! - Check cache hits
//! - Materialize final outputs locally (if needed)

use facet::Facet;
use vx_cas_proto::{ManifestHash, ServiceVersion};

/// Rhea protocol version.
/// Bump this when making breaking changes to the Rhea RPC interface.
pub const RHEA_PROTOCOL_VERSION: u32 = 1;

// =============================================================================
// Rust Compilation
// =============================================================================

/// A dependency for Rust compilation
#[derive(Debug, Clone, Facet)]
pub struct RustDep {
    /// Extern crate name (used in --extern flag)
    pub extern_name: String,
    /// Manifest hash of the dependency's rlib in CAS
    pub manifest_hash: ManifestHash,
    /// For registry dependencies: manifest hash of the registry crate tarball
    /// If present, rhea must materialize and compile the source before using the rlib
    pub registry_crate_manifest: Option<ManifestHash>,
}

/// Request to compile a Rust crate
///
/// All paths are relative to the source tree root.
/// All artifacts are identified by CAS manifest hashes.
#[derive(Debug, Clone, Facet)]
pub struct RustCompileRequest {
    /// Toolchain manifest hash (rhea materializes locally)
    pub toolchain_manifest: ManifestHash,

    /// Source tree manifest hash (all source files)
    pub source_manifest: ManifestHash,

    /// Crate root within source tree (e.g., "src/lib.rs")
    pub crate_root: String,

    /// Crate name (with underscores, as rustc expects)
    pub crate_name: String,

    /// Crate type: "lib" or "bin"
    pub crate_type: String,

    /// Rust edition: "2015", "2018", "2021", "2024"
    pub edition: String,

    /// Target triple (e.g., "aarch64-apple-darwin")
    pub target_triple: String,

    /// Profile: "debug" or "release"
    pub profile: String,

    /// Dependencies (rhea materializes rlibs from CAS)
    pub deps: Vec<RustDep>,
}

/// Result of compiling a Rust crate
#[derive(Debug, Clone, Facet)]
pub struct RustCompileResult {
    /// Whether compilation succeeded
    pub success: bool,

    /// Exit code from rustc
    pub exit_code: i32,

    /// Captured stdout
    pub stdout: String,

    /// Captured stderr
    pub stderr: String,

    /// Duration in milliseconds
    pub duration_ms: u64,

    /// Manifest hash of the build output in CAS (if successful)
    /// For libs: contains the rlib blob
    /// For bins: contains the executable blob
    pub output_manifest: Option<ManifestHash>,

    /// Error message (if failed before rustc ran)
    pub error: Option<String>,
}

// =============================================================================
// C/C++ Compilation
// =============================================================================

/// Request to compile a C/C++ translation unit
#[derive(Debug, Clone, Facet)]
pub struct CcCompileRequest {
    /// Zig toolchain manifest hash
    pub toolchain_manifest: ManifestHash,

    /// Source tree manifest hash
    pub source_manifest: ManifestHash,

    /// Source file within tree (e.g., "src/main.c")
    pub source_path: String,

    /// Target triple
    pub target_triple: String,

    /// Profile: "debug" or "release"
    pub profile: String,

    /// Include paths (relative to source tree root)
    pub include_paths: Vec<String>,

    /// Preprocessor defines
    pub defines: Vec<(String, Option<String>)>,
}

/// Result of compiling a C/C++ translation unit
#[derive(Debug, Clone, Facet)]
pub struct CcCompileResult {
    /// Whether compilation succeeded
    pub success: bool,

    /// Exit code
    pub exit_code: i32,

    /// Captured stdout
    pub stdout: String,

    /// Captured stderr
    pub stderr: String,

    /// Duration in milliseconds
    pub duration_ms: u64,

    /// Manifest hash of the object file in CAS
    pub output_manifest: Option<ManifestHash>,

    /// Discovered dependencies from depfile (paths relative to source tree)
    /// Used by daemon to update incremental state
    pub discovered_deps: Vec<String>,

    /// Error message if failed
    pub error: Option<String>,
}

// =============================================================================
// Rhea Service Trait
// =============================================================================

/// Rhea service trait
///
/// A remote-capable compilation service. All inputs/outputs go through CAS.
/// No filesystem paths are shared between aether and rhea.
#[rapace::service]
pub trait Rhea {
    /// Get service version information (for health checks and compatibility)
    async fn version(&self) -> ServiceVersion;

    /// Compile a Rust crate (lib or bin)
    ///
    /// Rhea will:
    /// 1. Materialize the toolchain from CAS (cached locally)
    /// 2. Materialize the source tree from CAS
    /// 3. Materialize dependency rlibs from CAS
    /// 4. Run rustc
    /// 5. Ingest output to CAS
    /// 6. Return the output manifest hash
    async fn compile_rust(&self, request: RustCompileRequest) -> RustCompileResult;

    /// Compile a C/C++ translation unit
    ///
    /// Similar to compile_rust but for C/C++ via zig cc.
    /// Returns discovered dependencies for incremental builds.
    async fn compile_cc(&self, request: CcCompileRequest) -> CcCompileResult;
}
