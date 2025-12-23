//! Rust toolchain acquisition and management
//!
//! Downloads Rust toolchains from static.rust-lang.org without requiring rustup.

use facet::Facet;
use std::collections::HashMap;

use crate::{ToolchainError, detect_host_triple};
use vx_oort_proto::{Blake3Hash, RustChannel, RustComponent, RustToolchainSpec};

/// Parse a channel string like "stable", "beta", or "nightly-2024-02-01"
pub fn parse_rust_channel(s: &str) -> Result<RustChannel, ToolchainError> {
    match s {
        "stable" => Ok(RustChannel::Stable),
        "beta" => Ok(RustChannel::Beta),
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
            Ok(RustChannel::Nightly {
                date: date.to_string(),
            })
        }
        _ => Err(ToolchainError::InvalidChannel(format!(
            "unknown channel: {}",
            s
        ))),
    }
}

/// Get the manifest URL for a channel
pub fn rust_channel_manifest_url(channel: &RustChannel) -> String {
    match channel {
        RustChannel::Stable => {
            "https://static.rust-lang.org/dist/channel-rust-stable.toml".to_string()
        }
        RustChannel::Beta => "https://static.rust-lang.org/dist/channel-rust-beta.toml".to_string(),
        RustChannel::Nightly { date } => {
            format!(
                "https://static.rust-lang.org/dist/{}/channel-rust-nightly.toml",
                date
            )
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

// Raw TOML structures for parsing
#[derive(Facet, Debug)]
#[facet(rename_all = "kebab-case")]
struct RawChannelManifest {
    date: String,
    pkg: RawPkgTable,
}

#[derive(Facet, Debug)]
struct RawPkgTable {
    rustc: RawPackage,
    #[facet(rename = "rust-std")]
    rust_std: RawPackage,
}

#[derive(Facet, Debug)]
struct RawPackage {
    version: String,
    git_commit_hash: Option<String>,
    target: HashMap<String, RawTarget>,
}

#[derive(Facet, Debug)]
struct RawTarget {
    #[facet(default)]
    available: bool,
    url: Option<String>,
    hash: Option<String>,
    xz_url: Option<String>,
    xz_hash: Option<String>,
}

impl ChannelManifest {
    pub fn from_toml(contents: &str) -> Result<Self, ToolchainError> {
        let raw: RawChannelManifest = facet_toml::from_str(contents)
            .map_err(|e| ToolchainError::ParseError(format!("TOML parse error: {}", e)))?;

        Ok(ChannelManifest {
            date: raw.date,
            rustc: raw.pkg.rustc.into(),
            rust_std: raw.pkg.rust_std.into(),
        })
    }

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

impl From<RawPackage> for PackageManifest {
    fn from(raw: RawPackage) -> Self {
        let targets = raw
            .target
            .into_iter()
            .map(|(name, raw_target)| (name, raw_target.into()))
            .collect();

        PackageManifest {
            version: raw.version,
            git_commit_hash: raw.git_commit_hash,
            targets,
        }
    }
}

impl From<RawTarget> for TargetManifest {
    fn from(raw: RawTarget) -> Self {
        // Prefer xz_url over url, and xz_hash over hash
        let url = raw.xz_url.or(raw.url).unwrap_or_default();
        let hash = raw.xz_hash.or(raw.hash).unwrap_or_default();

        TargetManifest {
            available: raw.available,
            url,
            hash,
        }
    }
}

/// Create a RustToolchainSpec for native compilation on the current host
pub fn rust_toolchain_spec_native(
    channel: RustChannel,
) -> Result<RustToolchainSpec, ToolchainError> {
    let host = detect_host_triple()?;
    Ok(RustToolchainSpec {
        channel,
        host: host.clone(),
        target: host,
        components: vec![RustComponent::Rustc, RustComponent::RustStd],
    })
}

/// Create a RustToolchainSpec for cross-compilation
pub fn rust_toolchain_spec_cross(
    channel: RustChannel,
    host: String,
    target: String,
    components: Vec<RustComponent>,
) -> RustToolchainSpec {
    RustToolchainSpec {
        channel,
        host,
        target,
        components,
    }
}

/// Unique identifier for a Rust toolchain
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustToolchainId(pub Blake3Hash);

impl RustToolchainId {
    /// Create from manifest SHA256 hashes and target triples.
    ///
    /// Both hashes are the SHA256 hex strings from the channel manifest.
    /// Host and target triples are included to avoid ID collisions across
    /// different host/target combinations.
    pub fn from_manifest_sha256s(
        host_triple: &str,
        target_triple: &str,
        rustc_sha256: &str,
        rust_std_sha256: &str,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rust-toolchain-v2\n");
        hasher.update(b"host:");
        hasher.update(host_triple.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"target:");
        hasher.update(target_triple.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"rustc_sha256:");
        hasher.update(rustc_sha256.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"rust_std_sha256:");
        hasher.update(rust_std_sha256.as_bytes());
        hasher.update(b"\n");
        Self(Blake3Hash(*hasher.finalize().as_bytes()))
    }

    pub fn short_hex(&self) -> String {
        self.0.short_hex()
    }
}
