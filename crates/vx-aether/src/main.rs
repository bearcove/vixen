//! vx-aether - Build orchestration daemon
//!
//! The daemon orchestrates builds but never touches the filesystem directly.
//! All file I/O and compilation is delegated to vx-rhea.
//!
//! Responsibilities:
//! - Own the picante incremental computation database
//! - Compute cache keys
//! - Check CAS for cache hits
//! - Send compile requests to execd
//! - Materialize final outputs locally (via execd or direct CAS fetch)

mod action_graph;
mod db;
mod error;
mod executor;
mod inputs;
mod queries;
mod service;
mod tui;

use camino::Utf8PathBuf;
use eyre::Result;
use std::process::Child;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use vx_aether_proto::AetherServer;
use vx_oort_proto::OortClient;
use vx_rhea_proto::RheaClient;

pub use db::Database;
pub use inputs::*;
pub use queries::*;
pub use service::AetherService;
pub use tui::{ActionType, TuiHandle};

// No longer needed! The #[rapace::service] macro on the Aether trait
// generates a blanket impl for Arc<T>, solving the orphan rule issue.
// We can now use Arc<AetherService> directly.

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
            std::env::var("VX_OORT").unwrap_or_else(|_| "127.0.0.1:9002".to_string());
        let cas_endpoint = vx_io::net::normalize_tcp_endpoint(&cas_endpoint_raw)?;

        let exec_endpoint_raw =
            std::env::var("VX_RHEA").unwrap_or_else(|_| "127.0.0.1:9003".to_string());
        let exec_endpoint = vx_io::net::normalize_tcp_endpoint(&exec_endpoint_raw)?;

        let bind_raw = std::env::var("VX_AETHER").unwrap_or_else(|_| "127.0.0.1:9001".to_string());
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
    oort: Option<Child>,
    rhea: Option<Child>,
}

impl SpawnTracker {
    pub fn kill_all(&mut self) {
        if let Some(mut child) = self.oort.take() {
            tracing::info!("Killing spawned vx-oort (pid: {})", child.id());
            if let Err(e) = child.kill() {
                tracing::warn!("Failed to kill vx-oort (pid: {}): {e}", child.id());
            }
        }
        if let Some(mut child) = self.rhea.take() {
            tracing::info!("Killing spawned vx-rhea (pid: {})", child.id());
            if let Err(e) = child.kill() {
                tracing::warn!("Failed to kill vx-rhea (pid: {}): {e}", child.id());
            }
        }
    }
}

/// Try to connect to a service endpoint
async fn try_connect(endpoint: &str) -> Result<TcpStream> {
    TcpStream::connect(endpoint).await.map_err(Into::into)
}

/// Spawn a service binary and return the child process
fn spawn_service(binary_name: &str, env_vars: &[(&str, &str)], log_path: &camino::Utf8Path) -> Result<Child> {
    use std::fs::OpenOptions;
    use std::process::Stdio;

    // Open log file for appending
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let mut cmd = std::process::Command::new(binary_name);
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    // Redirect stdout and stderr to log file
    cmd.stdout(Stdio::from(log_file.try_clone()?));
    cmd.stderr(Stdio::from(log_file));

    ur_taking_me_with_you::spawn_dying_with_parent(cmd).map_err(Into::into)
}

