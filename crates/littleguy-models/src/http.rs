//! The HTTP pieces that every remote model shares.

use std::time::Duration;

use autumn_harvest_agent::{AgentError, ErrorKind};

/// How long one request may take, thinking included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// The most bytes of one response body that a model reads.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A client with the request and connect timeouts.
pub fn client() -> reqwest::Client {
    // Reqwest panics without a provider. An `Err` means that one is installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// Read at most [`MAX_BODY_BYTES`] of `response`.
///
/// # Errors
///
/// A lost connection is a `Transport` error. A longer body is a `Decode`
/// error.
pub async fn read_body(mut response: reqwest::Response) -> Result<String, AgentError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AgentError::new(ErrorKind::Transport, format!("response lost: {e}")))?
    {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(AgentError::new(
                ErrorKind::Decode,
                format!("the response is over {MAX_BODY_BYTES} bytes"),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The error kind of an HTTP status. A timeout, a conflict, a rate limit and
/// a server fault retry, as the SDKs retry them.
#[must_use]
pub const fn error_kind(status: u16) -> ErrorKind {
    match status {
        429 => ErrorKind::RateLimited,
        401 | 403 => ErrorKind::Authentication,
        408 | 409 | 500..=599 => ErrorKind::Unavailable,
        _ => ErrorKind::Provider,
    }
}
