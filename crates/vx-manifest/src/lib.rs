//! Cargo.toml and Cargo.lock parsing for vx
//!
//! This crate provides:
//! - `Manifest`: Cargo.toml parsing using facet-cargo-toml with policy validation
//! - Lockfile and reachability analysis for Cargo.lock files
//!
//! ## Policy Validation
//!
//! While the parser accepts all valid Cargo.toml files, the `Manifest` type
//! enforces vx's current support level:
//! - Path dependencies and registry dependencies
//! - No git dependencies, features, optional deps, dev-deps/build-deps
//! - No build.rs, proc-macros, tests/benches/examples (for now)

pub mod lockfile;

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use facet_cargo_toml::Spanned;

use miette::{Diagnostic, NamedSource, SourceSpan};

/// Errors that can occur during manifest parsing
#[derive(Debug)]
pub enum ManifestError {
    ReadError {
        path: Utf8PathBuf,
        source: std::io::Error,
    },
    ParseError(String),
    MissingSection(&'static str),
    MissingField(&'static str),
    Unsupported {
        feature: &'static str,
        details: String,
    },
    NoTargets,
    InvalidDependency {
        name: String,
        reason: String,
        src: Option<NamedSource<Arc<String>>>,
        span: Option<SourceSpan>,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadError { path, .. } => write!(f, "failed to read {path}"),
            Self::ParseError(msg) => write!(f, "failed to parse Cargo.toml: {msg}"),
            Self::MissingSection(section) => write!(f, "missing required section: [{section}]"),
            Self::MissingField(field) => write!(f, "missing required field: [package].{field}"),
            Self::Unsupported { feature, details } => write!(f, "unsupported: {feature} (found {details})"),
            Self::NoTargets => write!(f, "no targets found (expected [lib] or [[bin]] with src/lib.rs or src/main.rs)"),
            Self::InvalidDependency { name, reason, .. } => write!(f, "invalid dependency '{name}': {reason}"),
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadError { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Diagnostic for ManifestError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        let code = match self {
            Self::ReadError { .. } => "vx_manifest::read_error",
            Self::ParseError(_) => "vx_manifest::parse_error",
            Self::MissingSection(_) => "vx_manifest::missing_section",
            Self::MissingField(_) => "vx_manifest::missing_field",
            Self::Unsupported { .. } => "vx_manifest::unsupported",
            Self::NoTargets => "vx_manifest::no_targets",
            Self::InvalidDependency { .. } => "vx_manifest::invalid_dependency",
        };
        Some(Box::new(code))
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        match self {
            Self::InvalidDependency { src: Some(src), .. } => Some(src as &dyn miette::SourceCode),
            _ => None,
        }
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        match self {
            Self::InvalidDependency { reason, span: Some(span), .. } => {
                Some(Box::new(std::iter::once(miette::LabeledSpan::new_with_span(
                    Some(reason.clone()),
                    *span,
                ))))
            }
            _ => None,
        }
    }
}

/// Edition of Rust to use
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, facet::Facet)]
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

impl From<facet_cargo_toml::Edition> for Edition {
    fn from(e: facet_cargo_toml::Edition) -> Self {
        match e {
            facet_cargo_toml::Edition::E2015 => Edition::E2015,
            facet_cargo_toml::Edition::E2018 => Edition::E2018,
            facet_cargo_toml::Edition::E2021 => Edition::E2021,
            facet_cargo_toml::Edition::E2024 => Edition::E2024,
        }
    }
}

/// A binary target
#[derive(Debug, Clone)]
pub struct BinTarget {
    /// Binary name (with source span for error reporting)
    pub name: Spanned<String>,
    /// Path to binary source (computed from manifest, no span)
    pub path: Utf8PathBuf,
}

/// A library target
#[derive(Debug, Clone)]
pub struct LibTarget {
    /// Library name (with source span for error reporting)
    pub name: Spanned<String>,
    /// Path to library source (computed from manifest, no span)
    pub path: Utf8PathBuf,
}

/// A path dependency
#[derive(Debug, Clone)]
pub struct PathDependency {
    /// Dependency name (used as extern crate name, with source span)
    pub name: Spanned<String>,
    /// Path to the dependency (with source span for "not found" errors)
    pub path: Spanned<String>,
    /// Features to enable (with source spans)
    pub features: Vec<Spanned<String>>,
}

/// A versioned (registry) dependency
#[derive(Debug, Clone)]
pub struct VersionDependency {
    /// Dependency name (used as extern crate name, with source span)
    pub name: Spanned<String>,
    /// Version requirement string (with source span for version resolution errors)
    pub version: Spanned<String>,
    /// Features to enable (with source spans)
    pub features: Vec<Spanned<String>>,
}

/// Parsed and validated manifest
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Source file (for pixel-perfect error reporting)
    pub source: Arc<NamedSource<String>>,
    /// Package name (with source span)
    pub name: Spanned<String>,
    /// Rust edition
    pub edition: Edition,
    /// Library target (if this crate has a lib.rs)
    pub lib: Option<LibTarget>,
    /// Binary target (if this crate has a main.rs or [[bin]])
    pub bin: Option<BinTarget>,
    /// Path dependencies (with source spans)
    pub deps: Vec<PathDependency>,
    /// Versioned (registry) dependencies (with source spans)
    pub version_deps: Vec<VersionDependency>,
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

