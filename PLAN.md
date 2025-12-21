# CAS-Owned Toolchain Acquisition - Implementation Plan (v4)

## Executive Summary

Make `vx-casd` the sole component that downloads, verifies, and stores toolchain artifacts. Daemon and execd communicate only with CAS—never hitting the network directly. Daemon does NOT materialize toolchains; execd materializes on-demand.

---

## CAS Invariants (Reference)

These invariants must hold across the entire system:

1. **Network Authority**: Only `vx-casd` touches the network for toolchain downloads
2. **Identity Semantics**: 
   - `SpecKey` = hash of *what was requested* (channel, host, target, components)
   - `ToolchainId` = hash of *what was fetched* (derived from upstream SHA256s)
   - `ManifestHash` = hash of the `ToolchainManifest` stored in CAS
3. **CAS Guarantees**:
   - Blobs are immutable and content-addressed
   - Manifests are immutable and content-addressed  
   - `spec → manifest_hash` mapping is first-writer-wins, atomic, durable
   - All toolchain data flows through CAS RPCs
4. **GC Policy**: Toolchains are GC'd like any other CAS content. Spec mappings and manifests are GC roots. Pruning a spec mapping makes the toolchain eligible for GC once no other references exist. (Full GC policy is out of scope for v1.)
5. **Plan Stability**: For a given `manifest_hash`, `get_materialization_plan` returns bit-for-bit identical plans across time and casd versions within a `layout_version`. If plan format changes, bump `MATERIALIZATION_LAYOUT_VERSION`.

---

## Phase 0: Enable Tracing (MUST DO FIRST)

We're flying blind without proper logging. Before any implementation work:

### Step 0.1: Add Default Tracing Level with Span Events

```rust
// crates/vx/src/main.rs

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};
    
    // Default to info for vx crates, warn for everything else
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new("warn,vx=info,vx_daemon=info,vx_casd=info,vx_toolchain=info")
        });
    
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        // Show span close events with duration - critical for toolchain downloads
        .with_span_events(FmtSpan::CLOSE)
        .init();
}
```

### Step 0.2: Add Structured Spans for Toolchain Operations

```rust
// In acquisition code:
#[tracing::instrument(skip(self), fields(spec_key = %spec.spec_key().short_hex()))]
async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
    // ...
}
```

---

## Phase 1: Protocol Layer (`vx-toolchain-proto`)

Create a new crate for protocol types. This keeps daemon/execd from depending on acquisition code.

### Step 1.1: Create Crate

```toml
# crates/vx-toolchain-proto/Cargo.toml
[package]
name = "vx-toolchain-proto"
version.workspace = true
edition.workspace = true

[dependencies]
facet.workspace = true
vx-cas-proto = { path = "../vx-cas-proto" }
blake3.workspace = true
```

### Step 1.2: Define Specs and Canonicalization

```rust
// crates/vx-toolchain-proto/src/lib.rs

use facet::Facet;
use vx_cas_proto::Blake3Hash;

pub type SpecKey = Blake3Hash;
pub type ToolchainId = Blake3Hash;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub enum RustChannel {
    Stable,
    Beta,
    Nightly { date: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub enum RustComponent {
    Rustc,
    RustStd,
    Cargo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct RustToolchainSpec {
    pub channel: RustChannel,
    pub host: String,
    pub target: String,
    pub components: Vec<RustComponent>,
}

impl RustToolchainSpec {
    /// Compute canonical SpecKey. 
    /// NOTE: No lowercasing - triples are already canonical. Just trim().
    pub fn spec_key(&self) -> Result<SpecKey, &'static str> {
        // Reject empty components early
        if self.components.is_empty() {
            return Err("components cannot be empty");
        }
        if self.host.trim().is_empty() {
            return Err("host cannot be empty");
        }
        if self.target.trim().is_empty() {
            return Err("target cannot be empty");
        }
        
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"toolchain-spec-v1\n");
        hasher.update(b"kind:rust\n");
        
        match &self.channel {
            RustChannel::Stable => hasher.update(b"channel:stable\n"),
            RustChannel::Beta => hasher.update(b"channel:beta\n"),
            RustChannel::Nightly { date } => {
                let date = date.trim();
                if date.is_empty() {
                    return Err("nightly date cannot be empty");
                }
                hasher.update(b"channel:nightly:");
                hasher.update(date.as_bytes());
                hasher.update(b"\n");
            }
        }
        
        hasher.update(b"host:");
        hasher.update(self.host.trim().as_bytes());
        hasher.update(b"\n");
        
        hasher.update(b"target:");
        hasher.update(self.target.trim().as_bytes());
        hasher.update(b"\n");
        
        // Sort components for determinism
        let mut components: Vec<_> = self.components.iter().collect();
        components.sort_by_key(|c| match c {
            RustComponent::Rustc => 0,
            RustComponent::RustStd => 1,
            RustComponent::Cargo => 2,
        });
        
        hasher.update(b"components:");
        for (i, c) in components.iter().enumerate() {
            if i > 0 { hasher.update(b","); }
            match c {
                RustComponent::Rustc => hasher.update(b"rustc"),
                RustComponent::RustStd => hasher.update(b"rust-std"),
                RustComponent::Cargo => hasher.update(b"cargo"),
            }
        }
        hasher.update(b"\n");
        
        Ok(Blake3Hash(*hasher.finalize().as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Facet)]
pub struct ZigToolchainSpec {
    pub version: String,
    pub host_platform: String,
}

impl ZigToolchainSpec {
    pub fn spec_key(&self) -> Result<SpecKey, &'static str> {
        if self.version.trim().is_empty() {
            return Err("version cannot be empty");
        }
        if self.host_platform.trim().is_empty() {
            return Err("host_platform cannot be empty");
        }
        
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"toolchain-spec-v1\n");
        hasher.update(b"kind:zig\n");
        hasher.update(b"version:");
        hasher.update(self.version.trim().as_bytes());
        hasher.update(b"\n");
        hasher.update(b"platform:");
        hasher.update(self.host_platform.trim().as_bytes());
        hasher.update(b"\n");
        Ok(Blake3Hash(*hasher.finalize().as_bytes()))
    }
}
```

