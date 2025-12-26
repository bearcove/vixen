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
pub(crate) mod service_vfs;
pub(crate) mod toolchain;
pub(crate) mod vfs;

use error::{Result as RheaResult, RheaError};

use camino::{Utf8Path, Utf8PathBuf};
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

        // Parse VFS endpoint
        // On macOS, default to TCP for fs-kitty URL mounting (fskitty://host:port)
        // On other platforms, default to Unix socket
        let vfs_bind = match std::env::var("VX_RHEA_VFS") {
            Ok(v) => Endpoint::parse(&v)?,
            Err(_) => {
                #[cfg(target_os = "macos")]
                {
                    // Use port 0 to let OS pick an available port
                    Endpoint::parse("127.0.0.1:0")?
                }
                #[cfg(all(unix, not(target_os = "macos")))]
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

/// Check if a socket endpoint is already in use by trying to connect
#[cfg(unix)]
async fn is_socket_in_use(path: &Utf8PathBuf) -> bool {
    use tokio::net::UnixStream;
    // Try to connect - if successful, someone is listening
    UnixStream::connect(path.as_std_path()).await.is_ok()
}

/// Check if fs-kitty extension is installed
#[cfg(target_os = "macos")]
fn check_fskitty_extension() -> Result<(), String> {
    use std::process::Command;

    let output = Command::new("pluginkit")
        .args(["-m", "-p", "com.apple.fskit.fsmodule"])
        .output()
        .map_err(|e| format!("failed to run pluginkit: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.contains("me.amos.fs-kitty") || stdout.contains("fskitty") {
        Ok(())
    } else {
        Err(format!(
            "fs-kitty extension not found.\n\
             Install FsKitty.app and enable the extension in:\n\
             System Settings → General → Login Items & Extensions → File System Extensions"
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn check_fskitty_extension() -> Result<(), String> {
    // On Linux, we'll use FUSE instead - for now just pass
    Ok(())
}

/// Check if there's a stale mount at the rhea mount point
#[cfg(target_os = "macos")]
fn check_stale_mount(rhea_home: &Utf8PathBuf) -> Option<String> {
    use std::process::Command;

    // Check if ~/.rhea is a mount point
    let output = Command::new("mount").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        if line.contains(rhea_home.as_str()) && line.contains("fskitty") {
            return Some(format!(
                "Stale fs-kitty mount detected at {}\n\
                 To clean up, run: umount {} or diskutil unmount force {}",
                rhea_home, rhea_home, rhea_home
            ));
        }
    }
    None
}

/// Mount the VFS at ~/.rhea using fs-kitty
#[cfg(target_os = "macos")]
fn mount_fskitty(mount_point: &Utf8Path, host: &str, port: u16) -> Result<(), String> {
    use std::process::Command;

    // Ensure mount point exists
    if let Err(e) = std::fs::create_dir_all(mount_point) {
        return Err(format!("Failed to create mount point {}: {}", mount_point, e));
    }

    // Mount using: mount -t fskitty fskitty://host:port /mount/point
    let url = format!("fskitty://{}:{}", host, port);
    tracing::info!("Mounting fs-kitty at {} ({})", mount_point, url);

    let output = Command::new("mount")
        .arg("-t")
        .arg("fskitty")
        .arg(&url)
        .arg(mount_point.as_str())
        .output()
        .map_err(|e| format!("Failed to execute mount: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Mount failed: {}", stderr.trim()));
    }

    tracing::info!("VFS mounted at {}", mount_point);
    Ok(())
}

/// Unmount the VFS
#[cfg(target_os = "macos")]
fn unmount_fskitty(mount_point: &Utf8Path) {
    use std::process::Command;

    tracing::info!("Unmounting fs-kitty at {}", mount_point);

    // Try regular unmount first
    let output = Command::new("umount").arg(mount_point.as_str()).output();

    match output {
        Ok(o) if o.status.success() => {
            tracing::info!("VFS unmounted successfully");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("umount failed, trying force unmount: {}", stderr.trim());

            // Try force unmount
            let _ = Command::new("diskutil")
                .arg("unmount")
                .arg("force")
                .arg(mount_point.as_str())
                .output();
        }
        Err(e) => {
            tracing::warn!("Failed to execute umount: {}", e);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn check_stale_mount(_rhea_home: &Utf8PathBuf) -> Option<String> {
    None // Only macOS uses FSKit for now
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

    // Check fs-kitty extension is installed (macOS)
    if let Err(e) = check_fskitty_extension() {
        tracing::warn!("{}", e);
        // Don't fail - extension might be installed but not detected
    }

    // Check for stale mounts
    let rhea_home = args.toolchains_dir.parent().unwrap_or(&args.toolchains_dir);
    if let Some(warning) = check_stale_mount(&rhea_home.to_path_buf()) {
        tracing::warn!("{}", warning);
    }

    // Check if another rhea instance is already running
    #[cfg(unix)]
    {
        if let Endpoint::Unix(ref path) = args.bind {
            if path.exists() && is_socket_in_use(path).await {
                return Err(eyre::eyre!(
                    "Another vx-rhea instance is already running (socket {} is in use).\n\
                     Kill the existing instance or use a different VX_RHEA endpoint.",
                    path
                ));
            }
        }
        if let Endpoint::Unix(ref path) = args.vfs_bind {
            if path.exists() && is_socket_in_use(path).await {
                return Err(eyre::eyre!(
                    "VFS socket {} is already in use.\n\
                     Kill the existing instance or use a different VX_RHEA_VFS endpoint.",
                    path
                ));
            }
        }
    }

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
    let rhea_listener = match Listener::bind(&args.bind).await {
        Ok(l) => l,
        Err(e) => {
            return Err(eyre::eyre!(
                "Failed to bind Rhea RPC socket at {}: {}\n\
                 Another instance may be running, or the socket file is stale.",
                args.bind, e
            ));
        }
    };
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
    let vfs_listener = match Listener::bind(&args.vfs_bind).await {
        Ok(l) => l,
        Err(e) => {
            return Err(eyre::eyre!(
                "Failed to bind VFS socket at {}: {}\n\
                 Another instance may be running, or the socket file is stale.",
                args.vfs_bind, e
            ));
        }
    };

    // Get the actual bound address (important when using port 0)
    let vfs_local_addr = vfs_listener.local_addr()?;
    tracing::info!("VFS (fs-kitty) listening on {}", vfs_local_addr);

    // Mount the VFS on macOS
    #[cfg(target_os = "macos")]
    let mount_point: Option<Utf8PathBuf> = {
        // Extract host and port from the bound address (format: "host:port")
        if let Endpoint::Tcp(addr_str) = &vfs_local_addr {
            // Parse "host:port" string
            let (host, port) = match addr_str.rsplit_once(':') {
                Some((h, p)) => (h, p.parse::<u16>().ok()),
                None => (addr_str.as_str(), None),
            };

            if let Some(port) = port {
                // Mount at ~/.rhea (user's home directory)
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                let mount_path = Utf8PathBuf::from(format!("{}/.rhea", home));
                match mount_fskitty(&mount_path, host, port) {
                    Ok(()) => Some(mount_path),
                    Err(e) => {
                        tracing::warn!("Failed to mount VFS: {}", e);
                        tracing::warn!("Build actions will fail until VFS is mounted");
                        None
                    }
                }
            } else {
                tracing::warn!("Failed to parse port from VFS address: {}", addr_str);
                None
            }
        } else {
            tracing::warn!("VFS endpoint is not TCP, cannot mount fs-kitty");
            None
        }
    };

    // Set up shutdown handler to unmount
    #[cfg(target_os = "macos")]
    let mount_point_for_shutdown = mount_point.clone();

    #[cfg(target_os = "macos")]
    tokio::spawn(async move {
        // Wait for ctrl-c
        if let Ok(()) = tokio::signal::ctrl_c().await {
            if let Some(mp) = mount_point_for_shutdown {
                unmount_fskitty(&mp);
            }
            std::process::exit(0);
        }
    });

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

    // ========================================================================
    // VFS Prefix Management (for hermetic action execution)
    // ========================================================================

    /// Create a new action prefix in the VFS
    ///
    /// Returns the prefix ID (e.g., "build-01JFX...") which can be used
    /// to add files and later clean up.
    pub(crate) async fn create_action_prefix(&self) -> String {
        use jiff::Timestamp;
        // Generate a unique prefix ID using timestamp + random suffix
        let ts = Timestamp::now().as_millisecond();
        let prefix_id = format!("build-{:x}-{:04x}", ts, rand_u16());
        let (id, _item_id) = self.vfs.create_prefix(prefix_id).await;
        id
    }

    /// Add a toolchain to an action prefix
    ///
    /// Populates `<prefix>/toolchain/` with the toolchain's files from CAS.
    pub(crate) async fn add_toolchain_to_prefix(
        &self,
        prefix_id: &str,
        toolchain_manifest_hash: Blake3Hash,
    ) -> RheaResult<()> {
        // Get the toolchain manifest from CAS
        let manifest = self
            .cas
            .get_manifest(toolchain_manifest_hash)
            .await
            .map_err(|e| RheaError::CasFetch(format!("{:?}", e)))?
            .ok_or_else(|| {
                RheaError::CasFetch(format!(
                    "toolchain manifest {} not found",
                    toolchain_manifest_hash
                ))
            })?;

        // Add each output to the VFS prefix under toolchain/
        for output in &manifest.outputs {
            let path = format!("toolchain/{}", output.filename);
            self.vfs
                .add_file_to_prefix(
                    prefix_id,
                    &path,
                    output.blob,
                    0, // Size unknown, will be fetched on read
                    output.executable,
                )
                .await;
        }

        Ok(())
    }

    /// Add a source tree to an action prefix
    ///
    /// Populates `<prefix>/src/` with the source tree's files from CAS.
    pub(crate) async fn add_source_tree_to_prefix(
        &self,
        prefix_id: &str,
        source_manifest_hash: Blake3Hash,
    ) -> RheaResult<()> {
        // Get the source manifest from CAS
        let manifest = self
            .cas
            .get_manifest(source_manifest_hash)
            .await
            .map_err(|e| RheaError::CasFetch(format!("{:?}", e)))?
            .ok_or_else(|| {
                RheaError::CasFetch(format!(
                    "source manifest {} not found",
                    source_manifest_hash
                ))
            })?;

        // Add each file to the VFS prefix under src/
        for output in &manifest.outputs {
            let path = format!("src/{}", output.filename);
            self.vfs
                .add_file_to_prefix(prefix_id, &path, output.blob, 0, output.executable)
                .await;
        }

        Ok(())
    }

    /// Add a dependency artifact to an action prefix
    ///
    /// Places the dependency's rlib at `<prefix>/deps/<extern_name>.rlib`
    pub(crate) async fn add_dep_to_prefix(
        &self,
        prefix_id: &str,
        extern_name: &str,
        manifest_hash: Blake3Hash,
    ) -> RheaResult<()> {
        // Get the dependency manifest
        let manifest = self
            .cas
            .get_manifest(manifest_hash)
            .await
            .map_err(|e| RheaError::CasFetch(format!("{:?}", e)))?
            .ok_or_else(|| {
                RheaError::CasFetch(format!("dependency manifest {} not found", manifest_hash))
            })?;

        // Find the rlib output
        let rlib = manifest
            .outputs
            .iter()
            .find(|o| o.logical == "rlib")
            .ok_or_else(|| {
                RheaError::CasFetch(format!("no rlib in manifest for {}", extern_name))
            })?;

        let path = format!("deps/lib{}.rlib", extern_name);
        self.vfs
            .add_file_to_prefix(prefix_id, &path, rlib.blob, 0, false)
            .await;

        Ok(())
    }

    /// Create the output directory in a prefix
    pub(crate) async fn create_output_dir_in_prefix(&self, prefix_id: &str) {
        self.vfs.add_dir_to_prefix(prefix_id, "out").await;
    }

    /// Flush all written files in a prefix to CAS
    ///
    /// Returns a map of relative paths (e.g., "out/libfoo.rlib") to CAS hashes.
    pub(crate) async fn flush_prefix_outputs(
        &self,
        prefix_id: &str,
    ) -> HashMap<String, Blake3Hash> {
        self.vfs.flush_prefix_to_cas(prefix_id).await
    }

    /// Clean up an action prefix
    pub(crate) async fn remove_action_prefix(&self, prefix_id: &str) {
        self.vfs.remove_prefix(prefix_id).await;
    }

    /// Get the VFS mount path for an action prefix
    ///
    /// This is the path that should be passed to sandboxed processes.
    /// Format: ~/.rhea/<prefix_id>/
    #[allow(dead_code)] // Will be used for sandbox integration
    pub(crate) fn get_prefix_mount_path(&self, prefix_id: &str) -> Utf8PathBuf {
        // For now, return a path relative to rhea home
        // In the future, this will be the actual mount point
        let rhea_home = self
            .toolchains_dir
            .parent()
            .unwrap_or(&self.toolchains_dir);
        rhea_home.join(prefix_id)
    }
}

/// Generate a random u16 for prefix uniqueness
fn rand_u16() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    (nanos & 0xFFFF) as u16
}
