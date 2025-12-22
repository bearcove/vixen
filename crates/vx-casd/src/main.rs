//! vx-casd - Content-Addressed Storage daemon
//!
//! Provides CAS, toolchain, and registry services over TCP using rapace.

use camino::Utf8PathBuf;
use eyre::Result;
use std::sync::Arc;
use tokio::net::TcpListener;
use vx_casd::CasService;
use vx_cas_proto::CasServer;

#[derive(Debug)]
struct Args {
    /// Storage root directory
    root: Utf8PathBuf,

    /// Bind address (host:port)
    bind: String,
}

impl Args {
    fn from_env() -> Result<Self> {
        let root = std::env::var("VX_CAS_ROOT")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME not set");
                format!("{}/.vx/cas", home)
            });

        let bind = std::env::var("VX_CAS_BIND")
            .unwrap_or_else(|_| "127.0.0.1:9002".to_string());

        Ok(Args {
            root: Utf8PathBuf::from(root),
            bind,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_casd=info"))
        )
        .init();

    let args = Args::from_env()?;

    // Initialize CAS service
    tracing::info!("Initializing CAS at {}", args.root);
    let cas = Arc::new(CasService::new(args.root));
    cas.init()?;

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("CAS listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let cas = Arc::clone(&cas);

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
