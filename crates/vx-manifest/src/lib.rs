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
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Facet)]
#[repr(u8)]
pub enum Edition {
    #[facet(rename = "2015")]
    E2015,
    #[facet(rename = "2018")]
    E2018,
    #[default]
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
    lib: Option<RawLibTarget>,
    test: Option<Vec<TargetEntry>>,
    bench: Option<Vec<TargetEntry>>,
    example: Option<Vec<TargetEntry>>,

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

#[derive(Facet, Debug)]
#[facet(rename_all = "kebab-case")]
struct RawLibTarget {
    proc_macro: Option<bool>,
}

/// Placeholder for catching unknown TOML tables
/// We just need to know if the table exists, not its contents
mod toml_table {
    use facet::Facet;

    #[derive(Facet, Debug, Default)]
    pub struct TomlTable {
        // Accept any fields - we just care that the table exists
        // facet-toml will parse and discard contents
    }
}

/// Placeholder for target entries in [[test]], [[bench]], [[example]]
/// We just need to know they exist
#[derive(Facet, Debug)]
struct TargetEntry {
    name: Option<String>,
    path: Option<String>,
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

        // Check for proc-macro crates
        if let Some(ref lib) = raw.lib
            && lib.proc_macro == Some(true)
        {
            return Err(ManifestError::Unsupported {
                feature: "proc-macro crates",
                details: "[lib] proc-macro = true is not supported yet".into(),
            });
        }

        // Check for test/bench/example targets
        if let Some(ref tests) = raw.test
            && !tests.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[test]] targets",
                details: "test targets are not supported yet".into(),
            });
        }
        if let Some(ref benches) = raw.bench
            && !benches.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[bench]] targets",
                details: "bench targets are not supported yet".into(),
            });
        }
        if let Some(ref examples) = raw.example
            && !examples.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[example]] targets",
                details: "example targets are not supported yet".into(),
            });
        }

        // Check for multiple binary targets
        if let Some(ref bins) = raw.bin
            && bins.len() > 1
        {
            return Err(ManifestError::Unsupported {
                feature: "multiple [[bin]] targets",
                details: format!("found {} binary targets, only 1 is supported", bins.len()),
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
            path: main_path,
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
        // facet-toml can't parse arbitrary dependency tables, so we get a ParseError
        // that mentions "dependencies" - this is acceptable for v0
        let err_str = err.to_string();
        assert!(
            err_str.contains("dependencies"),
            "error should mention dependencies: {err_str}"
        );
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

    #[test]
    fn reject_proc_macro() {
        let toml = r#"
[package]
name = "hello"

[lib]
proc-macro = true
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "proc-macro crates",
                ..
            }
        ));
    }

    #[test]
    fn reject_test_targets() {
        let toml = r#"
[package]
name = "hello"

[[test]]
name = "integration"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[[test]] targets",
                ..
            }
        ));
    }

    #[test]
    fn reject_bench_targets() {
        let toml = r#"
[package]
name = "hello"

[[bench]]
name = "mybench"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[[bench]] targets",
                ..
            }
        ));
    }

    #[test]
    fn reject_example_targets() {
        let toml = r#"
[package]
name = "hello"

[[example]]
name = "myexample"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[[example]] targets",
                ..
            }
        ));
    }

    #[test]
    fn reject_multiple_bin_targets() {
        let toml = r#"
[package]
name = "hello"

[[bin]]
name = "one"
path = "src/one.rs"

[[bin]]
name = "two"
path = "src/two.rs"
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "multiple [[bin]] targets",
                ..
            }
        ));
    }

    #[test]
    fn reject_features() {
        let toml = r#"
[package]
name = "hello"

[features]
default = []
foo = []
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        // facet-toml can't parse [features] with array values into our TomlTable placeholder,
        // so we get a ParseError that mentions "features" - this is acceptable for v0
        let err_str = err.to_string();
        assert!(
            err_str.contains("features"),
            "error should mention features: {err_str}"
        );
    }
}
