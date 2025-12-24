//! vx.kdl project manifest parsing
//!
//! Provides unified project configuration for Rust, C, and mixed projects.
//! Falls back to Cargo.toml if vx.kdl doesn't exist.

use camino::{Utf8Path, Utf8PathBuf};
use facet_kdl as kdl;
use thiserror::Error;

/// Errors during vx.kdl parsing
#[derive(Debug, Error)]
pub enum VxProjectError {
    #[error("failed to read {path}: {source}")]
    ReadError {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse vx.kdl: {0}")]
    ParseError(String),

    #[error("missing required node: {0}")]
    MissingNode(&'static str),

    #[error("missing required property: {0}")]
    MissingProperty(&'static str),

    #[error("invalid value for {property}: {reason}")]
    InvalidValue {
        property: &'static str,
        reason: String,
    },

    #[error("no targets found (expected bin or lib)")]
    NoTargets,
}

/// Project metadata
#[derive(Debug, Clone, facet::Facet)]
pub struct Project {
    #[facet(kdl::property)]
    pub name: String,
    #[facet(kdl::property)]
    pub lang: String,
}

impl Project {
    /// Get the language as an enum
    pub fn language(&self) -> Option<Language> {
        match self.lang.as_str() {
            "rust" => Some(Language::Rust),
            "c" => Some(Language::C),
            _ => None,
        }
    }
}

/// Project language
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    C,
}

/// Binary target
#[derive(Debug, Clone, facet::Facet)]
pub struct BinTarget {
    #[facet(kdl::property)]
    pub name: String,
    /// Source file(s) - single file for now
    #[facet(kdl::property)]
    pub sources: String,
}

/// Library target (future)
#[derive(Debug, Clone, facet::Facet)]
pub struct LibTarget {
    #[facet(kdl::property)]
    pub name: String,
    #[facet(kdl::property)]
    pub sources: String,
}

/// Top-level vx.kdl manifest
#[derive(Debug, Clone, facet::Facet)]
pub struct VxManifest {
    #[facet(kdl::child)]
    pub project: Project,
    #[facet(kdl::children, rename = "bin", default)]
    pub bins: Vec<BinTarget>,
    #[facet(kdl::children, rename = "lib", default)]
    pub libs: Vec<LibTarget>,
}

impl VxManifest {
    /// Parse vx.kdl from a file path
    pub fn from_path(path: &Utf8Path) -> Result<Self, VxProjectError> {
        let contents = std::fs::read_to_string(path).map_err(|e| VxProjectError::ReadError {
            path: path.to_owned(),
            source: e,
        })?;

        Self::parse(&contents)
    }

    /// Parse vx.kdl from a string
    pub fn parse(source: &str) -> Result<Self, VxProjectError> {
        facet_kdl::from_str(source).map_err(|e| VxProjectError::ParseError(e.to_string()))
    }

    /// Check if vx.kdl exists in the given directory
    pub fn exists_in(dir: &Utf8Path) -> bool {
        dir.join("vx.kdl").exists()
    }

    /// Find vx.kdl starting from the given directory and searching upward
    pub fn find(start_path: &Utf8Path) -> Option<Utf8PathBuf> {
        let mut current = start_path;
        loop {
            let vx_kdl = current.join("vx.kdl");
            if vx_kdl.exists() {
                return Some(vx_kdl);
            }

            current = current.parent()?;
        }
    }

    /// Validate the manifest (check for required fields, consistency, etc.)
    pub fn validate(&self) -> Result<(), VxProjectError> {
        // Must have at least one target
        if self.bins.is_empty() && self.libs.is_empty() {
            return Err(VxProjectError::NoTargets);
        }

        // Project name must not be empty
        if self.project.name.is_empty() {
            return Err(VxProjectError::InvalidValue {
                property: "project.name",
                reason: "name cannot be empty".to_string(),
            });
        }

        // Binary names must not be empty
        for bin in &self.bins {
            if bin.name.is_empty() {
                return Err(VxProjectError::InvalidValue {
                    property: "bin.name",
                    reason: "name cannot be empty".to_string(),
                });
            }
            if bin.sources.is_empty() {
                return Err(VxProjectError::InvalidValue {
                    property: "bin.sources",
                    reason: "sources cannot be empty".to_string(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_c_project() {
        let kdl = r#"
            project name="hello" lang="c"
            bin name="hello" sources="main.c"
        "#;

        let manifest = VxManifest::parse(kdl).unwrap();
        assert_eq!(manifest.project.name, "hello");
        assert_eq!(manifest.project.lang, "c");
        assert_eq!(manifest.project.language(), Some(Language::C));
        assert_eq!(manifest.bins.len(), 1);
        assert_eq!(manifest.bins[0].name, "hello");
        assert_eq!(manifest.bins[0].sources, "main.c");
    }

    #[test]
    fn test_parse_rust_project() {
        let kdl = r#"
            project name="my-app" lang="rust"
            bin name="my-app" sources="src/main.rs"
        "#;

        let manifest = VxManifest::parse(kdl).unwrap();
        assert_eq!(manifest.project.name, "my-app");
        assert_eq!(manifest.project.lang, "rust");
        assert_eq!(manifest.project.language(), Some(Language::Rust));
    }

    #[test]
    fn test_validate_no_targets() {
        let kdl = r#"
            project name="empty" lang="c"
        "#;

        let manifest = VxManifest::parse(kdl).unwrap();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, VxProjectError::NoTargets));
    }

    #[test]
    fn test_validate_empty_name() {
        let kdl = r#"
            project name="" lang="c"
            bin name="hello" sources="main.c"
        "#;

        let manifest = VxManifest::parse(kdl).unwrap();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, VxProjectError::InvalidValue { .. }));
    }
}