    /// Returns true if this manifest has versioned (registry) dependencies.
    /// If true, a Cargo.lock file is required.
    pub fn has_version_deps(&self) -> bool {
        !self.version_deps.is_empty()
    }

    /// Parse a Cargo.toml file and validate it against current policy
    pub fn from_path(path: &Utf8Path) -> Result<Self, ManifestError> {
        let contents = std::fs::read_to_string(path).map_err(|e| ManifestError::ReadError {
            path: path.to_owned(),
            source: e,
        })?;

        // Create a NamedSource for error reporting
        let source = Arc::new(NamedSource::new(path.as_str(), contents.clone()));

        Self::parse(source, path.parent())
    }

    /// Parse Cargo.toml content with a base directory for resolving paths
    pub fn parse(source: Arc<NamedSource<String>>, base_dir: Option<&Utf8Path>) -> Result<Self, ManifestError> {
        let contents = source.inner();
        let cargo = facet_cargo_toml::CargoToml::parse(contents)
            .map_err(|e| ManifestError::ParseError(e.to_string()))?;

        // Extract package
        let package = cargo
            .package
            .as_ref()
            .ok_or(ManifestError::MissingSection("package"))?;

        // Validate unsupported features
        if let Some(ref dev_deps) = cargo.dev_dependencies
            && !dev_deps.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[dev-dependencies]",
                details: "dev-dependencies are not supported yet".into(),
            });
        }
        if let Some(ref build_deps) = cargo.build_dependencies
            && !build_deps.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[build-dependencies]",
                details: "build-dependencies are not supported yet".into(),
            });
        }
        if cargo.workspace.is_some() {
            return Err(ManifestError::Unsupported {
                feature: "[workspace]",
                details: "workspaces are not supported yet".into(),
            });
        }
        if let Some(ref features) = cargo.features
            && !features.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[features]",
                details: "features are not supported yet".into(),
            });
        }

        // Check for proc-macro crates
        if let Some(ref lib) = cargo.lib
            && lib.proc_macro.as_ref().map(|s| **s) == Some(true)
        {
            return Err(ManifestError::Unsupported {
                feature: "proc-macro crates",
                details: "[lib] proc-macro = true is not supported yet".into(),
            });
        }

        // Check for test/bench/example targets
        if let Some(ref tests) = cargo.test
            && !tests.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[test]] targets",
                details: "test targets are not supported yet".into(),
            });
        }
        if let Some(ref benches) = cargo.bench
            && !benches.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[bench]] targets",
                details: "bench targets are not supported yet".into(),
            });
        }
        if let Some(ref examples) = cargo.example
            && !examples.is_empty()
        {
            return Err(ManifestError::Unsupported {
                feature: "[[example]] targets",
                details: "example targets are not supported yet".into(),
            });
        }

        // Check for multiple binary targets
        if let Some(ref bins) = cargo.bin
            && bins.len() > 1
        {
            return Err(ManifestError::Unsupported {
                feature: "multiple [[bin]] targets",
                details: format!("found {} binary targets, only 1 is supported", bins.len()),
            });
        }

        // Check for build script
        if let Some(ref build) = package.build {
            let has_build = match build {
                facet_cargo_toml::StringOrBool::String(_) => true,
                facet_cargo_toml::StringOrBool::Bool(b) => **b,
            };
            if has_build {
                return Err(ManifestError::Unsupported {
                    feature: "build scripts",
                    details: "build = \"...\" is not supported yet".into(),
                });
            }
        }

        let name = package
            .name
            .clone()
            .ok_or(ManifestError::MissingField("name"))?;

        let edition = match &package.edition {
            Some(facet_cargo_toml::EditionOrWorkspace::Edition(e)) => Edition::from(**e),
            _ => Edition::default(),
        };

        // Parse dependencies
        let parsed_deps = parse_dependencies(&cargo, contents)?;
        let path_deps = parsed_deps.path_deps;
        let version_deps = parsed_deps.version_deps;

        // Determine library target
        let lib = determine_lib_target(&name, cargo.lib.as_ref(), base_dir);

        // Determine binary target
        let bin = determine_bin_target(&name, cargo.bin.as_ref(), base_dir);

        // Must have at least one target
        if lib.is_none() && bin.is_none() {
            return Err(ManifestError::NoTargets);
        }

        Ok(Manifest {
            source,
            name,
            edition,
            lib,
            bin,
            deps: path_deps,
            version_deps,
        })
    }
}

