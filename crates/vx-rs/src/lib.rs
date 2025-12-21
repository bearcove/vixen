//! Rust build support for vx
//!
//! This crate provides types and utilities for building Rust code,
//! including module discovery for computing accurate source closures
//! and crate graph resolution for multi-crate builds.

pub mod crate_graph;
pub mod depfile;
pub mod input_set;
pub mod module_scanner;

pub use crate_graph::{CrateGraph, CrateGraphError, CrateId, CrateNode, CrateType, DepEdge};
pub use depfile::{DepfileError, parse_depfile, parse_depfile_content};
pub use input_set::{
    build_input_set_from_depinfo, classify_path, DeclaredExternal, GeneratedFile, InputSet,
    PathClassification, ToolchainFile, WorkspaceFile,
};
pub use module_scanner::{
    ModDecl, ModuleError, hash_source_closure, resolve_mod_path, rust_source_closure,
    scan_mod_decls, validate_mod_decls,
};