### Step 1.3: Define ToolchainManifest (Stored via put_manifest)

**Key decision**: Store `ToolchainManifest` using the CAS `put_manifest` API (stored under `manifests/blake3/...`), not as a generic blob. This is semantically correct and enables future GC/introspection.

```rust
#[derive(Debug, Clone, Facet)]
pub struct ToolchainManifest {
    /// Schema version for format evolution
    pub schema_version: u32,
    /// Kind of toolchain
    pub kind: ToolchainKind,
    /// SpecKey that produced this manifest (for reverse lookup)
    pub spec_key: SpecKey,
    /// Content-derived toolchain identity
    pub toolchain_id: ToolchainId,
    /// When acquired (RFC3339)
    pub created_at: String,
    
    // === Rust-specific fields ===
    /// Resolved manifest date (for stable/beta this captures the actual version)
    pub rust_manifest_date: Option<String>,
    /// Rustc version string
    pub rust_version: Option<String>,
    
    // === Zig-specific fields ===
    pub zig_version: Option<String>,
    
    /// Components with their CAS blob references
    pub components: Vec<ToolchainComponentBlob>,
}

pub const TOOLCHAIN_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Facet)]
pub struct ToolchainComponentBlob {
    /// Component name: "rustc", "rust-std", "zig-exe", "zig-lib"
    pub name: String,
    /// Target triple (for rust-std)
    pub target: Option<String>,
    /// Compression format: "xz", "none", "tar"
    pub compression: String,
    /// CAS blob hash of the tarball/file bytes
    pub blob: Blake3Hash,
    /// Upstream SHA256 hex (for provenance/verification)
    pub sha256: String,
    /// Size in bytes
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub enum ToolchainKind {
    Rust,
    Zig,
}
```

### Step 1.4: Define MaterializationPlan

**Key decisions**:
- Use `strip_components: u32` instead of glob patterns
- CAS validates tarball structure and returns `strip_components` count (not dir name)
- Plan is purely derived from stored manifest, no heuristics
- `EnsureDir` with empty `relpath` is forbidden; use non-empty paths only

```rust
/// Materialization plan generated by CAS, executed by execd.
/// 
/// STABILITY CONTRACT: For a given manifest_hash, this plan is bit-for-bit
/// stable across time and casd versions within the same layout_version.
/// If plan generation changes, bump MATERIALIZATION_LAYOUT_VERSION.
#[derive(Debug, Clone, Facet)]
pub struct MaterializationPlan {
    pub toolchain_id: ToolchainId,
    /// Layout version - execd cache includes this to avoid corruption
    pub layout_version: u32,
    pub steps: Vec<MaterializeStep>,
}

pub const MATERIALIZATION_LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone, Facet)]
pub enum MaterializeStep {
    /// Ensure a directory exists (for explicit dir creation).
    /// relpath must be non-empty; use "." for toolchain root if needed.
    EnsureDir {
        relpath: String,
    },
    /// Extract a tar.xz blob
    ExtractTarXz {
        blob: Blake3Hash,
        /// Number of path components to strip (usually 1)
        strip_components: u32,
        /// Destination subdirectory relative to toolchain root
        dest_subdir: String,
    },
    /// Write a single file from a blob
    WriteFile {
        /// Relative path within toolchain root
        relpath: String,
        blob: Blake3Hash,
        /// Unix mode (e.g., 0o755 for executables)
        mode: u32,
    },
    /// Create a symlink (target must not escape toolchain root)
    Symlink {
        relpath: String,
        target: String,
    },
}
```

### Step 1.5: Define CAS Toolchain RPCs

