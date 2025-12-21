//! Cargo.toml parsing for vx
//!
//! Parses `Cargo.toml` into a typed internal model and validates
//! that only the v0-supported subset is used.

use camino::{Utf8Path, Utf8PathBuf};
use facet::Facet;
use thiserror::Error;

/// Errors that can occur during manifest parsing
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read {path}: {source}")]
    ReadError {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse Cargo.toml: {0}")]
    ParseError(String),

    #[error("missing required field: [package].{0}")]
    MissingField(&'static str),

    #[error("unsupported: {feature} (found {details})")]
    Unsupported {
        feature: &'static str,
        details: String,
    },

    #[error("no binary target found (expected src/main.rs)")]
    NoBinaryTarget,
}

/// Edition of Rust to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum Edition {
    #[facet(rename = "2015")]
    E2015,
    #[facet(rename = "2018")]
    E2018,
    #[facet(rename = "2021")]
    E2021,
    #[facet(rename = "2024")]
    E2024,
}

impl Edition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Edition::E2015 => "2015",
            Edition::E2018 => "2018",
            Edition::E2021 => "2021",
            Edition::E2024 => "2024",
        }
    }
}

impl Default for Edition {
    fn default() -> Self {
        Edition::E2021
    }
}

/// A binary target
#[derive(Debug, Clone)]
pub struct BinTarget {
    pub name: String,
    pub path: Utf8PathBuf,
}

/// Parsed and validated manifest (v0 subset)
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub edition: Edition,
    pub bin: BinTarget,
}

/// Raw TOML structure for parsing (before validation)
#[derive(Facet, Debug)]
#[facet(rename_all = "kebab-case")]
struct RawManifest {
    package: Option<RawPackage>,
    bin: Option<Vec<RawBinTarget>>,

    dependencies: Option<toml_table::TomlTable>,
    dev_dependencies: Option<toml_table::TomlTable>,
    build_dependencies: Option<toml_table::TomlTable>,
    workspace: Option<toml_table::TomlTable>,
    features: Option<toml_table::TomlTable>,
}

#[derive(Facet, Debug)]
struct RawPackage {
    name: Option<String>,
    version: Option<String>,
    edition: Option<Edition>,
    build: Option<String>,
}

#[derive(Facet, Debug)]
struct RawBinTarget {
    name: Option<String>,
    path: Option<String>,
}

/// Placeholder for catching unknown TOML tables
mod toml_table {
    use facet::Facet;
    use std::collections::HashMap;

    #[derive(Facet, Debug, Default)]
    pub struct TomlTable {
        #[facet(default)]
        _items: HashMap<String, String>,
    }
}

impl Manifest {
    /// Parse a Cargo.toml file and validate it against v0 constraints
    pub fn from_path(path: &Utf8Path) -> Result<Self, ManifestError> {
        let contents = std::fs::read_to_string(path).map_err(|e| ManifestError::ReadError {
            path: path.to_owned(),
            source: e,
        })?;

        Self::from_str(&contents, path.parent())
    }

    /// Parse Cargo.toml content with a base directory for resolving src/main.rs
    pub fn from_str(contents: &str, base_dir: Option<&Utf8Path>) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            facet_toml::from_str(contents).map_err(|e| ManifestError::ParseError(e.to_string()))?;

        // Validate unsupported features
        if raw.dependencies.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[dependencies]",
                details: "dependencies are not supported yet".into(),
            });
        }
        if raw.dev_dependencies.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[dev-dependencies]",
                details: "dev-dependencies are not supported yet".into(),
            });
        }
        if raw.build_dependencies.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[build-dependencies]",
                details: "build-dependencies are not supported yet".into(),
            });
        }
        if raw.workspace.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[workspace]",
                details: "workspaces are not supported yet".into(),
            });
        }
        if raw.features.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[features]",
                details: "features are not supported yet".into(),
            });
        }

        let package = raw.package.ok_or(ManifestError::MissingField("name"))?;

        if package.build.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "build scripts",
                details: "build = \"...\" is not supported yet".into(),
            });
        }

        let name = package.name.ok_or(ManifestError::MissingField("name"))?;

        let edition = package.edition.unwrap_or_default();

        // Infer binary target from src/main.rs
        let main_path = if let Some(base) = base_dir {
            base.join("src/main.rs")
        } else {
            Utf8PathBuf::from("src/main.rs")
        };

        // For now, we just assume src/main.rs exists
        // The executor will fail with a clear error if it doesn't
        let bin = BinTarget {
            name: name.clone(),
            path: Utf8PathBuf::from("src/main.rs"),
        };

        Ok(Manifest { name, edition, bin })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
[package]
name = "hello"
edition = "2021"
"#;
        let manifest = Manifest::from_str(toml, None).unwrap();
        assert_eq!(manifest.name, "hello");
        assert_eq!(manifest.edition, Edition::E2021);
        assert_eq!(manifest.bin.name, "hello");
    }

    #[test]
    fn reject_dependencies() {
        let toml = r#"
[package]
name = "hello"

[dependencies]
foo = "1.0"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[dependencies]",
                ..
            }
        ));
    }

    #[test]
    fn reject_build_script() {
        let toml = r#"
[package]
name = "hello"
build = "build.rs"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "build scripts",
                ..
            }
        ));
    }
}
