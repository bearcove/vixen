//! Rust toolchain acquisition and management
//!
//! Downloads Rust toolchains from static.rust-lang.org without requiring rustup.

use facet::Facet;
use facet_value::Value;
use std::collections::HashMap;

use crate::{ToolchainError, detect_host_triple};
use vx_cas_proto::{Blake3Hash, RustChannel, RustComponent, RustToolchainSpec};

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
    manifest_version: Option<String>,
    date: Option<String>,
    pkg: Option<Value>,
}

#[derive(Facet, Debug)]
struct RawPackage {
    version: Option<String>,
    git_commit_hash: Option<String>,
    target: Option<Value>,
}

#[derive(Facet, Debug)]
struct RawTarget {
    available: Option<bool>,
    url: Option<String>,
    hash: Option<String>,
    xz_url: Option<String>,
    xz_hash: Option<String>,
    components: Option<Value>,
    extensions: Option<Value>,
}

impl ChannelManifest {
    pub fn from_toml(contents: &str) -> Result<Self, ToolchainError> {
        let doc: toml::Value = contents
            .parse()
            .map_err(|e| ToolchainError::ParseError(format!("TOML parse error: {}", e)))?;

        let date = doc
            .get("date")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolchainError::ParseError("missing 'date' field".into()))?
            .to_string();

        let pkg = doc
            .get("pkg")
            .and_then(|v| v.as_table())
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg' table".into()))?;

        let rustc = pkg
            .get("rustc")
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rustc'".into()))?;
        let rust_std = pkg
            .get("rust-std")
            .ok_or_else(|| ToolchainError::ParseError("missing 'pkg.rust-std'".into()))?;

        Ok(ChannelManifest {
            date,
            rustc: Self::parse_package(rustc)?,
            rust_std: Self::parse_package(rust_std)?,
        })
    }

    fn parse_package(pkg_value: &toml::Value) -> Result<PackageManifest, ToolchainError> {
        let pkg_table = pkg_value
            .as_table()
            .ok_or_else(|| ToolchainError::ParseError("package is not a table".into()))?;

        let version = pkg_table
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolchainError::ParseError("missing package version".into()))?
            .to_string();

        let git_commit_hash = pkg_table
            .get("git_commit_hash")
            .and_then(|v| v.as_str())
            .map(String::from);

        let target_table = pkg_table
            .get("target")
            .and_then(|v| v.as_table())
            .ok_or_else(|| ToolchainError::ParseError("missing 'target' table".into()))?;

        let mut targets = HashMap::new();
        for (target_name, target_value) in target_table {
            let target = Self::parse_target(target_value)?;
            targets.insert(target_name.clone(), target);
        }

        Ok(PackageManifest {
            version,
            git_commit_hash,
            targets,
        })
    }

    fn parse_target(target_value: &toml::Value) -> Result<TargetManifest, ToolchainError> {
        let target_table = target_value
            .as_table()
            .ok_or_else(|| ToolchainError::ParseError("target is not a table".into()))?;

        let available = target_table
            .get("available")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let xz_url = target_table
            .get("xz_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let url = target_table
            .get("url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let url = xz_url.or(url).unwrap_or_default();

        let xz_hash = target_table
            .get("xz_hash")
            .and_then(|v| v.as_str())
            .map(String::from);
        let hash = target_table
            .get("hash")
            .and_then(|v| v.as_str())
            .map(String::from);
        let hash = xz_hash.or(hash).unwrap_or_default();

        Ok(TargetManifest {
            available,
            url,
            hash,
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

/// Fetch a channel manifest from static.rust-lang.org
pub async fn fetch_channel_manifest(
    channel: &RustChannel,
) -> Result<ChannelManifest, ToolchainError> {
    let url = rust_channel_manifest_url(channel);

    tracing::debug!(url = %url, "fetching channel manifest");

    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(ToolchainError::FetchError {
            url: url.clone(),
            source: format!("HTTP {}", response.status()).into(),
        });
    }

    let body = response.text().await?;
    ChannelManifest::from_toml(&body)
}

/// Download a component with checksum verification
pub async fn download_component(url: &str, expected_hash: &str) -> Result<Vec<u8>, ToolchainError> {
    tracing::debug!(url = %url, "downloading component");

    let response = reqwest::get(url).await?;

    if !response.status().is_success() {
        return Err(ToolchainError::FetchError {
            url: url.to_string(),
            source: format!("HTTP {}", response.status()).into(),
        });
    }

    let bytes = response.bytes().await?;

    // Verify SHA256
    let actual_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    };

    if actual_hash != expected_hash {
        return Err(ToolchainError::ChecksumMismatch {
            url: url.to_string(),
            expected: expected_hash.to_string(),
            actual: actual_hash,
        });
    }

    tracing::debug!(
        url = %url,
        size = bytes.len(),
        hash = %actual_hash,
        "component downloaded and verified"
    );

    Ok(bytes.to_vec())
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

/// Result of toolchain acquisition
#[derive(Debug)]
pub struct AcquiredRustToolchain {
    pub id: RustToolchainId,
    pub manifest_date: String,
    pub rustc_version: String,
    pub rustc_tarball: Vec<u8>,
    pub rustc_manifest_sha256: String,
    pub rust_std_tarball: Vec<u8>,
    pub rust_std_manifest_sha256: String,
}

/// Acquire a Rust toolchain
pub async fn acquire_rust_toolchain(
    spec: &RustToolchainSpec,
) -> Result<AcquiredRustToolchain, ToolchainError> {
    let manifest = fetch_channel_manifest(&spec.channel).await?;

    let rustc_target = manifest.rustc_for_target(&spec.host)?;
    let rustc_manifest_sha256 = rustc_target.hash.clone();

    let target = &spec.target;
    let rust_std_target = manifest.rust_std_for_target(target)?;
    let rust_std_manifest_sha256 = rust_std_target.hash.clone();

    let toolchain_id = RustToolchainId({
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rust-toolchain-id-v1\n");
        hasher.update(b"rustc-sha256:");
        hasher.update(rustc_manifest_sha256.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"rust-std-sha256:");
        hasher.update(rust_std_manifest_sha256.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"host:");
        hasher.update(spec.host.as_bytes());
        hasher.update(b"\n");
        hasher.update(b"target:");
        hasher.update(target.as_bytes());
        hasher.update(b"\n");
        Blake3Hash(*hasher.finalize().as_bytes())
    });

    tracing::info!(
        toolchain_id = %toolchain_id.short_hex(),
        rustc = %manifest.rustc.version,
        date = %manifest.date,
        "downloading Rust toolchain"
    );

    let rustc_tarball = download_component(&rustc_target.url, &rustc_manifest_sha256).await?;
    let rust_std_tarball =
        download_component(&rust_std_target.url, &rust_std_manifest_sha256).await?;

    tracing::info!(
        toolchain_id = %toolchain_id.short_hex(),
        rustc_size = rustc_tarball.len(),
        rust_std_size = rust_std_tarball.len(),
        "Rust toolchain acquired"
    );

    Ok(AcquiredRustToolchain {
        id: toolchain_id,
        manifest_date: manifest.date.clone(),
        rustc_version: manifest.rustc.version.clone(),
        rustc_tarball,
        rustc_manifest_sha256,
        rust_std_tarball,
        rust_std_manifest_sha256,
    })
}
