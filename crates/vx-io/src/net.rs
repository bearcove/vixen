use eyre::Result;

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
        eyre::bail!("unsupported endpoint scheme (expected tcp:// or host:port): {}", endpoint);
    }
    Ok(endpoint.to_string())
}

/// Best-effort check for whether an endpoint is loopback-local.
///
/// This intentionally does not do DNS resolution. Hostnames other than `localhost`
/// are treated as non-loopback.
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    let Ok(endpoint) = normalize_tcp_endpoint(endpoint) else {
        return false;
    };

    if endpoint.starts_with("localhost:") {
        return true;
    }

    // SocketAddr parsing covers IPv4 literals and bracketed IPv6 literals.
    if let Ok(addr) = endpoint.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }

    false
}

