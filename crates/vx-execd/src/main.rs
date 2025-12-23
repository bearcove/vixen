//! vx-execd - Execution daemon
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

pub(crate) mod extract;
pub(crate) mod registry;
pub(crate) mod service;
pub(crate) mod toolchain;

use camino::Utf8PathBuf;
use eyre::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use vx_cas_proto::{Blake3Hash, CasClient};
use vx_exec_proto::ExecServer;

use crate::registry::RegistryMaterializer;

#[derive(Debug)]
struct Args {
    /// Toolchains directory
    toolchains_dir: Utf8PathBuf,

    /// Registry cache directory
    registry_cache_dir: Utf8PathBuf,

    /// CAS endpoint (host:port)
    cas_endpoint: String,

    /// Bind address (host:port)
    bind: String,
}

impl Args {
    fn from_env() -> Result<Self> {
        let vx_home = std::env::var("VX_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{}/.vx", home)
        });

        let toolchains_dir = Utf8PathBuf::from(&vx_home).join("toolchains");
        let registry_cache_dir = Utf8PathBuf::from(&vx_home).join("registry");

        let cas_endpoint = std::env::var("VX_CAS").unwrap_or_else(|_| "127.0.0.1:9002".to_string());

        let bind = std::env::var("VX_EXEC").unwrap_or_else(|_| "127.0.0.1:9003".to_string());

        // V1: Enforce loopback-only
        validate_loopback(&bind)?;
        validate_loopback(&cas_endpoint)?;

        Ok(Args {
            toolchains_dir,
            registry_cache_dir,
            cas_endpoint,
            bind,
        })
    }
}

/// Validate that the endpoint is loopback-only (127.0.0.1:*)
fn validate_loopback(endpoint: &str) -> Result<()> {
    let addr = endpoint
        .parse::<std::net::SocketAddr>()
        .map_err(|e| eyre::eyre!("invalid endpoint '{}': {}", endpoint, e))?;

    if !addr.ip().is_loopback() {
        eyre::bail!(
            "vx-execd V1 only supports loopback endpoints (127.0.0.1:*), got: {}\n\
            Remote execution is not yet supported.",
            endpoint
        );
    }

    Ok(())
}

/// Connect to CAS and return a client handle
async fn connect_to_cas(endpoint: &str) -> Result<vx_cas_proto::CasClient> {
    let stream = TcpStream::connect(endpoint).await?;
    let transport = rapace::Transport::stream(stream);

    // Create RPC session and client
    let session = Arc::new(rapace::RpcSession::new(transport));
    let client = CasClient::new(session.clone());

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
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_execd=info")),
        )
        .init();

    let args = Args::from_env()?;

    // Connect to CAS
    tracing::info!("Connecting to CAS at {}", args.cas_endpoint);
    let cas = connect_to_cas(&args.cas_endpoint).await?;
    tracing::info!("Connected to CAS");

    // Initialize Exec service
    tracing::info!("Initializing Exec service");
    tracing::info!("  Toolchains: {}", args.toolchains_dir);
    tracing::info!("  Registry:   {}", args.registry_cache_dir);

    // Ensure directories exist
    tokio::fs::create_dir_all(&args.toolchains_dir).await?;
    tokio::fs::create_dir_all(&args.registry_cache_dir).await?;

    let exec = ExecService::new(Arc::new(cas), args.toolchains_dir, args.registry_cache_dir);

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("Exec listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let exec = exec.clone();

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            // Create transport from TCP stream
            let transport = rapace::Transport::stream(socket);

            // Serve the Exec service
            let server = ExecServer::new(exec);
            if let Err(e) = server.serve(transport).await {
                tracing::warn!("Connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}

/// Inner Exec service implementation
pub struct ExecServiceInner {
    /// CAS client for storing outputs and fetching toolchains
    pub(crate) cas: Arc<CasClient>,

    /// Toolchain materialization directory
    pub(crate) toolchains_dir: Utf8PathBuf,

    /// In-flight toolchain materializations (keyed by manifest_hash)
    /// Uses Arc<tokio::sync::Mutex> for async locking
    materializing: Arc<
        tokio::sync::Mutex<
            HashMap<Blake3Hash, Arc<tokio::sync::OnceCell<Result<Utf8PathBuf, String>>>>,
        >,
    >,

    /// Registry crate materializer (kept for future use)
    #[allow(dead_code)]
    registry_materializer: RegistryMaterializer,
}

/// Exec service handle - cloneable wrapper around shared inner state
#[derive(Clone)]
pub struct ExecService {
    inner: Arc<ExecServiceInner>,
}

impl std::ops::Deref for ExecService {
    type Target = ExecServiceInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ExecService {
    pub fn new(
        cas: Arc<CasClient>,
        toolchains_dir: Utf8PathBuf,
        registry_cache_dir: Utf8PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(ExecServiceInner {
                registry_materializer: RegistryMaterializer::new(cas.clone(), registry_cache_dir),
                cas,
                toolchains_dir,
                materializing: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            }),
        }
    }

    /// Ensures a toolchain is materialized locally, returns the materialized directory.
    /// Uses async locking to prevent concurrent materializations of the same toolchain.
    pub(crate) async fn ensure_materialized(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
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
    async fn materialize_toolchain(
        &self,
        manifest_hash: Blake3Hash,
    ) -> Result<Utf8PathBuf, String> {
        self.materialize_toolchain_impl(manifest_hash).await
    }
}
