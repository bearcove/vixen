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

    let manifest = ChannelManifest::from_toml(toml).unwrap();
    assert_eq!(manifest.date, "2025-12-11");
    assert_eq!(manifest.rustc.version, "1.76.0");

    let rustc_target = manifest
        .rustc_for_target("x86_64-unknown-linux-gnu")
        .unwrap();
    assert!(rustc_target.available);
    assert!(rustc_target.url.contains("rustc-1.76.0"));
}

#[tokio::test]
#[ignore] // Requires network access - run with --ignored
async fn fetch_stable_manifest() {
    let channel = Channel::Stable;
    let manifest = fetch_channel_manifest(&channel).await.unwrap();

    // Stable should have a recent date
    assert!(!manifest.date.is_empty());

    // Should have rustc version
    assert!(!manifest.rustc.version.is_empty());

    // Should have x86_64-unknown-linux-gnu target
    let rustc_target = manifest
        .rustc_for_target("x86_64-unknown-linux-gnu")
        .unwrap();
    assert!(rustc_target.available);
    assert!(!rustc_target.url.is_empty());
    assert!(!rustc_target.hash.is_empty());
}

// =============================================================================
// RUST TOOLCHAIN SPEC TESTS
// =============================================================================

#[test]
fn rust_toolchain_spec_effective_target() {
    let spec = RustToolchainSpec {
        channel: Channel::Stable,
        host: "x86_64-unknown-linux-gnu".to_string(),
        target: None,
    };
    assert_eq!(spec.effective_target(), "x86_64-unknown-linux-gnu");

    let cross_spec = RustToolchainSpec {
        channel: Channel::Stable,
        host: "x86_64-unknown-linux-gnu".to_string(),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
    };
    assert_eq!(cross_spec.effective_target(), "aarch64-unknown-linux-gnu");
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
    assert_eq!(id1.to_hex(), id2.to_hex());

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
fn rust_toolchain_id_display() {
    let host = "aarch64-apple-darwin";
    let target = "aarch64-apple-darwin";
    let rustc_sha256 = "abc123";
    let std_sha256 = "def456";

    let id = RustToolchainId::from_manifest_sha256s(host, target, rustc_sha256, std_sha256);
    let display = format!("{}", id);

    assert!(display.starts_with("rust:"));
    // Short hex is 16 chars
    assert_eq!(display.len(), 5 + 16); // "rust:" + 16 hex chars
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

#[tokio::test]
#[ignore] // Requires network access and takes a long time - run with --ignored
async fn acquire_and_materialize_rust_toolchain() {
    let host = detect_host_triple().unwrap();
    let spec = RustToolchainSpec {
        channel: Channel::Stable,
        host,
        target: None,
    };

    // Acquire toolchain
    let acquired = acquire_rust_toolchain(&spec).await.unwrap();

    println!("Toolchain ID: {}", acquired.id);
    println!("Rustc version: {}", acquired.rustc_version);
    println!("Manifest date: {}", acquired.manifest_date);

    // Materialize it
    let temp_dir = Utf8PathBuf::from("/tmp/vx-rust-toolchain-test");
    let _ = std::fs::remove_dir_all(&temp_dir);

    let materialized = materialize_rust_toolchain(
        &acquired.rustc_tarball,
        &acquired.rust_std_tarball,
        &temp_dir,
    )
    .unwrap();

    assert!(materialized.rustc.exists());
    assert!(materialized.sysroot.exists());

    // Test that rustc works
    let output = std::process::Command::new(&materialized.rustc)
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let version = String::from_utf8_lossy(&output.stdout);
    println!("Installed rustc version: {}", version);

    // Cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);
}
