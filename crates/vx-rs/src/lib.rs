//! Rust build support for vx
//!
//! This crate provides types and utilities for building Rust code,
//! including module discovery for computing accurate source closures.

pub mod module_scanner;

pub use module_scanner::{
    ModDecl, ModuleError, hash_source_closure, resolve_mod_path, rust_source_closure,
    scan_mod_decls, validate_mod_decls,
};