```rust
#[derive(Debug, Clone, Facet)]
pub struct EnsureToolchainResult {
    /// None if spec was invalid and couldn't be hashed
    pub spec_key: Option<SpecKey>,
    /// None on failure
    pub toolchain_id: Option<ToolchainId>,
    /// Hash of the ToolchainManifest in CAS
    pub manifest_hash: Option<Blake3Hash>,
    pub status: EnsureStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub enum EnsureStatus {
    Hit,        // Already in CAS
    Downloaded, // Just acquired
    Failed,     // Acquisition failed
}

// Extend vx_cas_proto with toolchain methods
// (Can be same trait or separate CasToolchain trait on same server)

#[rapace::service]
pub trait CasToolchain {
    /// Ensure Rust toolchain exists in CAS. CAS downloads if needed.
    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult;
    
    /// Ensure Zig toolchain exists in CAS.
    async fn ensure_zig_toolchain(&self, spec: ZigToolchainSpec) -> EnsureToolchainResult;
    
    /// Get toolchain manifest by its hash
    async fn get_toolchain_manifest(&self, manifest_hash: Blake3Hash) -> Option<ToolchainManifest>;
    
    /// Lookup manifest hash by spec key
    async fn lookup_toolchain_spec(&self, spec_key: SpecKey) -> Option<Blake3Hash>;
    
    /// Get materialization plan for a toolchain
    async fn get_materialization_plan(&self, manifest_hash: Blake3Hash) -> Option<MaterializationPlan>;
    
    /// Stream blob chunks for extraction.
    /// 
    /// NOTE: This is chunked delivery (64KB Vec<u8> per chunk), not zero-copy.
    /// Each chunk involves allocation. Fine for v1 but don't assume this is
    /// free for multi-GB transfers.
    async fn stream_blob(&self, blob: Blake3Hash) -> Streaming<Vec<u8>>;
}
```

---

## Phase 2: CAS Storage Updates

### Step 2.1: Storage Layout

```
~/.vx/cas/
├── blobs/blake3/<hh>/<hash>           # All blobs (including toolchain tarballs)
├── manifests/blake3/<hh>/<hash>.json  # All manifests (including ToolchainManifest)
├── cache/blake3/<hh>/<hash>           # Build cache keys
└── toolchains/
    └── spec/<hh>/<speckey>            # Maps SpecKey → manifest_hash (hex string)
```

**Note**: ToolchainManifest is stored via `put_manifest` under `manifests/`. The `toolchains/spec/` directory only stores the `SpecKey → manifest_hash` mapping.

### Step 2.2: Atomic Spec Mapping with O_EXCL, fsync, and Directory Sync

The spec mapping is the only durable index. We must ensure crash safety.

```rust
impl CasService {
    fn toolchains_spec_dir(&self) -> Utf8PathBuf {
        self.root.join("toolchains/spec")
    }
    
    fn spec_path(&self, spec_key: &SpecKey) -> Utf8PathBuf {
        let hex = spec_key.to_hex();
        self.toolchains_spec_dir().join(&hex[..2]).join(&hex)
    }
    
    /// Publish spec → manifest_hash mapping atomically (first-writer-wins).
    /// Returns Ok(true) if we published, Ok(false) if already exists.
    /// 
    /// DURABILITY: Uses O_EXCL + fsync + directory sync for crash safety.
    fn publish_spec_mapping(&self, spec_key: &SpecKey, manifest_hash: &Blake3Hash) -> std::io::Result<bool> {
        use std::fs::{self, File, OpenOptions};
        use std::io::Write;
        
        let path = self.spec_path(spec_key);
        
        let parent = path.parent().expect("spec path has parent");
        fs::create_dir_all(parent)?;
        
        // Use O_EXCL (create_new) for atomic first-writer-wins
        match OpenOptions::new()
            .write(true)
            .create_new(true)  // O_EXCL - fails if exists
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(manifest_hash.to_hex().as_bytes())?;
                // Ensure data is on disk
                file.sync_all()?;
                
                // Sync parent directory to ensure the directory entry is durable
                // (required on some filesystems for crash safety)
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
                
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(false) // Another writer won
            }
            Err(e) => Err(e),
        }
    }
    
    /// Lookup manifest_hash by spec_key.
    /// 
    /// If the mapping file exists but is unreadable/corrupt, we treat it as
    /// a cache miss (re-acquire will fix it via first-writer-wins).
    fn lookup_spec(&self, spec_key: &SpecKey) -> Option<Blake3Hash> {
        let path = self.spec_path(spec_key);
        let content = std::fs::read_to_string(&path).ok()?;
        Blake3Hash::from_hex(content.trim())
    }
}
```

---

## Phase 3: Inflight Deduplication

### Step 3.1: ToolchainManager with Async Lookup (Remote CAS Reality)

**Key decisions**:
- Don't remove inflight entries (negligible memory, avoids race)
- `lookup_fn` must be async because CAS lookup is an RPC in production

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

type InflightFuture = Arc<tokio::sync::OnceCell<EnsureToolchainResult>>;

/// Manages in-flight toolchain acquisitions with deduplication.
/// 
/// Inflight entries are never removed. This is intentional:
/// - Memory cost is negligible (one OnceCell per unique SpecKey)
/// - Avoids race between CAS lookup miss and inflight insert
/// - Once CAS has the mapping, lookup_fn fast-paths and OnceCell is never awaited
pub struct ToolchainManager {
    inflight: Mutex<HashMap<SpecKey, InflightFuture>>,
}

