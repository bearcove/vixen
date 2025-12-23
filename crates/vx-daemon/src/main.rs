//! vx-daemon - Build orchestration daemon
//!
//! The daemon orchestrates builds but never touches the filesystem directly.
//! All file I/O and compilation is delegated to vx-execd.
//!
//! Responsibilities:
//! - Own the picante incremental computation database
//! - Compute cache keys
//! - Check CAS for cache hits
//! - Send compile requests to execd
//! - Materialize final outputs locally (via execd or direct CAS fetch)

mod db;
mod inputs;
mod queries;
mod service;

use camino::Utf8PathBuf;
use eyre::Result;
use std::process::Child;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use vx_cas_proto::{CasClient, ServiceVersion};
use vx_daemon_proto::{BuildRequest, BuildResult, Daemon, DaemonServer, DAEMON_PROTOCOL_VERSION};
use vx_exec_proto::ExecClient;

pub use db::Database;
pub use inputs::*;
pub use queries::*;
pub use service::DaemonService;

/// Newtype wrapper around Arc<DaemonService> to satisfy orphan rules.
/// This allows us to implement the Daemon trait from vx-daemon-proto.
#[derive(Clone)]
pub struct DaemonHandle(Arc<DaemonService>);

impl DaemonHandle {
    pub fn new(service: DaemonService) -> Self {
        Self(Arc::new(service))
    }
}

impl std::ops::Deref for DaemonHandle {
    type Target = DaemonService;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Daemon for DaemonHandle {
    async fn build(&self, request: BuildRequest) -> BuildResult {
        match self.0.do_build(request).await {
            Ok(result) => result,
            Err(e) => BuildResult {
                success: false,
                message: "Build failed".to_string(),
                cached: false,
                duration_ms: 0,
                output_path: None,
                error: Some(e),
            },
        }
    }

    async fn shutdown(&self) {
        tracing::info!("Shutdown requested, killing spawned services");
        self.0.kill_spawned_services().await;
        tracing::info!("Spawned services killed, exiting daemon");
        std::process::exit(0);
    }

    async fn version(&self) -> ServiceVersion {
        ServiceVersion {
            service: "vx-daemon".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: DAEMON_PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug)]
struct Args {
    /// VX_HOME directory
    vx_home: Utf8PathBuf,

    /// CAS endpoint (host:port)
    cas_endpoint: String,

    /// Exec endpoint (host:port)
    exec_endpoint: String,

    /// Bind address (host:port)
    bind: String,
}

impl Args {
    fn from_env() -> Result<Self> {
        let vx_home = std::env::var("VX_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{}/.vx", home)
        });

        let cas_endpoint_raw =
            std::env::var("VX_CAS").unwrap_or_else(|_| "127.0.0.1:9002".to_string());
        let cas_endpoint = vx_io::net::normalize_tcp_endpoint(&cas_endpoint_raw)?;

        let exec_endpoint_raw =
            std::env::var("VX_EXEC").unwrap_or_else(|_| "127.0.0.1:9003".to_string());
        let exec_endpoint = vx_io::net::normalize_tcp_endpoint(&exec_endpoint_raw)?;

        let bind_raw =
            std::env::var("VX_DAEMON").unwrap_or_else(|_| "127.0.0.1:9001".to_string());
        let bind = vx_io::net::normalize_tcp_endpoint(&bind_raw)?;

        Ok(Args {
            vx_home: Utf8PathBuf::from(vx_home),
            cas_endpoint,
            exec_endpoint,
            bind,
        })
    }
}

/// Tracks spawned child services
#[derive(Default)]
pub struct SpawnTracker {
    casd: Option<Child>,
    execd: Option<Child>,
}

impl SpawnTracker {
    pub fn kill_all(&mut self) {
        if let Some(mut child) = self.casd.take() {
            tracing::info!("Killing spawned vx-casd (pid: {})", child.id());
            let _ = child.kill();
        }
        if let Some(mut child) = self.execd.take() {
            tracing::info!("Killing spawned vx-execd (pid: {})", child.id());
            let _ = child.kill();
        }
    }
}

/// Try to connect to a service endpoint
async fn try_connect(endpoint: &str) -> Result<TcpStream> {
    TcpStream::connect(endpoint).await.map_err(Into::into)
}

/// Spawn a service binary and return the child process
fn spawn_service(binary_name: &str, env_vars: &[(&str, &str)]) -> Result<Child> {
    let mut cmd = std::process::Command::new(binary_name);
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    cmd.spawn().map_err(Into::into)
}

