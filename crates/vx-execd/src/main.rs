//! vx-execd - Execution daemon
//!
//! Implements the Exec rapace service trait.
//! Runs rustc and zig cc, streams outputs to CAS.
//! Materializes toolchains and registry crates on-demand from CAS.

pub(crate) mod extract;
pub(crate) mod registry;
pub(crate) mod service;
pub(crate) mod toolchain;

use camino::{Utf8Path, Utf8PathBuf};
use eyre::Result;
use futures_util::StreamExt;
use std::io::Read as _;
use std::sync::Arc;
use std::{collections::HashMap, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};
use vx_cas_proto::{Blake3Hash, CasClient, MaterializeStep};
use vx_cc::depfile::{canonicalize_deps, parse_depfile_path};
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

        Ok(Args {
            toolchains_dir,
            registry_cache_dir,
            cas_endpoint,
            bind,
        })
    }
}

/// Connect to CAS and return a client handle
async fn connect_to_cas(endpoint: &str) -> Result<vx_cas_proto::CasClient> {
    use std::sync::Arc;
    use vx_cas_proto::CasClient;

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
struct ExecServiceInner {
    /// CAS client for storing outputs and fetching toolchains
    cas: Arc<CasClient>,

    /// Toolchain materialization directory
    toolchains_dir: Utf8PathBuf,

    /// In-flight toolchain materializations (keyed by manifest_hash)
    /// Uses Arc<tokio::sync::Mutex> for async locking
    materializing: Arc<
        tokio::sync::Mutex<
            HashMap<Blake3Hash, Arc<tokio::sync::OnceCell<Result<Utf8PathBuf, String>>>>,
        >,
    >,

    /// Registry crate materializer
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
    /// Uses file locking to prevent concurrent materializations.
    async fn ensure_materialized(&self, manifest_hash: Blake3Hash) -> Result<Utf8PathBuf, String> {
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

/// Parse a depfile and canonicalize its dependencies to workspace-relative paths
fn parse_and_canonicalize_depfile(
    depfile_path: &Utf8Path,
    base_dir: &Utf8Path,
    workspace_root: &Utf8Path,
) -> Vec<String> {
    match parse_depfile_path(depfile_path) {
        Ok(parsed) => match canonicalize_deps(&parsed.deps, base_dir, workspace_root) {
            Ok(canonical) => canonical.into_iter().map(|p| p.to_string()).collect(),
            Err(e) => {
                warn!(
                    depfile = %depfile_path,
                    error = %e,
                    "failed to canonicalize depfile dependencies"
                );
                vec![]
            }
        },
        Err(e) => {
            warn!(
                depfile = %depfile_path,
                error = %e,
                "failed to parse depfile"
            );
            vec![]
        }
    }
}