impl ToolchainManager {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }
    
    /// Ensure a toolchain, deduplicating concurrent requests.
    /// 
    /// `lookup_fn` is async because CAS is remote in production.
    pub async fn ensure<L, LFut, A, AFut>(
        &self,
        spec_key: SpecKey,
        lookup_fn: L,
        acquire_fn: A,
    ) -> EnsureToolchainResult
    where
        L: Fn() -> LFut,
        LFut: std::future::Future<Output = Option<Blake3Hash>>,
        A: FnOnce() -> AFut,
        AFut: std::future::Future<Output = EnsureToolchainResult>,
    {
        // Fast path: check if already in CAS (async RPC)
        if let Some(manifest_hash) = lookup_fn().await {
            return EnsureToolchainResult {
                spec_key: Some(spec_key),
                toolchain_id: None, // Caller should read manifest for ID
                manifest_hash: Some(manifest_hash),
                status: EnsureStatus::Hit,
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
        
        // NOTE: We intentionally do NOT remove entries from inflight.
        // See struct doc comment for rationale.
    }
}
```

---

## Phase 4: Implement CasToolchain

### Step 4.1: Validate Tarball Structure (Returns strip_components)

Validation returns the number of components to strip, not the directory name (we don't use it).

```rust
/// Validate that a tarball has exactly one top-level directory.
/// Returns the number of components to strip (always 1 for valid tarballs).
/// 
/// PERF: This decompresses the entire tarball to validate structure.
/// Future optimization: validate during extraction in execd.
fn validate_tarball_structure(tarball_bytes: &[u8]) -> Result<u32, String> {
    use std::io::Read;
    use xz2::read::XzDecoder;
    
    let mut decoder = XzDecoder::new(tarball_bytes);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)
        .map_err(|e| format!("xz decompression failed: {}", e))?;
    
    let mut archive = tar::Archive::new(decompressed.as_slice());
    let mut top_level_dirs = std::collections::HashSet::new();
    
    for entry in archive.entries().map_err(|e| format!("tar error: {}", e))? {
        let entry = entry.map_err(|e| format!("tar entry error: {}", e))?;
        let path = entry.path().map_err(|e| format!("path error: {}", e))?;
        
        // Get the first component
        if let Some(first) = path.components().next() {
            if let std::path::Component::Normal(name) = first {
                top_level_dirs.insert(name.to_string_lossy().to_string());
            }
        }
    }
    
    if top_level_dirs.len() != 1 {
        return Err(format!(
            "tarball must have exactly one top-level directory, found {}: {:?}",
            top_level_dirs.len(),
            top_level_dirs
        ));
    }
    
    Ok(1) // strip_components = 1
}
```

### Step 4.2: Wire Up ensure_rust_toolchain

```rust
impl CasToolchain for CasService {
    async fn ensure_rust_toolchain(&self, spec: RustToolchainSpec) -> EnsureToolchainResult {
        // Validate and compute spec_key first
        let spec_key = match spec.spec_key() {
            Ok(k) => k,
            Err(e) => {
                return EnsureToolchainResult {
                    spec_key: None,  // Can't compute - no sentinel hash
                    toolchain_id: None,
                    manifest_hash: None,
                    status: EnsureStatus::Failed,
                    error: Some(format!("invalid spec: {}", e)),
                };
            }
        };
        
        self.toolchain_manager.ensure(
            spec_key,
            // Async lookup (CAS is remote in production)
            || async { self.lookup_toolchain_spec(spec_key).await },
            || async {
                // Convert proto spec to vx_toolchain types
                let channel = match &spec.channel {
                    RustChannel::Stable => vx_toolchain::Channel::Stable,
                    RustChannel::Beta => vx_toolchain::Channel::Beta,
                    RustChannel::Nightly { date } => vx_toolchain::Channel::Nightly { 
                        date: date.clone() 
                    },
                };
                
                let toolchain_spec = vx_toolchain::RustToolchainSpec {
                    channel,
                    host: spec.host.clone(),
                    target: if spec.target == spec.host { None } else { Some(spec.target.clone()) },
                };
                
                // Acquire (downloads, verifies, returns tarballs)
                let acquired = match vx_toolchain::acquire_rust_toolchain(&toolchain_spec).await {
                    Ok(a) => a,
                    Err(e) => {
                        return EnsureToolchainResult {
                            spec_key: Some(spec_key),
                            toolchain_id: None,
                            manifest_hash: None,
                            status: EnsureStatus::Failed,
                            error: Some(format!("{}", e)),
                        };
                    }
                };
                
                // Validate tarball structure (PERF: double decompression, see note)
                if let Err(e) = validate_tarball_structure(&acquired.rustc_tarball) {
                    return EnsureToolchainResult {
                        spec_key: Some(spec_key),
                        toolchain_id: None,
                        manifest_hash: None,
                        status: EnsureStatus::Failed,
                        error: Some(format!("rustc tarball invalid: {}", e)),
                    };
                }
                if let Err(e) = validate_tarball_structure(&acquired.rust_std_tarball) {
                    return EnsureToolchainResult {
                        spec_key: Some(spec_key),
                        toolchain_id: None,
                        manifest_hash: None,
                        status: EnsureStatus::Failed,
                        error: Some(format!("rust-std tarball invalid: {}", e)),
                    };
                }
                
                // Store component blobs
                let rustc_blob = self.put_blob(acquired.rustc_tarball.clone()).await;
                let rust_std_blob = self.put_blob(acquired.rust_std_tarball.clone()).await;
                
                // Build manifest
                let manifest = ToolchainManifest {
                    schema_version: TOOLCHAIN_MANIFEST_SCHEMA_VERSION,
                    kind: ToolchainKind::Rust,
                    spec_key,
                    toolchain_id: acquired.id.0.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
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
                
                // Store manifest using put_manifest (not put_blob!)
                let manifest_hash = self.put_toolchain_manifest(&manifest).await;
                
                // Publish spec → manifest_hash mapping (atomic, first-writer-wins)
                let _ = self.publish_spec_mapping(&spec_key, &manifest_hash);
                
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
                    status: EnsureStatus::Downloaded,
                    error: None,
                }
            },
        ).await
    }
    
