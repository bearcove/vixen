//! Cargo.toml parsing for vx
//!
//! Parses `Cargo.toml` into a typed internal model and validates
//! that only the v0-supported subset is used.
//!
//! ## v0.3 Subset
//!
//! - Package with [lib] or [[bin]] targets (at least one required)
//! - Path dependencies only: `foo = { path = "../foo" }`
//! - No features, no optional deps, no dev-deps/build-deps
//! - No build.rs, proc-macros, tests/benches/examples

use std::collections::HashMap;

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

    #[error("no targets found (expected [lib] or [[bin]] with src/lib.rs or src/main.rs)")]
    NoTargets,

    #[error("invalid dependency '{name}': {reason}")]
    InvalidDependency { name: String, reason: String },
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

/// A library target
#[derive(Debug, Clone)]
pub struct LibTarget {
    pub name: String,
    pub path: Utf8PathBuf,
}

/// A path dependency
#[derive(Debug, Clone)]
pub struct PathDependency {
    /// Dependency name (used as extern crate name)
    pub name: String,
    /// Path to the dependency (relative to manifest directory)
    pub path: Utf8PathBuf,
}

/// Parsed and validated manifest (v0.3 subset)
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub edition: Edition,
    /// Library target (if this crate has a lib.rs)
    pub lib: Option<LibTarget>,
    /// Binary target (if this crate has a main.rs or [[bin]])
    pub bin: Option<BinTarget>,
    /// Path dependencies
    pub deps: Vec<PathDependency>,
}

impl Manifest {
    /// Returns true if this is a library crate
    pub fn is_lib(&self) -> bool {
        self.lib.is_some()
    }

    /// Returns true if this is a binary crate
    pub fn is_bin(&self) -> bool {
        self.bin.is_some()
    }

    /// Get the crate name (with hyphens converted to underscores)
    pub fn crate_name(&self) -> String {
        self.name.replace('-', "_")
    }
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

    /// Dependencies as a map of name -> dependency spec
    dependencies: Option<HashMap<String, RawDependency>>,
    dev_dependencies: Option<HashMap<String, RawDependency>>,
    build_dependencies: Option<HashMap<String, RawDependency>>,
    workspace: Option<RawWorkspace>,
    features: Option<HashMap<String, Vec<String>>>,
}

/// A dependency specification - can be a version string or a table
#[derive(Facet, Debug)]
#[repr(u8)]
#[facet(untagged)]
enum RawDependency {
    /// Simple version string: `foo = "1.0"` - we reject these but need to parse them
    #[allow(dead_code)]
    Version(String),
    /// Table form: `foo = { path = "...", version = "...", etc }`
    Table(RawDependencyTable),
}

/// Detailed dependency table
#[derive(Facet, Debug, Default)]
#[facet(rename_all = "kebab-case")]
struct RawDependencyTable {
    path: Option<String>,
    version: Option<String>,
    git: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
    registry: Option<String>,
    features: Option<Vec<String>>,
    optional: Option<bool>,
    default_features: Option<bool>,
    package: Option<String>,
}

/// Placeholder for workspace (just to detect it exists)
#[derive(Facet, Debug)]
struct RawWorkspace {
    members: Option<Vec<String>>,
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
    name: Option<String>,
    path: Option<String>,
    proc_macro: Option<bool>,
}

/// Placeholder for target entries in [[test]], [[bench]], [[example]]
/// We just need to know they exist
#[derive(Facet, Debug)]
struct TargetEntry {
    name: Option<String>,
    path: Option<String>,
}

impl Manifest {
    /// Parse a Cargo.toml file and validate it against v0.3 constraints
    pub fn from_path(path: &Utf8Path) -> Result<Self, ManifestError> {
        let contents = std::fs::read_to_string(path).map_err(|e| ManifestError::ReadError {
            path: path.to_owned(),
            source: e,
        })?;

        Self::from_str(&contents, path.parent())
    }

    /// Parse Cargo.toml content with a base directory for resolving paths
    pub fn from_str(contents: &str, base_dir: Option<&Utf8Path>) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            facet_toml::from_str(contents).map_err(|e| ManifestError::ParseError(e.to_string()))?;

