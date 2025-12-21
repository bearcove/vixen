//! Rust toolchain management for vx
//!
//! Downloads and manages Rust toolchains from static.rust-lang.org
//! without depending on rustup. Toolchains are stored in CAS for
//! deduplication and remote execution.

use std::collections::HashMap;

use facet::Facet;
use facet_value::Value;
use thiserror::Error;

/// Errors that can occur during toolchain operations
#[derive(Debug, Error)]
pub enum ToolchainError {
    #[error("failed to fetch manifest from {url}: {source}")]
    FetchError {
        url: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to parse channel manifest: {0}")]
    ParseError(String),

    #[error("component {component} not available for target {target}")]
    ComponentNotAvailable { component: String, target: String },

    #[error("invalid channel specification: {0}")]
    InvalidChannel(String),
}

/// Rust channel specification (what the user specifies)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Stable channel (latest stable)
    Stable,
    /// Beta channel
    Beta,
    /// Nightly channel with date pin (e.g., "2024-02-01")
    Nightly { date: String },
}

impl Channel {
    /// Parse a channel string like "stable", "beta", or "nightly-2024-02-01"
    pub fn parse(s: &str) -> Result<Self, ToolchainError> {
        match s {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            s if s.starts_with("nightly-") => {
                let date = s.strip_prefix("nightly-").unwrap();
                // Validate date format (YYYY-MM-DD)
                if date.len() != 10
                    || !date
                        .chars()
                        .enumerate()
                        .all(|(i, c)| (i == 4 || i == 7 && c == '-') || c.is_ascii_digit())
                {
                    return Err(ToolchainError::InvalidChannel(format!(
                        "nightly date must be YYYY-MM-DD, got: {}",
                        date
                    )));
                }
                Ok(Channel::Nightly {
                    date: date.to_string(),
                })
            }
            _ => Err(ToolchainError::InvalidChannel(format!(
                "unknown channel: {}",
                s
            ))),
        }
    }

    /// Get the manifest URL for this channel
    pub fn manifest_url(&self) -> String {
        match self {
            Channel::Stable => {
                "https://static.rust-lang.org/dist/channel-rust-stable.toml".to_string()
            }
            Channel::Beta => "https://static.rust-lang.org/dist/channel-rust-beta.toml".to_string(),
            Channel::Nightly { date } => {
                format!(
                    "https://static.rust-lang.org/dist/{}/channel-rust-nightly.toml",
                    date
                )
            }
        }
    }
}

/// Parsed channel manifest from static.rust-lang.org
#[derive(Debug, Clone)]
pub struct ChannelManifest {
    /// Release date (e.g., "2025-12-11")
    pub date: String,
    /// Rustc package metadata
    pub rustc: PackageManifest,
    /// Rust-std package metadata
    pub rust_std: PackageManifest,
}

/// Package metadata from the channel manifest
#[derive(Debug, Clone)]
pub struct PackageManifest {
    /// Package version (e.g., "1.76.0")
    pub version: String,
    /// Git commit hash
    pub git_commit_hash: Option<String>,
    /// Per-target download information
    pub targets: HashMap<String, TargetManifest>,
}

/// Per-target download information
#[derive(Debug, Clone)]
pub struct TargetManifest {
    /// Whether this target is available
    pub available: bool,
    /// Download URL (xz-compressed tarball)
    pub url: String,
    /// SHA256 hash of the tarball
    pub hash: String,
}

// =============================================================================
// RAW TOML STRUCTURES (for facet-toml parsing)
// =============================================================================

/// Raw channel manifest structure
#[derive(Facet, Debug)]
#[facet(rename_all = "kebab-case")]
struct RawChannelManifest {
    manifest_version: Option<String>,
    date: Option<String>,
    pkg: Option<RawPackages>,
}

/// Package table
#[derive(Facet, Debug)]
struct RawPackages {
    rustc: Option<RawPackage>,
    #[facet(rename = "rust-std")]
    rust_std: Option<RawPackage>,
}

/// Individual package entry
#[derive(Facet, Debug)]
struct RawPackage {
    version: Option<String>,
    git_commit_hash: Option<String>,
    target: Option<Value>,
}

/// Target-specific download info
#[derive(Facet, Debug)]
struct RawTarget {
    available: Option<bool>,
    url: Option<String>,
    hash: Option<String>,
    xz_url: Option<String>,
    xz_hash: Option<String>,
}

