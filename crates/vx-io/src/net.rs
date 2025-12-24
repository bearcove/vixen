use camino::Utf8Path;
use eyre::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A network endpoint - either TCP or Unix socket
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// TCP endpoint (host:port)
    Tcp(String),
    /// Unix socket path
    #[cfg(unix)]
    Unix(camino::Utf8PathBuf),
}

impl Endpoint {
    /// Parse an endpoint string.
    ///
    /// Accepts:
    /// - `host:port` or `tcp://host:port` → TCP
    /// - `unix:/path/to/socket` → Unix socket
    /// - Absolute path starting with `/` → Unix socket (convenience)
    pub fn parse(endpoint: &str) -> Result<Self> {
        let endpoint = endpoint.trim();

        // Unix socket: unix:/path or just /path
        #[cfg(unix)]
        {
            if let Some(path) = endpoint.strip_prefix("unix:") {
                return Ok(Endpoint::Unix(camino::Utf8PathBuf::from(path)));
            }
            if endpoint.starts_with('/') {
                return Ok(Endpoint::Unix(camino::Utf8PathBuf::from(endpoint)));
            }
        }

        // TCP: tcp://host:port or host:port
        if let Some(rest) = endpoint.strip_prefix("tcp://") {
            return Ok(Endpoint::Tcp(rest.to_string()));
        }

        // Reject unknown schemes
        if endpoint.contains("://") {
            eyre::bail!(
                "unsupported endpoint scheme (expected tcp://, unix:, or host:port): {}",
                endpoint
            );
        }

        // Default: TCP
        Ok(Endpoint::Tcp(endpoint.to_string()))
    }

    /// Check if this endpoint is local (loopback or unix socket)
    pub fn is_local(&self) -> bool {
        match self {
            Endpoint::Tcp(addr) => is_loopback_tcp(addr),
            #[cfg(unix)]
            Endpoint::Unix(_) => true,
        }
    }

    /// Get a display string for this endpoint
    pub fn display(&self) -> String {
        match self {
            Endpoint::Tcp(addr) => addr.clone(),
            #[cfg(unix)]
            Endpoint::Unix(path) => format!("unix:{}", path),
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

/// A stream that can be either TCP or Unix socket
pub enum Stream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            Stream::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Stream::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Stream::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Stream::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Connect to an endpoint
pub async fn connect(endpoint: &Endpoint) -> Result<Stream> {
    match endpoint {
        Endpoint::Tcp(addr) => {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            Ok(Stream::Tcp(stream))
        }
        #[cfg(unix)]
        Endpoint::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path.as_std_path()).await?;
            Ok(Stream::Unix(stream))
        }
    }
}

/// Try to connect to an endpoint, returning None if connection refused
pub async fn try_connect(endpoint: &Endpoint) -> Result<Option<Stream>> {
    match connect(endpoint).await {
        Ok(stream) => Ok(Some(stream)),
        Err(e) => {
            // Check if it's a connection refused error
            if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                if io_err.kind() == std::io::ErrorKind::ConnectionRefused {
                    return Ok(None);
                }
                // Also treat "No such file or directory" as not running (Unix socket)
                if io_err.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
            }
            Err(e)
        }
    }
}

/// A listener that can accept connections from an endpoint
pub enum Listener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

impl Listener {
    /// Bind to an endpoint
    pub async fn bind(endpoint: &Endpoint) -> Result<Self> {
        match endpoint {
            Endpoint::Tcp(addr) => {
                let listener = tokio::net::TcpListener::bind(addr).await?;
                Ok(Listener::Tcp(listener))
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                // Remove existing socket file if it exists
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
                // Ensure parent directory exists
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let listener = tokio::net::UnixListener::bind(path.as_std_path())?;
                Ok(Listener::Unix(listener))
            }
        }
    }

    /// Accept a connection, returning the stream and peer address string
    pub async fn accept(&self) -> Result<(Stream, String)> {
        match self {
            Listener::Tcp(listener) => {
                let (stream, addr) = listener.accept().await?;
                Ok((Stream::Tcp(stream), addr.to_string()))
            }
            #[cfg(unix)]
            Listener::Unix(listener) => {
                let (stream, _addr) = listener.accept().await?;
                Ok((Stream::Unix(stream), "unix".to_string()))
            }
        }
    }

    /// Get the local address this listener is bound to
    pub fn local_addr(&self) -> Result<Endpoint> {
        match self {
            Listener::Tcp(listener) => {
                let addr = listener.local_addr()?;
                Ok(Endpoint::Tcp(addr.to_string()))
            }
            #[cfg(unix)]
            Listener::Unix(listener) => {
                let addr = listener.local_addr()?;
                if let Some(path) = addr.as_pathname() {
                    Ok(Endpoint::Unix(camino::Utf8PathBuf::try_from(
                        path.to_path_buf(),
                    )?))
                } else {
                    eyre::bail!("Unix socket has no pathname")
                }
            }
        }
    }
}

/// Create a default endpoint path for a service within VX_HOME
#[cfg(unix)]
pub fn default_unix_endpoint(vx_home: &Utf8Path, service: &str) -> Endpoint {
    Endpoint::Unix(vx_home.join(format!("{}.sock", service)))
}

// --- Legacy API for backwards compatibility ---

/// Normalize a TCP endpoint string to something `TcpStream::connect` / `TcpListener::bind` accepts.
///
/// Accepts:
/// - `host:port`
/// - `tcp://host:port`
///
/// Rejects other schemes (e.g. `http://`).
pub fn normalize_tcp_endpoint(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim();
    if let Some(rest) = endpoint.strip_prefix("tcp://") {
        return Ok(rest.to_string());
    }
    if endpoint.contains("://") {
        eyre::bail!(
            "unsupported endpoint scheme (expected tcp:// or host:port): {}",
            endpoint
        );
    }
    Ok(endpoint.to_string())
}

/// Best-effort check for whether an endpoint is loopback-local.
///
/// This intentionally does not do DNS resolution. Hostnames other than `localhost`
/// are treated as non-loopback.
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    // Check for Unix socket first
    #[cfg(unix)]
    {
        if endpoint.starts_with("unix:") || endpoint.starts_with('/') {
            return true;
        }
    }

    let Ok(endpoint) = normalize_tcp_endpoint(endpoint) else {
        return false;
    };

    is_loopback_tcp(&endpoint)
}

/// Check if a TCP address is loopback
fn is_loopback_tcp(addr: &str) -> bool {
    if addr.starts_with("localhost:") {
        return true;
    }

    // SocketAddr parsing covers IPv4 literals and bracketed IPv6 literals.
    if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }

    false
}