        // Validate unsupported features
        if raw.dev_dependencies.is_some() && !raw.dev_dependencies.as_ref().unwrap().is_empty() {
            return Err(ManifestError::Unsupported {
                feature: "[dev-dependencies]",
                details: "dev-dependencies are not supported yet".into(),
            });
        }
        if raw.build_dependencies.is_some() && !raw.build_dependencies.as_ref().unwrap().is_empty()
        {
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
        if raw.features.is_some() && !raw.features.as_ref().unwrap().is_empty() {
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

        // Parse dependencies
        let deps = parse_dependencies(raw.dependencies)?;

        // Determine library target
        let lib = determine_lib_target(&name, raw.lib, base_dir);

        // Determine binary target
        let bin = determine_bin_target(&name, raw.bin, base_dir);

        // Must have at least one target
        if lib.is_none() && bin.is_none() {
            return Err(ManifestError::NoTargets);
        }

        Ok(Manifest {
            name,
            edition,
            lib,
            bin,
            deps,
        })
    }
}

/// Parse [dependencies] table, extracting only path dependencies
fn parse_dependencies(
    deps_map: Option<HashMap<String, RawDependency>>,
) -> Result<Vec<PathDependency>, ManifestError> {
    let Some(deps) = deps_map else {
        return Ok(Vec::new());
    };

    let mut result = Vec::new();

    for (name, dep) in deps {
        match dep {
            RawDependency::Version(_) => {
                return Err(ManifestError::InvalidDependency {
                    name,
                    reason: "version dependencies are not supported, only path = \"...\"".into(),
                });
            }
            RawDependency::Table(table) => {
                // Check for unsupported fields
                if table.version.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "version field is not supported, only path dependencies".into(),
                    });
                }
                if table.git.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "git dependencies are not supported, only path dependencies".into(),
                    });
                }
                if table.registry.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "registry dependencies are not supported".into(),
                    });
                }
                if table.features.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "features are not supported".into(),
                    });
                }
                if table.optional.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "optional dependencies are not supported".into(),
                    });
                }
                if table.default_features.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "default-features is not supported".into(),
                    });
                }
                if table.package.is_some() {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "package rename is not supported".into(),
                    });
                }

                // Extract path (required)
                let Some(path) = table.path else {
                    return Err(ManifestError::InvalidDependency {
                        name,
                        reason: "missing 'path' field, only path dependencies are supported".into(),
                    });
                };

                result.push(PathDependency {
                    name,
                    path: Utf8PathBuf::from(path),
                });
            }
        }
    }

    Ok(result)
}

/// Determine the library target from raw manifest data
fn determine_lib_target(
    package_name: &str,
    raw_lib: Option<RawLibTarget>,
    base_dir: Option<&Utf8Path>,
) -> Option<LibTarget> {
    // Check if src/lib.rs exists
    let default_lib_path = if let Some(base) = base_dir {
        base.join("src/lib.rs")
    } else {
        Utf8PathBuf::from("src/lib.rs")
    };

    if let Some(lib) = raw_lib {
        // Explicit [lib] section
        let path = if let Some(p) = lib.path {
            if let Some(base) = base_dir {
                base.join(&p)
            } else {
                Utf8PathBuf::from(p)
            }
        } else {
            default_lib_path
        };

        let name = lib.name.unwrap_or_else(|| package_name.replace('-', "_"));
        Some(LibTarget { name, path })
    } else if default_lib_path.exists() {
        // Auto-detect src/lib.rs
        Some(LibTarget {
            name: package_name.replace('-', "_"),
            path: default_lib_path,
        })
    } else {
        None
    }
}