/// Parsed dependencies from Cargo.toml
struct ParsedDependencies {
    path_deps: Vec<PathDependency>,
    version_deps: Vec<VersionDependency>,
}

/// Parse [dependencies] table, extracting path and version dependencies
fn parse_dependencies(
    cargo: &facet_cargo_toml::CargoToml,
    source: &str,
) -> Result<ParsedDependencies, ManifestError> {
    let Some(ref deps) = cargo.dependencies else {
        return Ok(ParsedDependencies {
            path_deps: Vec::new(),
            version_deps: Vec::new(),
        });
    };

    // Helper to create a NamedSource for error reporting
    let make_source = || NamedSource::new("Cargo.toml", Arc::new(source.to_owned()));

    let mut path_deps = Vec::new();
    let mut version_deps = Vec::new();

    for (name, dep) in deps {
        match dep {
            facet_cargo_toml::Dependency::Version(version) => {
                // Simple version string: `foo = "1.0"`
                version_deps.push(VersionDependency {
                    name: Spanned::new(name.clone(), version.span()),
                    version: version.clone(),
                    features: Vec::<Spanned<String>>::new(),
                });
            }
            facet_cargo_toml::Dependency::Workspace(_) => {
                return Err(ManifestError::InvalidDependency {
                    name: name.clone(),
                    reason: "workspace dependencies are not supported".into(),
                    src: None,
                    span: None,
                });
            }
            facet_cargo_toml::Dependency::Detailed(detail) => {
                // Check for unsupported fields first - all with span information
                if let Some(ref git) = detail.git {
                    let span = git.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "git dependencies are not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }
                if let Some(ref registry) = detail.registry {
                    let span = registry.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "custom registries are not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }
                if let Some(ref registry_index) = detail.registry_index {
                    let span = registry_index.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "custom registry index is not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }

                if let Some(ref optional) = detail.optional
                    && *optional.value()
                {
                    let span = optional.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "optional dependencies are not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }
                if let Some(ref default_features) = detail.default_features {
                    let span = default_features.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "default-features is not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }
                if let Some(ref package) = detail.package {
                    let span = package.span();
                    return Err(ManifestError::InvalidDependency {
                        name: name.clone(),
                        reason: "package rename is not supported".into(),
                        src: Some(make_source()),
                        span: Some(SourceSpan::new(span.offset.into(), span.len)),
                    });
                }

                // Extract features if present (keep spans)
                // Note: Each feature string uses the span of the features array
                let features = detail
                    .features
                    .as_ref()
                    .map(|f_list| {
                        let span = f_list.span();
                        f_list.value.iter()
                            .map(|s| Spanned::new(s.clone(), span.clone()))
                            .collect()
                    })
                    .unwrap_or_default();

                // Determine dependency type: path or version
                // Note: path + version is valid (version used for crates.io, path for local dev)
                // When both are present, we prefer path for local builds (like Cargo does)
                match (&detail.path, &detail.version) {
                    (Some(path), _) => {
                        // Path dependency (possibly with version for crates.io publishing)
                        path_deps.push(PathDependency {
                            name: Spanned::new(name.clone(), path.span()),
                            path: path.clone(),
                            features,
                        });
                    }
                    (None, Some(version)) => {
                        // Version dependency: `foo = { version = "1.0" }`
                        version_deps.push(VersionDependency {
                            name: Spanned::new(name.clone(), version.span()),
                            version: version.clone(),
                            features,
                        });
                    }
                    (None, None) => {
                        return Err(ManifestError::InvalidDependency {
                            name: name.clone(),
                            reason: "missing 'path' or 'version' field".into(),
                            src: None,
                            span: None,
                        });
                    }
                }
            }
        }
    }

    Ok(ParsedDependencies {
        path_deps,
        version_deps,
    })
}

/// Determine the library target from parsed manifest data
fn determine_lib_target(
    package_name: &Spanned<String>,
    lib: Option<&facet_cargo_toml::LibTarget>,
    base_dir: Option<&Utf8Path>,
) -> Option<LibTarget> {
    if let Some(lib) = lib {
        // Explicit [lib] section - always trust it
        let path = if let Some(ref p) = lib.path {
            if let Some(base) = base_dir {
                base.join(&**p)
            } else {
                Utf8PathBuf::from(&**p)
            }
        } else if let Some(base) = base_dir {
            base.join("src/lib.rs")
        } else {
            Utf8PathBuf::from("src/lib.rs")
        };

        let name = lib
            .name
            .clone()
            .unwrap_or_else(|| {
                // Use package name with replaced hyphens, keep package name's span
                Spanned::new(package_name.value.replace('-', "_"), package_name.span())
            });
        Some(LibTarget { name, path })
    } else if let Some(base) = base_dir {
        // Auto-detect src/lib.rs only when we have a known base directory
        let lib_path = base.join("src/lib.rs");
        if lib_path.exists() {
            Some(LibTarget {
                name: Spanned::new(package_name.value.replace('-', "_"), package_name.span()),
                path: lib_path,
            })
        } else {
            None
        }
    } else {
        // No explicit [lib] and no base_dir - cannot auto-detect
        None
    }
}

/// Determine the binary target from parsed manifest data
fn determine_bin_target(
    package_name: &Spanned<String>,
    bins: Option<&Vec<facet_cargo_toml::BinTarget>>,
    base_dir: Option<&Utf8Path>,
) -> Option<BinTarget> {
    if let Some(bins) = bins {
        // Explicit [[bin]] section(s) - we only support one (validated earlier)
        if let Some(bin) = bins.first() {
            let path = if let Some(ref p) = bin.path {
                if let Some(base) = base_dir {
                    base.join(&**p)
                } else {
                    Utf8PathBuf::from(&**p)
                }
            } else if let Some(base) = base_dir {
                base.join("src/main.rs")
            } else {
                Utf8PathBuf::from("src/main.rs")
            };

            let name = bin.name.clone().unwrap_or_else(|| package_name.clone());
            return Some(BinTarget { name, path });
        }
    }

    // Auto-detect src/main.rs only when we have a known base directory
    if let Some(base) = base_dir {
        let main_path = base.join("src/main.rs");
        if main_path.exists() {
            Some(BinTarget {
                name: package_name.clone(),
                path: main_path,
            })
        } else {
            None
        }
    } else {
        // No explicit [[bin]] and no base_dir - cannot auto-detect
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a test source from TOML string
    fn test_source(toml: &str) -> Arc<NamedSource<String>> {
        Arc::new(NamedSource::new("Cargo.toml", toml.to_string()))
    }

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
[package]
name = "hello"
edition = "2021"
"#;
        // This will fail because no src/main.rs exists
        // In real usage, base_dir points to actual directory
        let result = Manifest::parse(test_source(toml), None);
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
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.name.value, "mylib");
        assert!(manifest.lib.is_some());
        assert!(manifest.bin.is_none());
        assert_eq!(manifest.lib.as_ref().unwrap().name.value, "mylib");
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
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.name.value, "mybin");
        assert!(manifest.lib.is_none());
        assert!(manifest.bin.is_some());
        assert_eq!(manifest.bin.as_ref().unwrap().name.value, "mybin");
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
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 1);
        assert_eq!(manifest.deps[0].name.value, "util");
        assert_eq!(manifest.deps[0].path.value, "../util");
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
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 2);
        // Note: order may vary since we're iterating a map
        let names: Vec<_> = manifest.deps.iter().map(|d| d.name.value.as_str()).collect();
        assert!(names.contains(&"util"));
        assert!(names.contains(&"common"));
    }

    #[test]
    fn parse_version_dependency_simple() {
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
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.version_deps.len(), 1);
        assert_eq!(manifest.version_deps[0].name.value, "serde");
        assert_eq!(manifest.version_deps[0].version.value, "1.0");
        assert!(manifest.has_version_deps());
    }

    #[test]
    fn parse_version_dependency_table() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1.0.197" }
"#;
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.version_deps.len(), 1);
        assert_eq!(manifest.version_deps[0].name.value, "serde");
        assert_eq!(manifest.version_deps[0].version.value, "1.0.197");
    }

    #[test]
    fn parse_mixed_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
serde = "1.0"
mylib = { path = "../mylib" }
"#;
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 1);
        assert_eq!(manifest.deps[0].name.value, "mylib");
        assert_eq!(manifest.version_deps.len(), 1);
        assert_eq!(manifest.version_deps[0].name.value, "serde");
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
        let err = Manifest::parse(test_source(toml), Some(&base)).unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidDependency { name, reason, .. }
            if name == "foo" && reason.contains("git"))
        );
    }

    #[test]
    fn parse_features_in_dependency() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();

        let toml = r#"
[package]
name = "app"

[dependencies]
util = { path = "../util", features = ["foo", "bar"] }
"#;
        let manifest = Manifest::parse(test_source(toml), Some(&base)).unwrap();
        assert_eq!(manifest.deps.len(), 1);
        assert_eq!(manifest.deps[0].name.value, "util");
        let features: Vec<&str> = manifest.deps[0].features.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(features, vec!["foo", "bar"]);
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
        let err = Manifest::parse(test_source(toml), Some(&base)).unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidDependency { name, reason, .. }
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
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
        let err = Manifest::parse(test_source(toml), None).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::Unsupported {
                feature: "[features]",
                ..
            }
        ));
    }
}
