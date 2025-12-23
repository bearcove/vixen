//! Simple HTTP client using hyper

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::OnceLock;
use thiserror::Error;

type HttpsConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

/// Maximum number of redirects to follow
const MAX_REDIRECTS: u8 = 10;

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid URI: {0}")]
    InvalidUri(#[from] hyper::http::uri::InvalidUri),

    #[error("HTTP request failed: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("HTTP error {0}")]
    HttpError(#[from] hyper_util::client::legacy::Error),

    #[error("HTTP builder error: {0}")]
    HttpBuilder(#[from] hyper::http::Error),

    #[error("invalid UTF-8 in response body")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    #[error("HTTP {status}")]
    Status { status: StatusCode },

    #[error("too many redirects")]
    TooManyRedirects,

    #[error("redirect without Location header")]
    MissingLocation,
}

/// Get a shared HTTPS client instance
fn client() -> &'static Client<HttpsConnector, String> {
    static CLIENT: OnceLock<Client<HttpsConnector, String>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("failed to load native roots")
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();

        Client::builder(TokioExecutor::new()).build(https)
    })
}

/// Perform a simple GET request and return the response, following redirects
pub async fn get(url: &str) -> Result<Response<Incoming>, HttpError> {
    let mut current_url = url.to_string();

    for _ in 0..MAX_REDIRECTS {
        let req = Request::builder()
            .uri(&current_url)
            .body(String::new())?;

        let response = client().request(req).await?;
        let status = response.status();

        // Follow redirects (301, 302, 303, 307, 308)
        if status.is_redirection() {
            let location = response
                .headers()
                .get(hyper::header::LOCATION)
                .ok_or(HttpError::MissingLocation)?
                .to_str()
                .map_err(|_| HttpError::MissingLocation)?;

            // Handle relative URLs by resolving against current URL
            current_url = if location.starts_with('/') {
                // Relative path - extract scheme + host from current URL
                let uri: hyper::Uri = current_url.parse()?;
                format!(
                    "{}://{}{}",
                    uri.scheme_str().unwrap_or("https"),
                    uri.host().unwrap_or(""),
                    location
                )
            } else {
                location.to_string()
            };

            tracing::debug!(from = %url, to = %current_url, "following redirect");
            continue;
        }

        return Ok(response);
    }

    Err(HttpError::TooManyRedirects)
}

/// GET request and collect the full response body as bytes
pub async fn get_bytes(url: &str) -> Result<Vec<u8>, HttpError> {
    use http_body_util::BodyExt;

    let response = get(url).await?;
    let status = response.status();

    if !status.is_success() {
        return Err(HttpError::Status { status });
    }

    let body = response.into_body().collect().await?.to_bytes();
    Ok(body.to_vec())
}

/// GET request and collect the response as a UTF-8 string
pub async fn get_text(url: &str) -> Result<String, HttpError> {
    let bytes = get_bytes(url).await?;
    let text = String::from_utf8(bytes)?;
    Ok(text)
}