    /// Store a ToolchainManifest using the manifest storage path.
    async fn put_toolchain_manifest(&self, manifest: &ToolchainManifest) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);
        
        if !path.exists() {
            let _ = self.atomic_write(&path, json.as_bytes());
        }
        
        hash
    }
    
    async fn get_materialization_plan(&self, manifest_hash: Blake3Hash) -> Option<MaterializationPlan> {
        let manifest = self.get_toolchain_manifest(manifest_hash).await?;
        
        // Pure function: manifest → plan (no heuristics, bit-for-bit stable)
        let steps = match manifest.kind {
            ToolchainKind::Rust => {
                let mut steps = vec![
                    MaterializeStep::EnsureDir { relpath: "sysroot".to_string() },
                ];
                steps.extend(manifest.components.iter().map(|c| {
                    MaterializeStep::ExtractTarXz {
                        blob: c.blob,
                        strip_components: 1, // Validated during acquisition
                        dest_subdir: "sysroot".to_string(),
                    }
                }));
                steps
            }
            ToolchainKind::Zig => {
                let mut steps = Vec::new();
                for c in &manifest.components {
                    match c.name.as_str() {
                        "zig-exe" => {
                            steps.push(MaterializeStep::WriteFile {
                                relpath: "zig".to_string(),
                                blob: c.blob,
                                mode: 0o755,
                            });
                        }
                        "zig-lib" => {
                            steps.push(MaterializeStep::EnsureDir { relpath: "lib".to_string() });
                            steps.push(MaterializeStep::ExtractTarXz {
                                blob: c.blob,
                                strip_components: 0,
                                dest_subdir: "lib".to_string(),
                            });
                        }
                        _ => {} // Unknown component, skip
                    }
                }
                steps
            }
        };
        
        Some(MaterializationPlan {
            toolchain_id: manifest.toolchain_id,
            layout_version: MATERIALIZATION_LAYOUT_VERSION,
            steps,
        })
    }
    
    /// Stream blob chunks for extraction without loading full blob in RAM.
    async fn stream_blob(&self, blob: Blake3Hash) -> Streaming<Vec<u8>> {
        use tokio::sync::mpsc;
        use tokio_stream::wrappers::ReceiverStream;
        
        let (tx, rx) = mpsc::channel(16);
        let blob_path = self.blob_path(&blob);
        
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            
            let Ok(file) = tokio::fs::File::open(&blob_path).await else {
                let _ = tx.send(Err(rapace_core::RpcError::internal("blob not found"))).await;
                return;
            };
            
            let mut reader = tokio::io::BufReader::new(file);
            let mut buf = vec![0u8; 16 * 1024 * 1024]; // 16MB chunks
            
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(rapace_core::RpcError::internal(format!("read error: {}", e)))).await;
                        break;
                    }
                }
            }
        });
        
        Box::pin(ReceiverStream::new(rx))
    }
}
```

---

## Phase 5: Update Daemon (Minimal Changes)

**Key decision**: Daemon does NOT materialize toolchains. It only:
1. Calls `ensure_*` to make sure toolchain exists in CAS
2. Records `toolchain_id` in cache keys and reports
3. Passes `toolchain_manifest` to execd in invocations

### Step 5.1: Simplify Daemon's ensure_rust_toolchain

```rust
impl DaemonService {
    /// Ensure Rust toolchain exists in CAS. Returns info for cache keys.
    /// Does NOT materialize - that's execd's job.
    pub async fn ensure_rust_toolchain(
        &self,
        channel: vx_toolchain::Channel,
    ) -> Result<ToolchainInfo, String> {
        let host = vx_toolchain::detect_host_triple()
            .map_err(|e| format!("failed to detect host: {}", e))?;
        
        let spec = RustToolchainSpec {
            channel: match channel {
                vx_toolchain::Channel::Stable => RustChannel::Stable,
                vx_toolchain::Channel::Beta => RustChannel::Beta,
                vx_toolchain::Channel::Nightly { date } => RustChannel::Nightly { date },
            },
            host: host.clone(),
            target: host.clone(),
            components: vec![RustComponent::Rustc, RustComponent::RustStd],
        };
        
        let result = self.cas.ensure_rust_toolchain(spec).await;
        
        if result.status == EnsureStatus::Failed {
            return Err(result.error.unwrap_or_else(|| "unknown error".into()));
        }
        
        let manifest_hash = result.manifest_hash
            .ok_or("ensure succeeded but no manifest_hash")?;
        
        // Get manifest to extract version info for reports
        let manifest = self.cas.get_toolchain_manifest(manifest_hash).await
            .ok_or("failed to get toolchain manifest")?;
        
        Ok(ToolchainInfo {
            toolchain_id: manifest.toolchain_id,
            manifest_hash,
            version: manifest.rust_version,
            manifest_date: manifest.rust_manifest_date,
        })
    }
}

