//! vx-rhea - Worker daemon
//!
//! Implements the Exec rapace service trait.
//! A remote-capable compilation service - all inputs/outputs go through CAS.
//!
//! Responsibilities:
//! - Materialize toolchains from CAS (cached locally)
//! - Materialize source trees from CAS
//! - Materialize dependency artifacts from CAS
//! - Run rustc/zig
//! - Ingest outputs to CAS
//! - Return output manifest hashes

pub(crate) mod error;
pub(crate) mod extract;
pub(crate) mod registry;
pub(crate) mod service;
pub(crate) mod toolchain;
pub(crate) mod vfs;

use error::{Result as RheaResult, RheaError};

use camino::Utf8PathBuf;
use eyre::Result;
use fs_kitty_proto::VfsServer;
use std::collections::HashMap;
use std::sync::Arc;
use vx_cass_proto::{Blake3Hash, CassClient};
use vx_io::net::{Endpoint, Listener};
use vx_rhea_proto::RheaServer;

use crate::registry::RegistryMaterializer;
use crate::vfs::CasVfs;

/// Type alias for inflight materialization tracking
type InflightMaterializations = Arc<
    tokio::sync::Mutex<HashMap<Blake3Hash, Arc<tokio::sync::OnceCell<RheaResult<Utf8PathBuf>>>>>,
>;

#[derive(Debug)]
struct Args {
    /// Toolchains directory
    toolchains_dir: Utf8PathBuf,

    /// Registry cache directory
    registry_cache_dir: Utf8PathBuf,

    /// CAS endpoint
    cas_endpoint: Endpoint,

    /// Bind endpoint for Rhea RPC service
    bind: Endpoint,

    /// Bind endpoint for VFS server (fs-kitty)
    vfs_bind: Endpoint,
}

impl Args {
    fn from_env() -> Result<Self> {
        let vx_home = std::env::var("VX_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{}/.vx", home)
        });
        let vx_home = Utf8PathBuf::from(&vx_home);

        let toolchains_dir = vx_home.join("toolchains");
        let registry_cache_dir = vx_home.join("registry");

        // Parse CAS endpoint (defaults to Unix socket)
        let cas_endpoint = match std::env::var("VX_CASS") {
            Ok(v) => Endpoint::parse(&v)?,
            Err(_) => {
                #[cfg(unix)]
                {
                    vx_io::net::default_unix_endpoint(&vx_home, "cass")
                }
                #[cfg(not(unix))]
                {
                    Endpoint::parse("127.0.0.1:9002")?
                }
            }
        };

        // Parse bind endpoint (defaults to Unix socket)
        let bind = match std::env::var("VX_RHEA") {
            Ok(v) => Endpoint::parse(&v)?,
            Err(_) => {
                #[cfg(unix)]
                {
                    vx_io::net::default_unix_endpoint(&vx_home, "rhea")
                }
                #[cfg(not(unix))]
                {
                    Endpoint::parse("127.0.0.1:9003")?
                }
            }
        };

        // Parse VFS endpoint (defaults to Unix socket)
        let vfs_bind = match std::env::var("VX_RHEA_VFS") {
            Ok(v) => Endpoint::parse(&v)?,
            Err(_) => {
                #[cfg(unix)]
                {
                    vx_io::net::default_unix_endpoint(&vx_home, "rhea-vfs")
                }
                #[cfg(not(unix))]
                {
                    Endpoint::parse("127.0.0.1:9004")?
                }
            }
        };

        Ok(Args {
            toolchains_dir,
            registry_cache_dir,
            cas_endpoint,
            bind,
            vfs_bind,
        })
    }
}

/// Connect to CAS and return a client handle
async fn connect_to_cas(endpoint: &Endpoint) -> Result<vx_cass_proto::CassClient> {
    let stream = vx_io::net::connect(endpoint).await?;
    let transport = rapace::Transport::stream(stream);

    // Create RPC session and client
    let session = Arc::new(rapace::RpcSession::new(transport));
    let client = CassClient::new(session.clone());

    // CRITICAL: spawn session.run() in background
    // rapace requires explicit receive loop
    tokio::spawn(async move {
        if let Err(e) = session.run().await {
            tracing::error!("CAS session error: {}", e);
        }
    });

    Ok(client)
}