/// Ensure services are running, spawning them if necessary
async fn ensure_services(args: &Args, spawn_tracker: &Arc<Mutex<SpawnTracker>>) -> Result<()> {
    let backoff_ms = [10, 50, 100, 500, 1000];

    // Check Oort (storage)
    if try_connect(&args.cas_endpoint).await.is_err() {
        if !vx_io::net::is_loopback_endpoint(&args.cas_endpoint) {
            eyre::bail!(
                "Oort is not reachable at {}. Auto-spawn is only supported for loopback endpoints.\n\
                Start vx-oort on the remote host, or point VX_OORT to a local endpoint.",
                args.cas_endpoint
            );
        }

        tracing::info!("Oort not running, spawning vx-oort on {}", args.cas_endpoint);

        let oort_log = args.vx_home.join("oort.log");
        let child = spawn_service(
            "vx-oort",
            &[
                ("VX_HOME", args.vx_home.as_str()),
                ("VX_OORT", &args.cas_endpoint),
            ],
            &oort_log,
        )?;

        tracing::info!("vx-oort logs: {}", oort_log);
        spawn_tracker.lock().await.oort = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.cas_endpoint).await.is_ok() {
                tracing::info!("Connected to Oort after {} attempts", attempt + 1);
                break;
            }
        }

        try_connect(&args.cas_endpoint).await?;
    } else {
        tracing::info!("Oort already running at {}", args.cas_endpoint);
    }

    // Check Rhea (worker)
    if try_connect(&args.exec_endpoint).await.is_err() {
        if !vx_io::net::is_loopback_endpoint(&args.exec_endpoint) {
            eyre::bail!(
                "Rhea is not reachable at {}. Auto-spawn is only supported for loopback endpoints.\n\
                Start vx-rhea on the remote host, or point VX_RHEA to a local endpoint.",
                args.exec_endpoint
            );
        }

        tracing::info!(
            "Rhea not running, spawning vx-rhea on {}",
            args.exec_endpoint
        );

        let rhea_log = args.vx_home.join("rhea.log");
        let child = spawn_service(
            "vx-rhea",
            &[
                ("VX_HOME", args.vx_home.as_str()),
                ("VX_OORT", &args.cas_endpoint),
                ("VX_RHEA", &args.exec_endpoint),
            ],
            &rhea_log,
        )?;

        tracing::info!("vx-rhea logs: {}", rhea_log);
        spawn_tracker.lock().await.rhea = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.exec_endpoint).await.is_ok() {
                tracing::info!("Connected to Rhea after {} attempts", attempt + 1);
                break;
            }
        }

        try_connect(&args.exec_endpoint).await?;
    } else {
        tracing::info!("Rhea already running at {}", args.exec_endpoint);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // FIRST THING: Create TUI and install tracing layer
    // This MUST happen before ANY other code that might log
    let tui = crate::tui::TuiHandle::new();
    let tui_layer = tui.tracing_layer();

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::fmt;

    // TEMPORARY: Write logs to stderr for debugging instead of TUI
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_aether=trace")),
        )
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    // Now safe to call things that might log
    ur_taking_me_with_you::die_with_parent();

    // Install miette-arborium for rich syntax-highlighted diagnostics
    if let Err(e) = miette_arborium::install_global() {
        tracing::warn!("Failed to install miette-arborium global handler: {e}");
    }

    let args = Args::from_env()?;
    tracing::info!("Starting vx-aether");
    tracing::info!("  VX_HOME: {}", args.vx_home);
    tracing::info!("  CAS:     {}", args.cas_endpoint);
    tracing::info!("  Exec:    {}", args.exec_endpoint);
    tracing::info!("  Bind:    {}", args.bind);

    let spawn_tracker = Arc::new(Mutex::new(SpawnTracker::default()));

    ensure_services(&args, &spawn_tracker).await?;

    let exec_host_triple = match std::env::var("VX_RHEA_HOST_TRIPLE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            if !vx_io::net::is_loopback_endpoint(&args.exec_endpoint) {
                eyre::bail!(
                    "VX_RHEA points to a non-loopback endpoint ({}) but VX_RHEA_HOST_TRIPLE is not set.\n\
                    vx-aether must know the rhea host triple to request the correct Rust toolchain from Oort.\n\
                    Set VX_RHEA_HOST_TRIPLE (e.g. x86_64-unknown-linux-gnu) and retry.",
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
    let cas_client = Arc::new(OortClient::new(cas_session.clone()));

    tracing::info!("Connecting to Exec at {}", args.exec_endpoint);
    let exec_stream = TcpStream::connect(&args.exec_endpoint).await?;
    let exec_transport = rapace::Transport::stream(exec_stream);
    let exec_session = Arc::new(rapace::RpcSession::new(exec_transport));
    let exec_client = Arc::new(RheaClient::new(exec_session.clone()));

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

    // Initialize aether service
    tracing::info!("Initializing aether service");
    let aether = Arc::new(
        AetherService::new(
            cas_client,
            exec_client,
            args.vx_home,
            exec_host_triple,
            spawn_tracker,
            tui,
        )
        .await,
    );

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("Aether listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let aether = aether.clone();

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            let transport = rapace::Transport::stream(socket);
            let server = AetherServer::new(aether);

            if let Err(e) = server.serve(transport).await {
                tracing::warn!("Connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}
