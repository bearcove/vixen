use super::*;

#[test]
fn parse_channel_stable() {
    assert_eq!(parse_rust_channel("stable").unwrap(), RustChannel::Stable);
}

#[test]
fn parse_channel_beta() {
    assert_eq!(parse_rust_channel("beta").unwrap(), RustChannel::Beta);
}

#[test]
fn parse_channel_nightly() {
    assert_eq!(
        parse_rust_channel("nightly-2024-02-01").unwrap(),
        RustChannel::Nightly {
            date: "2024-02-01".to_string()
        }
    );
}

#[test]
fn reject_invalid_channel() {
    assert!(parse_rust_channel("invalid").is_err());
    assert!(parse_rust_channel("nightly-02-01").is_err());
    assert!(parse_rust_channel("nightly-2024-2-1").is_err());
}

#[test]
fn channel_urls() {
    assert_eq!(
        rust_channel_manifest_url(&RustChannel::Stable),
        "https://static.rust-lang.org/dist/channel-rust-stable.toml"
    );
    assert_eq!(
        rust_channel_manifest_url(&RustChannel::Nightly {
            date: "2024-02-01".to_string()
        }),
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

    let manifest = ChannelManifest::from_toml(toml).unwrap();
    assert_eq!(manifest.date, "2025-12-11");
    assert_eq!(manifest.rustc.version, "1.76.0");

    let rustc_target = manifest
        .rustc_for_target("x86_64-unknown-linux-gnu")
        .unwrap();
    assert!(rustc_target.available);
    assert!(rustc_target.url.contains("rustc-1.76.0"));
}

// =============================================================================
// RUST TOOLCHAIN SPEC TESTS
// =============================================================================

#[test]
fn rust_toolchain_spec_targets() {
    // Native compilation: host == target
    let spec = RustToolchainSpec {
        channel: RustChannel::Stable,
        host: "x86_64-unknown-linux-gnu".to_string(),
        target: "x86_64-unknown-linux-gnu".to_string(),
        components: vec![RustComponent::Rustc, RustComponent::RustStd],
    };
    assert_eq!(spec.host, spec.target);

    // Cross compilation: host != target
    let cross_spec = RustToolchainSpec {
        channel: RustChannel::Stable,
        host: "x86_64-unknown-linux-gnu".to_string(),
        target: "aarch64-unknown-linux-gnu".to_string(),
        components: vec![RustComponent::Rustc, RustComponent::RustStd],
    };
    assert_ne!(cross_spec.host, cross_spec.target);
}

#[test]
fn rust_toolchain_id_deterministic() {
    // Toolchain ID is derived from host/target triples + manifest SHA256 hashes
    let host = "aarch64-apple-darwin";
    let target = "aarch64-apple-darwin";
    let rustc_sha256 = "abc123def456";
    let std_sha256 = "789xyz000111";

    let id1 = RustToolchainId::from_manifest_sha256s(host, target, rustc_sha256, std_sha256);
    let id2 = RustToolchainId::from_manifest_sha256s(host, target, rustc_sha256, std_sha256);

    assert_eq!(id1, id2);
    assert_eq!(id1.0.to_hex(), id2.0.to_hex());

    // Different manifest hash = different ID
    let std_sha256_different = "different_hash";
    let id3 =
        RustToolchainId::from_manifest_sha256s(host, target, rustc_sha256, std_sha256_different);
    assert_ne!(id1, id3);

    // Different target = different ID (even with same hashes)
    let id4 = RustToolchainId::from_manifest_sha256s(
        host,
        "x86_64-unknown-linux-gnu",
        rustc_sha256,
        std_sha256,
    );
    assert_ne!(id1, id4);
}

#[test]
fn rust_toolchain_id_hex() {
    let host = "aarch64-apple-darwin";
    let target = "aarch64-apple-darwin";
    let rustc_sha256 = "abc123";
    let std_sha256 = "def456";

    let id = RustToolchainId::from_manifest_sha256s(host, target, rustc_sha256, std_sha256);
    let hex = id.0.to_hex();

    // Full hex is 64 chars (32 bytes)
    assert_eq!(hex.len(), 64);

    // Short hex is 16 chars
    let short = id.0.short_hex();
    assert_eq!(short.len(), 16);
}

#[test]
fn detect_host_triple_works() {
    // This should work on any dev machine with rustc installed
    let triple = detect_host_triple().unwrap();
    assert!(!triple.is_empty());
    // Should contain something like "linux", "darwin", "windows"
    assert!(
        triple.contains("linux") || triple.contains("darwin") || triple.contains("windows"),
        "unexpected triple: {}",
        triple
    );
}