impl ChannelManifest {
    /// Parse a channel manifest from TOML content
    pub fn from_str(contents: &str) -> Result<Self, ToolchainError> {
        let raw: RawChannelManifest = facet_toml::from_str(contents)
            .map_err(|e| ToolchainError::ParseError(e.to_string()))?;

        let date = raw
            .date
            .ok_or_else(|| ToolchainError::ParseError("missing 'date' field".into()))?;

        let pkg = raw
            .pkg
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg' table".into()))?;

        let rustc = pkg
            .rustc
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rustc'".into()))?;

        let rust_std = pkg
            .rust_std
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rust-std'".into()))?;

        Ok(ChannelManifest {
            date,
            rustc: parse_package(rustc)?,
            rust_std: parse_package(rust_std)?,
        })
    }

    /// Get the download info for rustc for a specific target
    pub fn rustc_for_target(&self, target: &str) -> Result<&TargetManifest, ToolchainError> {
        self.rustc
            .targets
            .get(target)
            .filter(|t| t.available)
            .ok_or_else(|| ToolchainError::ComponentNotAvailable {
                component: "rustc".to_string(),
                target: target.to_string(),
            })
    }

    /// Get the download info for rust-std for a specific target
    pub fn rust_std_for_target(&self, target: &str) -> Result<&TargetManifest, ToolchainError> {
        self.rust_std
            .targets
            .get(target)
            .filter(|t| t.available)
            .ok_or_else(|| ToolchainError::ComponentNotAvailable {
                component: "rust-std".to_string(),
                target: target.to_string(),
            })
    }
}

fn parse_package(raw: RawPackage) -> Result<PackageManifest, ToolchainError> {
    let version = raw
        .version
        .ok_or_else(|| ToolchainError::ParseError("missing 'version' in package".into()))?;

    let mut targets = HashMap::new();

    if let Some(target_value) = raw.target {
        // The target field is an object/table containing platform triples as keys
        if let Some(obj) = target_value.as_object() {
            for (name, value) in obj.iter() {
                // Convert the Value back to RawTarget
                let raw_target: RawTarget =
                    facet_value::from_value(value.clone()).map_err(|e| {
                        ToolchainError::ParseError(format!(
                            "failed to parse target {}: {}",
                            name, e
                        ))
                    })?;

                let target = TargetManifest {
                    available: raw_target.available.unwrap_or(false),
                    // Prefer xz-compressed URLs
                    url: raw_target.xz_url.or(raw_target.url).unwrap_or_default(),
                    hash: raw_target.xz_hash.or(raw_target.hash).unwrap_or_default(),
                };
                targets.insert(name.to_string(), target);
            }
        }
    }

    Ok(PackageManifest {
        version,
        git_commit_hash: raw.git_commit_hash,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_stable() {
        assert_eq!(Channel::parse("stable").unwrap(), Channel::Stable);
    }

    #[test]
    fn parse_channel_beta() {
        assert_eq!(Channel::parse("beta").unwrap(), Channel::Beta);
    }

    #[test]
    fn parse_channel_nightly() {
        assert_eq!(
            Channel::parse("nightly-2024-02-01").unwrap(),
            Channel::Nightly {
                date: "2024-02-01".to_string()
            }
        );
    }

    #[test]
    fn reject_invalid_channel() {
        assert!(Channel::parse("invalid").is_err());
        assert!(Channel::parse("nightly-02-01").is_err());
        assert!(Channel::parse("nightly-2024-2-1").is_err());
    }

    #[test]
    fn channel_urls() {
        assert_eq!(
            Channel::Stable.manifest_url(),
            "https://static.rust-lang.org/dist/channel-rust-stable.toml"
        );
        assert_eq!(
            Channel::Nightly {
                date: "2024-02-01".to_string()
            }
            .manifest_url(),
            "https://static.rust-lang.org/dist/2024-02-01/channel-rust-nightly.toml"
        );
    }

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
manifest-version = "2"
date = "2025-12-11"

[pkg.rustc]
version = "1.76.0"

[pkg.rustc.target.x86_64-unknown-linux-gnu]
available = true
xz_url = "https://static.rust-lang.org/dist/rustc-1.76.0-x86_64-unknown-linux-gnu.tar.xz"
xz_hash = "abc123"

[pkg.rust-std]
version = "1.76.0"

[pkg.rust-std.target.x86_64-unknown-linux-gnu]
available = true
xz_url = "https://static.rust-lang.org/dist/rust-std-1.76.0-x86_64-unknown-linux-gnu.tar.xz"
xz_hash = "def456"
"#;

        let manifest = ChannelManifest::from_str(toml).unwrap();
        assert_eq!(manifest.date, "2025-12-11");
        assert_eq!(manifest.rustc.version, "1.76.0");

        let rustc_target = manifest
            .rustc_for_target("x86_64-unknown-linux-gnu")
            .unwrap();
        assert!(rustc_target.available);
        assert!(rustc_target.url.contains("rustc-1.76.0"));
    }
}