/// Info returned from ensure, used for cache keys and reports.
/// NO paths - those are execd's concern.
pub struct ToolchainInfo {
    pub toolchain_id: Blake3Hash,
    pub manifest_hash: Blake3Hash,
    pub version: Option<String>,
    pub manifest_date: Option<String>,
}
```

---

## Phase 6: Update Exec Invocations and Materialization

### Step 6.1: Change RustcInvocation to Use ToolchainManifest

```rust
// crates/vx-exec-proto/src/lib.rs

#[derive(Debug, Clone, Facet)]
pub struct RustcInvocation {
    /// Toolchain manifest hash (execd fetches manifest, materializes, runs)
    pub toolchain_manifest: Blake3Hash,
    /// Arguments (excluding rustc binary itself)
    pub args: Vec<String>,
    /// Environment variables
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: String,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
}

#[derive(Debug, Clone, Facet)]
pub struct CcInvocation {
    /// Toolchain manifest hash
    pub toolchain_manifest: Blake3Hash,
    /// Arguments (including "cc", "-c", etc.)
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub expected_outputs: Vec<ExpectedOutput>,
    pub depfile_path: Option<String>,
    pub workspace_root: String,
}
```

### Step 6.2: Execd Materializes on Demand with Local Lock

**Key fixes**:
- Temp dir under same parent for atomic rename
- Local lock per (toolchain_id, layout_version) prevents concurrent extraction race
- Path traversal / symlink escape validation in extraction

```rust
impl ExecService {
    /// Ensure toolchain is materialized locally.
    /// 
    /// Uses a local lockfile to prevent concurrent materializations of the
    /// same toolchain on this worker.
    async fn ensure_toolchain_materialized(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
        let plan = self.cas.get_materialization_plan(manifest_hash).await
            .ok_or("toolchain manifest not found in CAS")?;
        
        // Cache dir includes layout_version to avoid corruption on format changes
        let toolchains_base = self.cache_dir.join("toolchains");
        let toolchain_dir = toolchains_base
            .join(format!("{}/v{}", plan.toolchain_id.short_hex(), plan.layout_version));
        
        // Check if already materialized (simple marker file check)
        let marker = toolchain_dir.join(".materialized");
        if marker.exists() {
            return Ok(toolchain_dir);
        }
        
        // Acquire local lock to prevent concurrent materializations
        let lock_path = toolchains_base
            .join(format!("{}/v{}/.lock", plan.toolchain_id.short_hex(), plan.layout_version));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _lock = acquire_file_lock(&lock_path)?;
        
        // Double-check after acquiring lock
        if marker.exists() {
            return Ok(toolchain_dir);
        }
        
        tracing::info!(
            toolchain_id = %plan.toolchain_id.short_hex(),
            "materializing toolchain"
        );
        
        // IMPORTANT: temp dir must be under same parent as final dir for atomic rename
        let tmp_dir = toolchains_base.join(".tmp").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
        
        // Execute plan, cleaning up on failure
        let result = self.execute_plan(&plan.steps, &tmp_dir).await;
        if let Err(e) = &result {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e.clone());
        }
        
        // Write marker and rename atomically
        std::fs::write(tmp_dir.join(".materialized"), "").map_err(|e| e.to_string())?;
        
