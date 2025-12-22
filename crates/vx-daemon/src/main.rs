//! vx-daemon - Build orchestration daemon
//!
//! Provides the Daemon service over TCP using rapace.
//! Spawns CAS and Exec services if not already running.

use camino::Utf8PathBuf;
use eyre::Result;
use std::process::Child;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use vx_daemon::DaemonService;
use vx_daemon_proto::{BuildRequest, BuildResult, Daemon, DaemonServer};

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

        let cas_endpoint = std::env::var("VX_DAEMON_CAS")
            .unwrap_or_else(|_| "127.0.0.1:9002".to_string());

        let exec_endpoint = std::env::var("VX_DAEMON_EXEC")
            .unwrap_or_else(|_| "127.0.0.1:9003".to_string());

        let bind = std::env::var("VX_DAEMON")
            .unwrap_or_else(|_| "127.0.0.1:9001".to_string());

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
struct SpawnTracker {
    casd: Option<Child>,
    execd: Option<Child>,
}

impl SpawnTracker {
    fn kill_all(&mut self) {
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

/// Wrapper service that implements Daemon trait with shutdown support
struct DaemonWrapper {
    inner: DaemonService,
    spawn_tracker: Arc<Mutex<SpawnTracker>>,
}

impl Daemon for DaemonWrapper {
    async fn build(&self, request: BuildRequest) -> BuildResult {
        self.inner.build(request).await
    }

    async fn shutdown(&self) {
        tracing::info!("Shutdown requested");

        // Kill spawned children
        let mut tracker = self.spawn_tracker.lock().await;
        tracker.kill_all();
        drop(tracker);

        // Exit process (V1: no graceful drain)
        tracing::info!("Exiting");
        std::process::exit(0);
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
async fn ensure_services(
    args: &Args,
    spawn_tracker: &Arc<Mutex<SpawnTracker>>,
) -> Result<()> {
    let backoff_ms = [10, 50, 100, 500, 1000];

    // Check CAS
    if try_connect(&args.cas_endpoint).await.is_err() {
        tracing::info!("CAS not running, spawning vx-casd on {}", args.cas_endpoint);

        let child = spawn_service(
            "vx-casd",
            &[
                ("VX_CAS_ROOT", args.vx_home.as_str()),
                ("VX_CAS_BIND", &args.cas_endpoint),
            ],
        )?;

        spawn_tracker.lock().await.casd = Some(child);

        // Retry connection with backoff
        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.cas_endpoint).await.is_ok() {
                tracing::info!("Connected to CAS after {} attempts", attempt + 1);
                break;
            }
        }

        // Final connection check
        try_connect(&args.cas_endpoint).await?;
    } else {
        tracing::info!("CAS already running at {}", args.cas_endpoint);
    }

    // Check Exec
    if try_connect(&args.exec_endpoint).await.is_err() {
        tracing::info!("Exec not running, spawning vx-execd on {}", args.exec_endpoint);

        let child = spawn_service(
            "vx-execd",
            &[
                ("VX_HOME", args.vx_home.as_str()),
                ("VX_CAS", &args.cas_endpoint),
                ("VX_EXEC", &args.exec_endpoint),
            ],
        )?;

        spawn_tracker.lock().await.execd = Some(child);

        // Retry connection with backoff
        for (attempt, delay) in backoff_ms.iter().enumerate() {
            tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
            if try_connect(&args.exec_endpoint).await.is_ok() {
                tracing::info!("Connected to Exec after {} attempts", attempt + 1);
                break;
            }
        }

        // Final connection check
        try_connect(&args.exec_endpoint).await?;
    } else {
        tracing::info!("Exec already running at {}", args.exec_endpoint);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vx_daemon=info"))
        )
        .init();

    let args = Args::from_env()?;
    tracing::info!("Starting vx-daemon");
    tracing::info!("  VX_HOME: {}", args.vx_home);
    tracing::info!("  CAS:     {}", args.cas_endpoint);
    tracing::info!("  Exec:    {}", args.exec_endpoint);
    tracing::info!("  Bind:    {}", args.bind);

    // Spawn tracker for child services
    let spawn_tracker = Arc::new(Mutex::new(SpawnTracker::default()));

    // Ensure services are running
    ensure_services(&args, &spawn_tracker).await?;

    // Initialize daemon service (V1: still uses in-process CAS/Exec)
    // TODO: Phase 1 Step 3 will refactor this to use TCP clients
    tracing::info!("Initializing daemon service");
    let daemon_service = DaemonService::new(args.vx_home)?;

    let daemon = Arc::new(DaemonWrapper {
        inner: daemon_service,
        spawn_tracker: spawn_tracker.clone(),
    });

    // Start TCP server
    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!("Daemon listening on {}", args.bind);

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        let daemon = Arc::clone(&daemon);

        tokio::spawn(async move {
            tracing::debug!("New connection from {}", peer_addr);

            // Create transport from TCP stream
            let transport = rapace::Transport::stream(socket);

            // Serve the Daemon service
            let server = DaemonServer::new(daemon);
            if let Err(e) = server.serve(transport).await {
                tracing::warn!("Connection error from {}: {}", peer_addr, e);
            }

            tracing::debug!("Connection from {} closed", peer_addr);
        });
    }
}
