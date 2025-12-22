//! Toolchain Manager (Inflight Deduplication)

use jiff::Timestamp;
use std::collections::HashMap;
use std::sync::Arc;
use vx_cas_proto::*;
use vx_cas_proto::{
    EnsureStatus as ToolchainEnsureStatus, EnsureToolchainResult, RustChannel, RustToolchainSpec,
    TOOLCHAIN_MANIFEST_SCHEMA_VERSION, ToolchainComponentBlob, ToolchainKind, ToolchainManifest,
    ToolchainSpecKey, ZigToolchainSpec,
};

use crate::types::CasService;

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureToolchainResult>>;

/// Manages in-flight toolchain acquisitions with deduplication.
///
/// Inflight entries are never removed. This is intentional:
/// - Memory cost is negligible (one OnceCell per unique ToolchainSpecKey)
/// - Avoids race between CAS lookup miss and inflight insert
/// - Once CAS has the mapping, lookup_fn fast-paths and OnceCell is never awaited
pub(crate) struct ToolchainManager {
    inflight: tokio::sync::Mutex<HashMap<ToolchainSpecKey, InflightFuture>>,
}

impl ToolchainManager {
    pub(crate) fn new() -> Self {
        Self {
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ensure a toolchain, deduplicating concurrent requests.
    ///
    /// `lookup_fn` is async because CAS is remote in production.
    pub(crate) async fn ensure(
        &self,
        spec_key: ToolchainSpecKey,
        lookup_fn: impl AsyncFn() -> Option<Blake3Hash>,
        acquire_fn: impl AsyncFn() -> EnsureToolchainResult,
    ) -> EnsureToolchainResult {
        // Fast path: check if already in CAS (async RPC)
        if let Some(manifest_hash) = lookup_fn().await {
            return EnsureToolchainResult {
                spec_key: Some(spec_key),
                toolchain_id: None, // Caller should read manifest for ID
                manifest_hash: Some(manifest_hash),
                status: ToolchainEnsureStatus::Hit,
                error: None,
            };
        }

        // Get or create inflight entry
        let cell = {
            let mut inflight = self.inflight.lock().await;
            inflight
                .entry(spec_key)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        // Initialize if we're first, otherwise wait
        cell.get_or_init(|| acquire_fn()).await.clone()
    }
}

// =============================================================================
// Toolchain methods (Inherent methods, NOT part of Cas RPC trait)
// =============================================================================

impl CasService {
    /// Ensure a Rust toolchain exists in CAS (internal helper, not an RPC method).
    #[tracing::instrument(skip(self), fields(spec_key = tracing::field::Empty))]
    pub async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Validate and compute spec_key first
        let spec_key = match spec.spec_key() {
            Ok(k) => {
                tracing::Span::current().record("spec_key", k.short_hex());
                k
            }
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: None, // Can't compute - no sentinel hash
                    toolchain_id: None,
                    manifest_hash: None,
                    status: ToolchainEnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };

        let this = self.clone();
        let this2 = self.clone();

        self.toolchain_manager
            .ensure(
                spec_key,
                // Async lookup (CAS is remote in production)
                async move || this.lookup_spec(&spec_key).await,
                async move || {
                    // Convert proto spec to vx_toolchain types
                    let channel = match &spec.channel {
                        RustChannel::Stable => vx_toolchain::Channel::Stable,
                        RustChannel::Beta => vx_toolchain::Channel::Beta,
                        RustChannel::Nightly { date } => {
                            vx_toolchain::Channel::Nightly { date: date.clone() }
                        }
                    };

                    let toolchain_spec = vx_toolchain::RustToolchainSpec {
                        channel,
                        host: spec.host.clone(),
                        target: if spec.target == spec.host {
                            None
                        } else {
                            Some(spec.target.clone())
                        },
                    };

                    // Acquire (downloads, verifies, returns tarballs)
                    let acquired = match vx_toolchain::acquire_rust_toolchain(&toolchain_spec).await
                    {
                        Ok(a) => a,
                        Err(e) => {
                            return EnsureToolchainResult {
                                spec_key: Some(spec_key),
                                toolchain_id: None,
                                manifest_hash: None,
                                status: ToolchainEnsureStatus::Failed,
                                error: Some(format!("{}", e)),
                            };
                        }
                    };

                    // Store component blobs
                    let rustc_blob = this2.put_blob(acquired.rustc_tarball.clone()).await;
                    let rust_std_blob = this2.put_blob(acquired.rust_std_tarball.clone()).await;

                    // Build manifest
                    let manifest = ToolchainManifest {
                        schema_version: TOOLCHAIN_MANIFEST_SCHEMA_VERSION,
                        kind: ToolchainKind::Rust,
                        spec_key,
                        toolchain_id: acquired.id.0,
                        created_at: Timestamp::now().in_tz("UTC").unwrap().datetime(),
                        rust_manifest_date: Some(acquired.manifest_date.clone()),
                        rust_version: Some(acquired.rustc_version.clone()),
                        zig_version: None,
                        components: vec![
                            ToolchainComponentBlob {
                                name: "rustc".to_string(),
                                target: Some(spec.host.clone()),
                                compression: "xz".to_string(),
                                blob: rustc_blob,
                                sha256: acquired.rustc_manifest_sha256.clone(),
                                size_bytes: acquired.rustc_tarball.len() as u64,
                            },
                            ToolchainComponentBlob {
                                name: "rust-std".to_string(),
                                target: Some(spec.target.clone()),
                                compression: "xz".to_string(),
                                blob: rust_std_blob,
                                sha256: acquired.rust_std_manifest_sha256.clone(),
                                size_bytes: acquired.rust_std_tarball.len() as u64,
                            },
                        ],
                    };

                    // Store manifest using put_toolchain_manifest (not put_blob!)
                    let manifest_hash = this2.put_toolchain_manifest(&manifest).await;

                    // Publish spec → manifest_hash mapping (atomic, first-writer-wins)
                    let _ = this2.publish_spec_mapping(&spec_key, &manifest_hash);

                    tracing::info!(
                        spec_key = %spec_key.short_hex(),
                        toolchain_id = %acquired.id.short_hex(),
                        manifest_hash = %manifest_hash.short_hex(),
                        "stored Rust toolchain in CAS"
                    );

                    EnsureToolchainResult {
                        spec_key: Some(spec_key),
                        toolchain_id: Some(acquired.id.0),
                        manifest_hash: Some(manifest_hash),
                        status: ToolchainEnsureStatus::Downloaded,
                        error: None,
                    }
                },
            )
            .await
    }

    /// Ensure a Zig toolchain exists in CAS (internal helper, not an RPC method).
    pub async fn ensure_zig_toolchain(&self, _spec: ZigToolchainSpec) -> EnsureToolchainResult {
        // TODO: Implement Zig toolchain acquisition
        EnsureToolchainResult {
            spec_key: None,
            toolchain_id: None,
            manifest_hash: None,
            status: ToolchainEnsureStatus::Failed,
            error: Some("Zig toolchain acquisition not yet implemented".to_string()),
        }
    }
}
