//! Error types for vx-rhea.

use camino::Utf8PathBuf;
use facet::Facet;
use thiserror::Error;
use vx_oort_proto::Blake3Hash;

/// Errors that can occur during exec operations.
#[derive(Debug, Clone, Error, Facet)]
#[repr(u8)]
pub enum RheaError {
    // === Toolchain Errors ===
    #[error("toolchain manifest {0} not found in CAS")]
    ToolchainManifestNotFound(Blake3Hash),

    #[error("toolchain blob {hash} not found for file {path}")]
    ToolchainBlobNotFound { hash: Blake3Hash, path: String },

    #[error("failed to materialize toolchain: {0}")]
    ToolchainMaterialization(String),

    #[error("symlinks not supported on this platform")]
    SymlinksNotSupported,

    // === Registry Errors ===
    #[error("registry crate manifest {0} not found")]
    RegistryCrateManifestNotFound(Blake3Hash),

    #[error("registry crate blob {0} not found")]
    RegistryCrateBlobNotFound(Blake3Hash),

    #[error("failed to extract registry crate: {0}")]
    RegistryCrateExtraction(String),

    #[error("failed to acquire lock {path}: {reason}")]
    LockAcquisition { path: Utf8PathBuf, reason: String },

    // === CAS/RPC Errors ===
    // Note: RpcError doesn't implement Facet (wraps io::Error), so we stringify
    #[error("CAS RPC error: {0}")]
    CasRpc(String),

    // === IO Errors ===
    #[error("failed to create directory {path}: {message}")]
    CreateDir { path: Utf8PathBuf, message: String },

    #[error("failed to write file {path}: {message}")]
    WriteFile { path: Utf8PathBuf, message: String },

    #[error("failed to read file {path}: {message}")]
    ReadFile { path: Utf8PathBuf, message: String },

    #[error("failed to create scratch directory: {0}")]
    ScratchDir(String),

    #[error("failed to set permissions on {path}: {message}")]
    SetPermissions { path: Utf8PathBuf, message: String },

    // === Compilation Errors ===
    #[error("dependency manifest {0} not found")]
    DependencyManifestNotFound(Blake3Hash),

    #[error("no rlib output in manifest for {0}")]
    NoRlibOutput(String),

    #[error("rlib blob {hash} not found for {extern_name}")]
    RlibBlobNotFound {
        hash: Blake3Hash,
        extern_name: String,
    },

    #[error("failed to execute rustc: {0}")]
    RustcExecution(String),

    #[error("failed to store blob in CAS: {0}")]
    CasBlobStore(String),

    #[error("failed to store manifest in CAS: {0}")]
    CasManifestStore(String),
}

/// Result type for exec operations.
pub type Result<T> = std::result::Result<T, RheaError>;