/// Ensure services are running, spawning them if necessary
async fn ensure_services(args: &Args, spawn_tracker: &Arc<Mutex<SpawnTracker>>) -> Result<()> {
    let backoff_ms = [10, 50, 100, 500, 1000];

    // Check CAS
    if try_connect(&args.cas_endpoint).await.is_err() {
        if !vx_io::net::is_loopback_endpoint(&args.cas_endpoint) {
            eyre::bail!(
                "CAS is not reachable at {}. Auto-spawn is only supported for loopback endpoints.\n\
                Start vx-casd on the remote host, or point VX_CAS to a local endpoint.",
                args.cas_endpoint
            );
        }

        tracing::info!("CAS not running, spawning vx-casd on {}", args.cas_endpoint);

        let child = spawn_service(
            "vx-casd",
            &[
                ("VX_HOME", args.vx_home.as_str()),
                ("VX_CAS", &args.cas_endpoint),
            ],
        )?;

        spawn_tracker.lock().await.casd = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.cas_endpoint).await.is_ok() {
                tracing::info!("Connected to CAS after {} attempts", attempt + 1);
                break;
            }
        }

        try_connect(&args.cas_endpoint).await?;
    } else {
        tracing::info!("CAS already running at {}", args.cas_endpoint);
    }

    // Check Exec
    if try_connect(&args.exec_endpoint).await.is_err() {
        if !vx_io::net::is_loopback_endpoint(&args.exec_endpoint) {
            eyre::bail!(
                "Exec is not reachable at {}. Auto-spawn is only supported for loopback endpoints.\n\
                Start vx-execd on the remote host, or point VX_EXEC to a local endpoint.",
                args.exec_endpoint
            );
        }

        tracing::info!(
            "Exec not running, spawning vx-execd on {}",
            args.exec_endpoint
        );

        let child = spawn_service(
            "vx-execd",
            &[
                ("VX_HOME", args.vx_home.as_str()),
                ("VX_CAS", &args.cas_endpoint),
                ("VX_EXEC", &args.exec_endpoint),
            ],
        )?;

        spawn_tracker.lock().await.execd = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.exec_endpoint).await.is_ok() {
                tracing::info!("Connected to Exec after {} attempts", attempt + 1);
                break;
            }
        }

        try_connect(&args.exec_endpoint).await?;
    } else {
        tracing::info!("Exec already running at {}", args.exec_endpoint);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_daemon=info")),
        )
        .init();

    // Install miette-arborium for rich syntax-highlighted diagnostics
    miette_arborium::install_global();

    let args = Args::from_env()?;
    tracing::info!("Starting vx-daemon");
    tracing::info!("  VX_HOME: {}", args.vx_home);
    tracing::info!("  CAS:     {}", args.cas_endpoint);
    tracing::info!("  Exec:    {}", args.exec_endpoint);
    tracing::info!("  Bind:    {}", args.bind);

    let spawn_tracker = Arc::new(Mutex::new(SpawnTracker::default()));

    ensure_services(&args, &spawn_tracker).await?;

    let exec_host_triple = match std::env::var("VX_EXEC_HOST_TRIPLE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            if !vx_io::net::is_loopback_endpoint(&args.exec_endpoint) {
                eyre::bail!(
                    "VX_EXEC points to a non-loopback endpoint ({}) but VX_EXEC_HOST_TRIPLE is not set.\n\
                    vx-daemon must know the execd host triple to request the correct Rust toolchain from CAS.\n\
                    Set VX_EXEC_HOST_TRIPLE (e.g. x86_64-unknown-linux-gnu) and retry.",
                    args.exec_endpoint
                );
            }

            vx_toolchain::detect_host_triple()
                .map_err(|e| eyre::eyre!("failed to detect host triple: {}", e))?
        }
    };

    // Create rapace clients for CAS and Exec
    tracing::info!("Connecting to CAS at {}", args.cas_endpoint);
    let cas_stream = TcpStream::connect(&args.cas_endpoint).await?;
    let cas_transport = rapace::Transport::stream(cas_stream);
    let cas_session = Arc::new(rapace::RpcSession::new(cas_transport));
    let cas_client = Arc::new(CasClient::new(cas_session.clone()));

    tracing::info!("Connecting to Exec at {}", args.exec_endpoint);
    let exec_stream = TcpStream::connect(&args.exec_endpoint).await?;
    let exec_transport = rapace::Transport::stream(exec_stream);
    let exec_session = Arc::new(rapace::RpcSession::new(exec_transport));
    let exec_client = Arc::new(ExecClient::new(exec_session.clone()));

    // Spawn session runners
    let cas_session_runner = cas_session.clone();
    tokio::spawn(async move {
        if let Err(e) = cas_session_runner.run().await {
            tracing::error!("CAS session error: {}", e);
        }
    });

    let exec_session_runner = exec_session.clone();
    tokio::spawn(async move {
        if let Err(e) = exec_session_runner.run().await {
            tracing::error!("Exec session error: {}", e);
        }
    });

    // Initialize daemon service
    tracing::info!("Initializing daemon service");
    let daemon = DaemonHandle::new(DaemonService::new(
        cas_client,
        exec_client,
        args.vx_home,
        exec_host_triple,
        spawn_tracker,
    ));

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("Daemon listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let daemon = daemon.clone();

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            let transport = rapace::Transport::stream(socket);
            let server = DaemonServer::new(daemon);

            if let Err(e) = server.serve(transport).await {
                tracing::warn!("Connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}
