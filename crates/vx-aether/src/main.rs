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

#[cfg(test)]
mod tests;

use camino::Utf8PathBuf;
use eyre::Result;
use std::process::Child;
use std::sync::Arc;
use tokio::sync::Mutex;
use vx_aether_proto::AetherServer;
use vx_cass_proto::CassClient;
use vx_io::net::{Endpoint, Listener};
use vx_rhea_proto::RheaClient;

pub use db::Database;
pub use inputs::*;
pub use queries::*;
pub use service::AetherService;

// No longer needed! The #[rapace::service] macro on the Aether trait
// generates a blanket impl for Arc<T>, solving the orphan rule issue.
// We can now use Arc<AetherService> directly.

#[derive(Debug)]
struct Args {
    /// VX_HOME directory
    vx_home: Utf8PathBuf,

    /// CAS endpoint
    cas_endpoint: Endpoint,

    /// Exec endpoint
    exec_endpoint: Endpoint,

    /// Bind endpoint
    bind: Endpoint,
}

impl Args {
    fn from_env() -> Result<Self> {
        let vx_home = std::env::var("VX_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{}/.vx", home)
        });
        let vx_home = Utf8PathBuf::from(&vx_home);

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

        // Parse Rhea endpoint (defaults to Unix socket)
        let exec_endpoint = match std::env::var("VX_RHEA") {
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

        // Parse bind endpoint (defaults to Unix socket)
        let bind = match std::env::var("VX_AETHER") {
            Ok(v) => Endpoint::parse(&v)?,
            Err(_) => {
                #[cfg(unix)]
                {
                    vx_io::net::default_unix_endpoint(&vx_home, "aether")
                }
                #[cfg(not(unix))]
                {
                    Endpoint::parse("127.0.0.1:9001")?
                }
            }
        };

        Ok(Args {
            vx_home,
            cas_endpoint,
            exec_endpoint,
            bind,
        })
    }
}

/// Tracks spawned child services
#[derive(Default)]
pub struct SpawnTracker {
    cass: Option<Child>,
    rhea: Option<Child>,
}

impl SpawnTracker {
    pub fn kill_all(&mut self) {
        if let Some(mut child) = self.cass.take() {
            tracing::info!("Killing spawned vx-cass (pid: {})", child.id());
            if let Err(e) = child.kill() {
                tracing::warn!("Failed to kill vx-cass (pid: {}): {e}", child.id());
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

/// Spawn a service binary and return the child process
fn spawn_service(
    binary_name: &str,
    env_vars: &[(&str, String)],
    log_path: &camino::Utf8Path,
) -> Result<Child> {
    use std::fs::OpenOptions;
    use std::process::Stdio;

    // Ensure parent directory exists for log file
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

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

    // Check CAS (storage)
    if vx_io::net::try_connect(&args.cas_endpoint).await?.is_none() {
        if !args.cas_endpoint.is_local() {
            eyre::bail!(
                "Cass is not reachable at {}. Auto-spawn is only supported for local endpoints.\n\
                Start vx-cass on the remote host, or point VX_CASS to a local endpoint.",
                args.cas_endpoint
            );
        }

        tracing::info!(
            "Cass not running, spawning vx-cass on {}",
            args.cas_endpoint
        );

        let cass_log = args.vx_home.join("cass.log");
        let child = spawn_service(
            "vx-cass",
            &[
                ("VX_HOME", args.vx_home.to_string()),
                ("VX_CASS", args.cas_endpoint.display()),
            ],
            &cass_log,
        )?;

        tracing::info!("vx-cass logs: {}", cass_log);
        spawn_tracker.lock().await.cass = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if vx_io::net::try_connect(&args.cas_endpoint).await?.is_some() {
                tracing::info!("Connected to Cass after {} attempts", attempt + 1);
                break;
            }
        }

        // Final connection attempt
        vx_io::net::connect(&args.cas_endpoint).await?;
    } else {
        tracing::info!("Cass already running at {}", args.cas_endpoint);
    }

    // Check Rhea (worker)
    if vx_io::net::try_connect(&args.exec_endpoint).await?.is_none() {
        if !args.exec_endpoint.is_local() {
            eyre::bail!(
                "Rhea is not reachable at {}. Auto-spawn is only supported for local endpoints.\n\
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
                ("VX_HOME", args.vx_home.to_string()),
                ("VX_CASS", args.cas_endpoint.display()),
                ("VX_RHEA", args.exec_endpoint.display()),
            ],
            &rhea_log,
        )?;

        tracing::info!("vx-rhea logs: {}", rhea_log);
        spawn_tracker.lock().await.rhea = Some(child);

        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if vx_io::net::try_connect(&args.exec_endpoint).await?.is_some() {
                tracing::info!("Connected to Rhea after {} attempts", attempt + 1);
                break;
            }
        }

        // Final connection attempt
        vx_io::net::connect(&args.exec_endpoint).await?;
    } else {
        tracing::info!("Rhea already running at {}", args.exec_endpoint);
    }

    Ok(())
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    // Install tracing to stderr (logs will go to ~/.vx/aether.log via spawn_service redirection)
    use tracing_subscriber::prelude::*;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_aether=debug")),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
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
            if !args.exec_endpoint.is_local() {
                eyre::bail!(
                    "VX_RHEA points to a non-local endpoint ({}) but VX_RHEA_HOST_TRIPLE is not set.\n\
                    vx-aether must know the rhea host triple to request the correct Rust toolchain from CAS.\n\
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
    let cas_stream = vx_io::net::connect(&args.cas_endpoint).await?;
    let cas_transport = rapace::Transport::stream(cas_stream);
    let cas_session = Arc::new(rapace::RpcSession::new(cas_transport));
    let cas_client = Arc::new(CassClient::new(cas_session.clone()));

    tracing::info!("Connecting to Exec at {}", args.exec_endpoint);
    let exec_stream = vx_io::net::connect(&args.exec_endpoint).await?;
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

    // Initialize aether service (singleton shared across all connections)
    tracing::info!("Initializing aether service");
    let aether = Arc::new(
        AetherService::new(
            cas_client,
            exec_client,
            args.vx_home,
            exec_host_triple,
            spawn_tracker,
        )
        .await,
    );

    // Start server (TCP or Unix socket)
    let listener = Listener::bind(&args.bind).await?;
    tracing::info!("Aether listening on {}", args.bind);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let aether = aether.clone();

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            let transport = rapace::Transport::stream(stream);

            // Create RPC session with even channel IDs (server uses 0, 2, 4, ...)
            let session = Arc::new(rapace::RpcSession::with_channel_start(transport, 0));

            // Create ProgressListenerClient from this session
            let progress_listener = Arc::new(vx_aether_proto::ProgressListenerClient::new(session.clone()));

            // Set progress listener for this connection
            aether.set_progress_listener(Some(progress_listener)).await;

            // Set up dispatcher for Aether service
            let server = Arc::new(AetherServer::new(aether.clone()));
            let buffer_pool = session.buffer_pool().clone();
            session.set_dispatcher(move |request| {
                let server = server.clone();
                let buffer_pool = buffer_pool.clone();
                Box::pin(async move {
                    server.dispatch(request.desc.method_id, &request, &buffer_pool).await
                })
            });

            // Run the session
            if let Err(e) = session.run().await {
                tracing::warn!("Session error from {}: {}", peer_addr, e);
            }

            // Clear progress listener when connection closes
            aether.set_progress_listener(None).await;

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}