#[tokio::main]
async fn main() -> Result<()> {
    // If spawned by parent, die when parent dies
    ur_taking_me_with_you::die_with_parent();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_rhea=info")),
        )
        .init();

    let args = Args::from_env()?;

    // Connect to CAS
    tracing::info!("Connecting to CAS at {}", args.cas_endpoint);
    let cas = connect_to_cas(&args.cas_endpoint).await?;
    tracing::info!("Connected to CAS");

    // Initialize services
    tracing::info!("Initializing Rhea services");
    tracing::info!("  Toolchains: {}", args.toolchains_dir);
    tracing::info!("  Registry:   {}", args.registry_cache_dir);

    // Ensure directories exist
    tokio::fs::create_dir_all(&args.toolchains_dir).await?;
    tokio::fs::create_dir_all(&args.registry_cache_dir).await?;

    let cas = Arc::new(cas);

    // Create VFS (shared between VFS server and RheaService)
    let vfs = Arc::new(CasVfs::new(cas.clone()));

    // Create Rhea service (will use VFS for action execution)
    let exec = RheaService::new(cas.clone(), args.toolchains_dir, args.registry_cache_dir, vfs.clone());

    // Start Rhea RPC server
    let rhea_listener = Listener::bind(&args.bind).await?;
    tracing::info!("Rhea RPC listening on {}", args.bind);

    let exec_for_rhea = exec.clone();
    tokio::spawn(async move {
        loop {
            match rhea_listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let exec = exec_for_rhea.clone();
                    tokio::spawn(async move {
                        tracing::debug!("Rhea connection from {}", peer_addr);
                        let transport = rapace::Transport::stream(stream);
                        let server = RheaServer::new(exec);
                        if let Err(e) = server.serve(transport).await {
                            tracing::warn!("Rhea connection error from {}: {}", peer_addr, e);
                        }
                        tracing::debug!("Rhea connection from {} closed", peer_addr);
                    });
                }
                Err(e) => {
                    tracing::error!("Rhea accept error: {}", e);
                }
            }
        }
    });

    // Start VFS server (fs-kitty)
    let vfs_listener = Listener::bind(&args.vfs_bind).await?;
    tracing::info!("VFS (fs-kitty) listening on {}", args.vfs_bind);

    loop {
        let (stream, peer_addr) = vfs_listener.accept().await?;
        let vfs = Arc::clone(&vfs);

        tokio::spawn(async move {
            tracing::debug!("VFS connection from {}", peer_addr);

            let transport = rapace::Transport::stream(stream);
            let session = Arc::new(rapace::RpcSession::new(transport.clone()));
            let vfs_server = VfsServer::new(vfs);
            session.set_dispatcher(vfs_server.into_session_dispatcher(transport));

            if let Err(e) = session.run().await {
                tracing::warn!("VFS connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("VFS connection from {} closed", peer_addr);
        });
    }
}

/// Inner Exec service implementation
pub struct RheaServiceInner {
    /// CAS client for storing outputs and fetching toolchains
    pub(crate) cas: Arc<CassClient>,

    /// Toolchain materialization directory
    pub(crate) toolchains_dir: Utf8PathBuf,

    /// In-flight toolchain materializations (keyed by manifest_hash)
    materializing: InflightMaterializations,

    /// Registry crate materializer
    pub(crate) registry_materializer: RegistryMaterializer,

    /// VFS for hermetic action execution
    pub(crate) vfs: Arc<CasVfs>,
}

/// Exec service handle - cloneable wrapper around shared inner state
#[derive(Clone)]
pub struct RheaService {
    inner: Arc<RheaServiceInner>,
}

impl std::ops::Deref for RheaService {
    type Target = RheaServiceInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl RheaService {
    pub fn new(
        cas: Arc<CassClient>,
        toolchains_dir: Utf8PathBuf,
        registry_cache_dir: Utf8PathBuf,
        vfs: Arc<CasVfs>,
    ) -> Self {
        Self {
            inner: Arc::new(RheaServiceInner {
                registry_materializer: RegistryMaterializer::new(cas.clone(), registry_cache_dir),
                cas,
                toolchains_dir,
                materializing: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                vfs,
            }),
        }
    }

    /// Ensures a toolchain is materialized locally, returns the materialized directory.
    /// Uses async locking to prevent concurrent materializations of the same toolchain.
    pub(crate) async fn ensure_materialized(
        &self,
        manifest_hash: Blake3Hash,
    ) -> RheaResult<Utf8PathBuf> {
        // Check if already materializing
        let cell = {
            let mut map = self.materializing.lock().await;
            map.entry(manifest_hash)
                .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
                .clone()
        };

        // Wait for or perform materialization
        cell.get_or_init(|| async { self.materialize_toolchain(manifest_hash).await })
            .await
            .clone()
    }

    /// Materialize a toolchain from CAS to local directory
    async fn materialize_toolchain(&self, manifest_hash: Blake3Hash) -> RheaResult<Utf8PathBuf> {
        self.materialize_toolchain_impl(manifest_hash).await
    }
}