        if let Some(parent) = toolchain_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp_dir, &toolchain_dir).map_err(|e| e.to_string())?;
        
        Ok(toolchain_dir)
    }
    
    async fn execute_plan(&self, steps: &[MaterializeStep], dest: &Utf8Path) -> Result<(), String> {
        for step in steps {
            self.execute_step(step, dest).await?;
        }
        Ok(())
    }
    
    async fn execute_step(&self, step: &MaterializeStep, dest: &Utf8Path) -> Result<(), String> {
        match step {
            MaterializeStep::EnsureDir { relpath } => {
                // Validate path is safe
                validate_safe_path(relpath)?;
                let path = dest.join(relpath);
                std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            }
            MaterializeStep::ExtractTarXz { blob, strip_components, dest_subdir } => {
                validate_safe_path(dest_subdir)?;
                let target_dir = dest.join(dest_subdir);
                std::fs::create_dir_all(&target_dir).map_err(|e| e.to_string())?;
                
                self.extract_streaming(*blob, *strip_components, &target_dir).await?;
            }
            MaterializeStep::WriteFile { relpath, blob, mode } => {
                validate_safe_path(relpath)?;
                let path = dest.join(relpath);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                
                let data = self.cas.get_blob(*blob).await
                    .ok_or_else(|| format!("blob {} not found", blob.short_hex()))?;
                
                std::fs::write(&path, &data).map_err(|e| e.to_string())?;
                
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode))
                        .map_err(|e| e.to_string())?;
                }
            }
            MaterializeStep::Symlink { relpath, target } => {
                validate_safe_path(relpath)?;
                validate_safe_symlink_target(target)?;
                
                let path = dest.join(relpath);
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &path).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    
    /// Extract tarball with true streaming - pipes async chunks through xz decoder.
    /// 
    /// Uses a sync adapter to bridge async Stream → sync Read for xz2/tar.
    async fn extract_streaming(
        &self,
        blob: Blake3Hash,
        strip_components: u32,
        dest: &Utf8Path,
    ) -> Result<(), String> {
        use futures::StreamExt;
        use std::io::Read;
        use xz2::read::XzDecoder;
        
        // Stream chunks from CAS
        let stream = self.cas.stream_blob(blob).await;
        
        // Bridge async stream to sync Read via a blocking adapter.
        // We spawn the extraction on a blocking thread since tar/xz2 are sync.
        let dest = dest.to_owned();
        
        tokio::task::spawn_blocking(move || {
            // Create a channel-based reader that receives chunks from the async stream
            let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(4);
            
            // Spawn a task to feed chunks into the channel
            let stream_handle = std::thread::spawn(move || {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    futures::pin_mut!(stream);
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|e| format!("stream error: {}", e));
                        if tx.send(chunk).is_err() {
                            break; // Receiver dropped
                        }
                    }
                });
            });
            
            // Create a Read adapter from the channel
            let reader = ChannelReader::new(rx);
            
            // Pipe through xz decoder
            let decoder = XzDecoder::new(reader);
            
            // Extract tar entries
            let mut archive = tar::Archive::new(decoder);
            
            for entry in archive.entries().map_err(|e| format!("tar error: {}", e))? {
                let mut entry = entry.map_err(|e| format!("tar entry error: {}", e))?;
                let path = entry.path().map_err(|e| format!("path error: {}", e))?;
                
                // SECURITY: Validate entry path before extraction
                validate_tar_entry_path(&path, strip_components)?;
                
                // Strip components
                let components: Vec<_> = path.components().collect();
                if components.len() <= strip_components as usize {
                    continue;
                }
                
                let stripped: std::path::PathBuf = components
                    .into_iter()
                    .skip(strip_components as usize)
                    .collect();
                
                let dest_path = dest.join(stripped.to_string_lossy().as_ref());
                
                // SECURITY: Verify final path is within dest
                let canonical_dest = dest.canonicalize_utf8().unwrap_or_else(|_| dest.to_owned());
                if let Ok(canonical_path) = dest_path.canonicalize_utf8() {
                    if !canonical_path.starts_with(&canonical_dest) {
                        return Err(format!("path escape detected: {:?}", dest_path));
                    }
                }
                
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                
                entry.unpack(&dest_path).map_err(|e| format!("unpack error: {}", e))?;
            }
            
            // Wait for stream thread to finish
            let _ = stream_handle.join();
            
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
    }
}

/// Adapter that implements std::io::Read from a sync channel of chunks.
/// This allows bridging async streams to sync readers without buffering everything.
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    current_chunk: Vec<u8>,
    offset: usize,
}

impl ChannelReader {
    fn new(rx: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>) -> Self {
        Self {
            rx,
            current_chunk: Vec::new(),
            offset: 0,
        }
    }
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // If we have data in current chunk, return it
            if self.offset < self.current_chunk.len() {
                let remaining = &self.current_chunk[self.offset..];
                let to_copy = std::cmp::min(remaining.len(), buf.len());
                buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
                self.offset += to_copy;
                return Ok(to_copy);
            }
            
            // Need more data - get next chunk
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.current_chunk = chunk;
                    self.offset = 0;
                    // Loop to read from new chunk
                }
                Ok(Err(e)) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                }
                Err(_) => {
                    // Channel closed, EOF
                    return Ok(0);
                }
            }
        }
    }
}

/// Validate a relative path is safe (no absolute paths, no .., stays within root)
fn validate_safe_path(path: &str) -> Result<(), String> {
    use std::path::Component;
    
    let p = std::path::Path::new(path);
    
    // Reject absolute paths
    if p.is_absolute() {
        return Err(format!("absolute path not allowed: {}", path));
    }
    
    // Reject paths with .. components
    for component in p.components() {
        match component {
            Component::ParentDir => {
                return Err(format!("parent directory (..) not allowed: {}", path));
            }
            Component::Prefix(_) => {
                return Err(format!("path prefix not allowed: {}", path));
            }
            _ => {}
        }
    }
    
    Ok(())
}

/// Validate symlink target doesn't escape
fn validate_safe_symlink_target(target: &str) -> Result<(), String> {
    // For now, reject any symlink with .. or absolute paths
    // In the future, we could allow relative symlinks that stay within the tree
    validate_safe_path(target)
}