/// Determine the binary target from raw manifest data
fn determine_bin_target(
    package_name: &str,
    raw_bins: Option<Vec<RawBinTarget>>,
    base_dir: Option<&Utf8Path>,
) -> Option<BinTarget> {
    // Check if src/main.rs exists
    let default_main_path = if let Some(base) = base_dir {
        base.join("src/main.rs")
    } else {
        Utf8PathBuf::from("src/main.rs")
    };

    if let Some(bins) = raw_bins {
        // Explicit [[bin]] section(s) - we only support one (validated earlier)
        if let Some(bin) = bins.into_iter().next() {
            let path = if let Some(p) = bin.path {
                if let Some(base) = base_dir {
                    base.join(&p)
                } else {
                    Utf8PathBuf::from(p)
                }
            } else {
                default_main_path
            };

            let name = bin.name.unwrap_or_else(|| package_name.to_string());
            return Some(BinTarget { name, path });
        }
    }

    // Auto-detect src/main.rs
    if default_main_path.exists() {
        Some(BinTarget {
            name: package_name.to_string(),
            path: default_main_path,
        })
    } else {
        None
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
        // This will fail because no src/main.rs exists
        // In real usage, base_dir points to actual directory
        let result = Manifest::from_str(toml, None);
        // Without base_dir, it can't check file existence, so it defaults to no targets
        assert!(result.is_err());
    }

    #[test]
    fn parse_lib_crate() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/lib.rs"), "pub fn hello() {}").unwrap();

        let toml = r#"
[package]
name = "mylib"
edition = "2021"
"#;
        let manifest = Manifest::from_str(toml, Some(&base)).unwrap();
        assert_eq!(manifest.name, "mylib");
        assert!(manifest.lib.is_some());
        assert!(manifest.bin.is_none());
        assert_eq!(manifest.lib.as_ref().unwrap().name, "mylib");
    }

    #[test]
    fn parse_bin_crate() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "mybin"
edition = "2021"
"#;
        let manifest = Manifest::from_str(toml, Some(&base)).unwrap();
        assert_eq!(manifest.name, "mybin");
        assert!(manifest.lib.is_none());
        assert!(manifest.bin.is_some());
        assert_eq!(manifest.bin.as_ref().unwrap().name, "mybin");
    }

    #[test]
    fn parse_path_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"
edition = "2021"

[dependencies]
util = { path = "../util" }
"#;
        let manifest = Manifest::from_str(toml, Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 1);
        assert_eq!(manifest.deps[0].name, "util");
        assert_eq!(manifest.deps[0].path, Utf8PathBuf::from("../util"));
    }

    #[test]
    fn parse_multiple_path_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"
edition = "2021"

[dependencies]
util = { path = "../util" }
common = { path = "../common" }
"#;
        let manifest = Manifest::from_str(toml, Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 2);
        // Note: order may vary since we're iterating a map
        let names: Vec<_> = manifest.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"util"));
        assert!(names.contains(&"common"));
    }

    #[test]
    fn reject_version_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
serde = "1.0"
"#;
        let err = Manifest::from_str(toml, Some(&base)).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidDependency { name, .. } if name == "serde"));
    }

    #[test]
    fn reject_git_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
foo = { git = "https://github.com/example/foo" }
"#;
        let err = Manifest::from_str(toml, Some(&base)).unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidDependency { name, reason }
            if name == "foo" && reason.contains("git"))
        );
    }

    #[test]
    fn reject_features_in_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
util = { path = "../util", features = ["foo"] }
"#;
        let err = Manifest::from_str(toml, Some(&base)).unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidDependency { name, reason }
            if name == "util" && reason.contains("features"))
        );
    }

    #[test]
    fn reject_optional_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
util = { path = "../util", optional = true }
"#;
        let err = Manifest::from_str(toml, Some(&base)).unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidDependency { name, reason }
            if name == "util" && reason.contains("optional"))
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
    fn reject_features_section() {
        let toml = r#"
[package]
name = "hello"

[features]
default = []
foo = []
"#;
        let err = Manifest::from_str(toml, None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[features]",
                ..
            }
        ));
    }

    #[test]
    fn crate_name_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/lib.rs"), "").unwrap();

        let toml = r#"
[package]
name = "my-lib-name"
edition = "2021"
"#;
        let manifest = Manifest::from_str(toml, Some(&base)).unwrap();
        assert_eq!(manifest.name, "my-lib-name");
        assert_eq!(manifest.crate_name(), "my_lib_name");
    }
}
