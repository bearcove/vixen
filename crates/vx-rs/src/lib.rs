//! Rust build support for vx
//!
//! This crate provides types and utilities for building Rust code,
//! including module discovery for computing accurate source closures
//! and crate graph resolution for multi-crate builds.

pub mod cache_key;
pub mod crate_graph;
pub mod depfile;
pub mod exec_action;
pub mod input_set;
pub mod module_scanner;
pub mod snapshot;

pub use cache_key::{compute_cache_key, compute_snapshot_hash, hash_file, hash_file_content};
pub use crate_graph::{CrateGraph, CrateGraphError, CrateId, CrateNode, CrateType, DepEdge};
pub use depfile::{normalize_dep_path, DepfileError, parse_depfile, parse_depfile_content};
pub use exec_action::{
    CompilationOutput, ExecutionStatus, GeneratedArtifact, OrchestrationDoc, RustDepInfoAction,
    RustDepInfoResult,
};
pub use input_set::{
    build_input_set_from_depinfo, classify_path, DeclaredExternal, GeneratedFile, InputRecords,
    InputSet, OutsideWorkspaceError, PathClassification, SnapshotIncompleteError, ToolchainFile,
    UndeclaredExtraInputsError, WorkspaceFile, validate_external_deps, validate_extra_inputs,
    validate_snapshot_complete,
};
pub use module_scanner::{
    ModDecl, ModuleError, hash_source_closure, resolve_mod_path, rust_source_closure,
    scan_mod_decls, validate_mod_decls,
};
pub use snapshot::{
    build_maximalist_snapshot, SnapshotEntry, SnapshotError, SnapshotManifest,
};
