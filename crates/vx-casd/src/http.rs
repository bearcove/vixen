//! Simple HTTP client using hyper

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::sync::OnceLock;
use thiserror::Error;

type HttpsConnector =
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>;

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

/// Perform a simple GET request and return the response
pub async fn get(url: &str) -> Result<Response<Incoming>, HttpError> {
    let req = Request::builder().uri(url).body(String::new())?;

    let response = client().request(req).await?;
    Ok(response)
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