/// Validate a tar entry path is safe
fn validate_tar_entry_path(path: &std::path::Path, strip_components: u32) -> Result<(), String> {
    use std::path::Component;
    
    // Reject absolute paths
    if path.is_absolute() {
        return Err(format!("tar entry has absolute path: {:?}", path));
    }
    
    // After stripping, check for escapes
    let components: Vec<_> = path.components().skip(strip_components as usize).collect();
    
    for component in &components {
        match component {
            Component::ParentDir => {
                return Err(format!("tar entry escapes with ..: {:?}", path));
            }
            _ => {}
        }
    }
    
    Ok(())
}

/// Acquire an exclusive file lock (blocking)
fn acquire_file_lock(path: &Utf8Path) -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;
    
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("failed to create lock file: {}", e))?;
    
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(format!("failed to acquire lock: {}", std::io::Error::last_os_error()));
        }
    }
    
    Ok(file)
}
```

---

## Phase 7: Update Zig Toolchain Handling

### Step 7.1: Fix Component Names

Use explicit names instead of inferring from `c.name == "zig"`:

```rust
// In ensure_zig_toolchain:
components: vec![
    ToolchainComponentBlob {
        name: "zig-exe".to_string(),  // Explicit name
        target: None,
        compression: "none".to_string(),
        blob: zig_exe_blob,
        sha256: zig_exe_sha256,
        size_bytes: zig_exe_contents.len() as u64,
    },
    ToolchainComponentBlob {
        name: "zig-lib".to_string(),  // Explicit name
        target: None,
        compression: "tar".to_string(), // Our internal tar format
        blob: lib_blob,
        sha256: lib_sha256,
        size_bytes: lib_tarball.len() as u64,
    },
],
```

---

## Testing Requirements

### Unit Tests

1. **Spec canonicalization stability**: Same spec → same SpecKey across runs, OS, etc.
2. **Spec validation**: Empty components, empty host, empty target → reject
3. **Inflight dedup**: 50 concurrent `ensure()` calls → exactly 1 download (mock HTTP)
4. **Publish race**: Two tasks complete and try publish → only one wins, both return same manifest_hash
5. **Plan determinism**: Same manifest → identical plan bytes
6. **Tarball validation**: Tarball with multiple top-level dirs → reject

### Security Tests

7. **Path traversal rejection**: Tar entries with `..` → reject
8. **Absolute path rejection**: Tar entries with `/etc/passwd` → reject
9. **Symlink escape rejection**: Symlinks pointing to `../../../etc` → reject

### Integration Tests

1. **Concurrent acquisition**: Two simulated daemons requesting same toolchain → single upstream fetch
2. **Concurrent materialization**: Two execd calls for same toolchain → one extracts, other waits and reuses
3. **Restart persistence**: Acquire, restart CasService, verify spec lookup returns same manifest_hash
4. **End-to-end**: Daemon requests toolchain via CAS, verify daemon never makes network calls, build works

---

## Migration Checklist

### Files to Create

| File | Purpose |
|------|---------|
| `crates/vx-toolchain-proto/Cargo.toml` | New crate |
| `crates/vx-toolchain-proto/src/lib.rs` | Specs, manifests, plans, RPCs |

### Files to Modify

| File | Changes |
|------|---------|
| `Cargo.toml` | Add vx-toolchain-proto to workspace |
| `crates/vx/src/main.rs` | Add default tracing filter with span events |
| `crates/vx-casd/Cargo.toml` | Add vx-toolchain, vx-toolchain-proto |
| `crates/vx-casd/src/lib.rs` | Add CasToolchain impl, ToolchainManager, put_toolchain_manifest |
| `crates/vx-daemon/src/lib.rs` | Remove materialization, use CAS for toolchains |
| `crates/vx-exec-proto/src/lib.rs` | Use toolchain_manifest instead of paths |
| `crates/vx-execd/src/lib.rs` | Add materialization with locking, security validation |
| `crates/vx-toolchain/src/lib.rs` | Keep acquisition logic, move manifest types to proto |

---

## Open Decisions (Resolved)

| Question | Decision |
|----------|----------|
| Separate vs extend Cas trait? | Separate `CasToolchain` trait, same rapace server |
| Materialization location? | Execd only in v1. Daemon does not materialize. |
| Store ToolchainManifest where? | Via `put_manifest` under `manifests/`, not as blob |
| Normalize triples? | No lowercasing, just `trim()` |
| Streaming for extraction? | Yes, use rapace `Streaming<Vec<u8>>` (chunked, not zero-copy) |
| Remove inflight entries? | No, keep forever (negligible memory, avoids race) |
| Crash durability? | fsync + directory sync on spec mapping writes |
| Invalid spec_key sentinel? | Use `Option<SpecKey>`, no magic zero-hash |

---

## Known Performance Costs (Documented for v1)

1. **Double decompression**: `validate_tarball_structure` decompresses, then execd decompresses again during extraction. Future: validate during extraction.

2. **Streaming is chunked, not zero-copy**: Each 16MB chunk is a `Vec<u8>` allocation. Fine for v1, but don't assume free for multi-GB.

3. **Thread-per-extraction**: `extract_streaming` uses `spawn_blocking` + a feeder thread to bridge async→sync. This is fine for toolchain extraction (infrequent, large files), but wouldn't scale to thousands of concurrent extractions.
