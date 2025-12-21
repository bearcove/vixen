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
