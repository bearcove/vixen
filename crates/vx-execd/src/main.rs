//! vx-execd - Execution daemon
//!
//! Implements the Exec rapace service trait.
//! Runs rustc and zig cc, streams outputs to CAS.
//! Materializes toolchains and registry crates on-demand from CAS.

pub(crate) mod registry;
pub(crate) mod service;

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

    let exec = ExecService::new(Arc::new(cas), args.toolchains_dir, args.registry_cache_dir);

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

/// Exec service implementation
pub struct ExecService {
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

impl ExecService {
    pub fn new(
        cas: Arc<CasClient>,
        toolchains_dir: Utf8PathBuf,
        registry_cache_dir: Utf8PathBuf,
    ) -> Self {
        Self {
            registry_materializer: RegistryMaterializer::new(cas.clone(), registry_cache_dir),
            cas,
            toolchains_dir,
            materializing: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
        info!(manifest_hash = %manifest_hash, "materializing toolchain");

        // Fetch manifest from CAS
        let _manifest = self
            .cas
            .get_toolchain_manifest(manifest_hash)
            .await
            .ok_or_else(|| format!("toolchain manifest {} not found in CAS", manifest_hash))?;

        // Fetch materialization plan
        let plan = self
            .cas
            .get_materialization_plan(manifest_hash)
            .await
            .ok_or_else(|| {
                format!(
                    "materialization plan for {} not found in CAS",
                    manifest_hash
                )
            })?;

        // Target directory: toolchains/<manifest_hash_hex>
        let target_dir = self.toolchains_dir.join(manifest_hash.to_hex());

        // Check if already materialized (using tokio::fs)
        if tokio::fs::try_exists(&target_dir).await.unwrap_or(false) {
            debug!(target_dir = %target_dir, "toolchain already materialized");
            return Ok(target_dir);
        }

        // Create temp directory for atomic materialization
        let temp_dir = self
            .toolchains_dir
            .join(format!("{}.tmp", manifest_hash.to_hex()));
        if tokio::fs::try_exists(&temp_dir).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&temp_dir)
                .await
                .map_err(|e| format!("failed to remove stale temp dir {}: {}", temp_dir, e))?;
        }
        tokio::fs::create_dir_all(&temp_dir)
            .await
            .map_err(|e| format!("failed to create temp dir {}: {}", temp_dir, e))?;

        // Execute materialization plan
        for step in &plan.steps {
            match step {
                MaterializeStep::ExtractTarXz {
                    blob,
                    dest_subdir,
                    strip_components,
                } => {
                    let dest = temp_dir.join(dest_subdir);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create dest dir {}: {}", dest, e))?;

                    self.extract_tar_xz_from_cas(*blob, &dest, *strip_components)
                        .await?;
                }
                MaterializeStep::EnsureDir { relpath } => {
                    let dest = temp_dir.join(relpath);
                    tokio::fs::create_dir_all(&dest)
                        .await
                        .map_err(|e| format!("failed to create directory {}: {}", dest, e))?;
                }
                MaterializeStep::WriteFile {
                    relpath,
                    blob,
                    mode,
                } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }

                    // Fetch blob from CAS
                    let mut stream = self.cas.stream_blob(*blob).await;
                    let mut data = Vec::new();
                    while let Some(chunk_result) = stream.next().await {
                        let chunk =
                            chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
                        data.extend_from_slice(&chunk);
                    }

                    tokio::fs::write(&dest, data)
                        .await
                        .map_err(|e| format!("failed to write file {}: {}", dest, e))?;

                    // Set permissions
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(*mode);
                        tokio::fs::set_permissions(&dest, perms)
                            .await
                            .map_err(|e| format!("failed to set permissions on {}: {}", dest, e))?;
                    }
                }
                MaterializeStep::Symlink { relpath, target } => {
                    let dest = temp_dir.join(relpath);
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            format!("failed to create parent directory {}: {}", parent, e)
                        })?;
                    }
                    #[cfg(unix)]
                    {
                        tokio::fs::symlink(target, &dest)
                            .await
                            .map_err(|e| format!("failed to create symlink {}: {}", dest, e))?;
                    }
                    #[cfg(not(unix))]
                    {
                        return Err(format!("symlinks not supported on this platform"));
                    }
                }
            }
        }

        // Atomic rename to final location
        tokio::fs::rename(&temp_dir, &target_dir)
            .await
            .map_err(|e| format!("failed to rename {} to {}: {}", temp_dir, target_dir, e))?;

        info!(target_dir = %target_dir, "toolchain materialized successfully");
        Ok(target_dir)
    }

    /// Extract a tar.xz blob from CAS to a destination directory
    async fn extract_tar_xz_from_cas(
        &self,
        blob_hash: Blake3Hash,
        dest: &Utf8Path,
        strip_components: u32,
    ) -> Result<(), String> {
        debug!(blob = %blob_hash, dest = %dest, strip = strip_components, "extracting tar.xz");

        // Stream blob from CAS
        let mut stream = self.cas.stream_blob(blob_hash).await;
        let mut compressed_data = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("failed to stream blob: {:?}", e))?;
            compressed_data.extend_from_slice(&chunk);
        }

        // CPU-bound: decompress and extract in spawn_blocking
        let dest = dest.to_owned();
        tokio::task::spawn_blocking(move || {
            // Decompress with xz2
            let mut decompressor = xz2::read::XzDecoder::new(&compressed_data[..]);
            let mut tarball_data = Vec::new();
            decompressor
                .read_to_end(&mut tarball_data)
                .map_err(|e| format!("failed to decompress tar.xz: {}", e))?;

            // Extract tar
            let mut archive = tar::Archive::new(&tarball_data[..]);
            for entry in archive
                .entries()
                .map_err(|e| format!("failed to read tar entries: {}", e))?
            {
                let mut entry = entry.map_err(|e| format!("failed to read tar entry: {}", e))?;
                let path = entry
                    .path()
                    .map_err(|e| format!("failed to get entry path: {}", e))?;

                // Strip components
                let components: Vec<_> = path.components().collect();
                if components.len() <= strip_components as usize {
                    continue;
                }
                let stripped_path = Utf8PathBuf::from_path_buf(
                    components[strip_components as usize..]
                        .iter()
                        .collect::<std::path::PathBuf>(),
                )
                .map_err(|_| "non-UTF8 path in tarball".to_string())?;

                // Security: validate no path traversal (use component-based check to avoid
                // false positives on valid filenames like "foo..rs")
                for component in stripped_path.components() {
                    if matches!(component, camino::Utf8Component::ParentDir) {
                        return Err(format!("path traversal in tarball: {}", stripped_path));
                    }
                }

                let target_path = dest.join(&stripped_path);

                // Create parent directories
                if let Some(parent) = target_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create directory {}: {}", parent, e))?;
                }

                // Extract file
                let mut output_file = std::fs::File::create(&target_path)
                    .map_err(|e| format!("failed to create file {}: {}", target_path, e))?;
                std::io::copy(&mut entry, &mut output_file)
                    .map_err(|e| format!("failed to write file {}: {}", target_path, e))?;

                // Set executable bit if needed
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mode) = entry.header().mode() {
                        if mode & 0o111 != 0 {
                            let perms = std::fs::Permissions::from_mode(mode);
                            std::fs::set_permissions(&target_path, perms).map_err(|e| {
                                format!("failed to set permissions on {}: {}", target_path, e)
                            })?;
                        }
                    }
                }
            }

            Ok(())
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
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
