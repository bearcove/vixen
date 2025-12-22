//! vx-execd - Execution daemon
//!
//! Provides Exec service over TCP using rapace.
//! Connects to CAS for storing outputs and fetching toolchains.

use camino::Utf8PathBuf;
use eyre::Result;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use vx_execd::ExecService;
use vx_exec_proto::ExecServer;

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

        let cas_endpoint = std::env::var("VX_CAS")
            .unwrap_or_else(|_| "127.0.0.1:9002".to_string());

        let bind = std::env::var("VX_EXEC")
            .unwrap_or_else(|_| "127.0.0.1:9003".to_string());

        Ok(Args {
            toolchains_dir,
            registry_cache_dir,
            cas_endpoint,
            bind,
        })
    }
}

/// Connect to CAS and return a client handle
async fn connect_to_cas(endpoint: &str) -> Result<impl vx_cas_proto::Cas + vx_cas_proto::CasToolchain + vx_cas_proto::CasRegistry + Clone + Send + Sync + 'static> {
    use vx_cas_proto::CasClient;

    let stream = TcpStream::connect(endpoint).await?;
    let transport = rapace::Transport::stream(stream);

    // Create client and spawn session.run() in background
    let (client, session) = CasClient::new(transport);

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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_execd=info"))
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
    tokio::task::spawn_blocking({
        let toolchains_dir = args.toolchains_dir.clone();
        let registry_cache_dir = args.registry_cache_dir.clone();
        move || -> Result<()> {
            std::fs::create_dir_all(&toolchains_dir)?;
            std::fs::create_dir_all(&registry_cache_dir)?;
            Ok(())
        }
    })
    .await??;

    let exec = Arc::new(ExecService::new(
        cas,
        args.toolchains_dir,
        args.registry_cache_dir,
    ));

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("Exec listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let exec = Arc::clone(&exec);

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
