//! vx-casd - Content-Addressed Storage daemon
//!
//! Provides CAS, toolchain, and registry services over TCP using rapace.

pub(crate) mod hash_reader;
pub(crate) mod http;
pub(crate) mod registry;
pub(crate) mod service;
pub(crate) mod toolchain;
pub(crate) mod types;

use crate::registry::RegistryManager;
use crate::toolchain::ToolchainManager;
use crate::types::{CasService, CasServiceInner};
use camino::Utf8PathBuf;
use eyre::Result;
use std::sync::Arc;
use tokio::net::TcpListener;
use vx_cas_proto::CasServer;
use vx_cas_proto::{
    Blake3Hash, BlobHash, CacheKey, ManifestHash, ToolchainManifest, ToolchainSpecKey,
};
use vx_io::atomic_write;

#[derive(Debug)]
struct Args {
    /// Storage root directory
    root: Utf8PathBuf,

    /// Bind address (host:port)
    bind: String,
}

impl Args {
    fn from_env() -> Result<Self> {
        let vx_home = std::env::var("VX_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{}/.vx", home)
        });
        let root = format!("{}/cas", vx_home);

        let bind_raw = std::env::var("VX_CAS").unwrap_or_else(|_| "127.0.0.1:9002".to_string());
        let bind = vx_io::net::normalize_tcp_endpoint(&bind_raw)?;

        Ok(Args {
            root: Utf8PathBuf::from(root),
            bind,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // If spawned by parent, die when parent dies
    ur_taking_me_with_you::die_with_parent();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_casd=info")),
        )
        .init();

    let args = Args::from_env()?;

    // Initialize CAS service
    tracing::info!("Initializing CAS at {}", args.root);
    let cas = CasService::new(args.root);
    cas.init().await?;

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("CAS listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let cas = cas.clone();

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            // Create transport from TCP stream
            let transport = rapace::Transport::stream(socket);

            // Serve the Cas service
            // Note: CasService implements the Cas trait, which is the only rapace service
            // Toolchain and registry are internal implementation details, not separate services
            let server = CasServer::new(cas);
            if let Err(e) = server.serve(transport).await {
                tracing::warn!("Connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}

impl CasService {
    fn new(root: Utf8PathBuf) -> Self {
        Self {
            inner: Arc::new(CasServiceInner {
                root,
                toolchain_manager: ToolchainManager::new(),
                registry_manager: RegistryManager::new(),
                download_semaphore: Arc::new(tokio::sync::Semaphore::new(32)),
            }),
        }
    }

    fn blobs_dir(&self) -> Utf8PathBuf {
        self.root.join("blobs/blake3")
    }

    fn manifests_dir(&self) -> Utf8PathBuf {
        self.root.join("manifests/blake3")
    }

    fn cache_dir(&self) -> Utf8PathBuf {
        self.root.join("cache/blake3")
    }

    fn tmp_dir(&self) -> Utf8PathBuf {
        self.root.join("tmp")
    }

    fn blob_path(&self, hash: &BlobHash) -> Utf8PathBuf {
        let hex = hash.to_hex();
        self.blobs_dir().join(&hex[..2]).join(&hex)
    }

    fn manifest_path(&self, hash: &ManifestHash) -> Utf8PathBuf {
        let hex = hash.to_hex();
        self.manifests_dir()
            .join(&hex[..2])
            .join(format!("{}.json", hex))
    }

    fn tree_manifests_dir(&self) -> Utf8PathBuf {
        self.root.join("trees/blake3")
    }

    fn tree_manifest_path(&self, hash: &ManifestHash) -> Utf8PathBuf {
        let hex = hash.to_hex();
        self.tree_manifests_dir()
            .join(&hex[..2])
            .join(format!("{}.json", hex))
    }

    fn cache_path(&self, key: &CacheKey) -> Utf8PathBuf {
        let hex = key.to_hex();
        self.cache_dir().join(&hex[..2]).join(&hex)
    }

    fn toolchains_spec_dir(&self) -> Utf8PathBuf {
        self.root.join("toolchains/spec")
    }

    fn spec_path(&self, spec_key: &ToolchainSpecKey) -> Utf8PathBuf {
        let hex = spec_key.to_hex();
        self.toolchains_spec_dir().join(&hex[..2]).join(&hex)
    }

    /// Publish spec → manifest_hash mapping atomically (first-writer-wins).
    /// Returns Ok(true) if we published, Ok(false) if already exists.
    ///
    /// DURABILITY: Uses O_EXCL + fsync + directory sync for crash safety.
    async fn publish_spec_mapping(
        &self,
        spec_key: &ToolchainSpecKey,
        manifest_hash: &Blake3Hash,
    ) -> std::io::Result<bool> {
        let path = self.spec_path(spec_key);

        // Check if file already exists first
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }

        // Use atomic_write which handles tmp + rename
        match atomic_write(&path, manifest_hash.to_hex().as_bytes()).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(false) // Another writer won during the race
            }
            Err(e) => Err(e),
        }
    }

    /// Lookup manifest_hash by spec_key.
    ///
    /// If the mapping file exists but is unreadable/corrupt, we treat it as
    /// a cache miss (re-acquire will fix it via first-writer-wins).
    async fn lookup_spec(&self, spec_key: &ToolchainSpecKey) -> Option<Blake3Hash> {
        let path = self.spec_path(spec_key);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        Blake3Hash::from_hex(content.trim())
    }

    /// Initialize directory structure
    async fn init(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(self.blobs_dir()).await?;
        tokio::fs::create_dir_all(self.manifests_dir()).await?;
        tokio::fs::create_dir_all(self.cache_dir()).await?;
        tokio::fs::create_dir_all(self.tmp_dir()).await?;
        Ok(())
    }

    /// Store a ToolchainManifest using the manifest storage path.
    async fn put_toolchain_manifest(&self, manifest: &ToolchainManifest) -> Blake3Hash {
        let json = facet_json::to_string(manifest);
        let hash = Blake3Hash::from_bytes(json.as_bytes());
        let path = self.manifest_path(&hash);

        if !path.exists() {
            let _ = atomic_write(&path, json.as_bytes()).await;
        }

        hash
    }
}
