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

/// Splits a Server-Sent Events byte stream into the `data` of each event.
///
/// An event ends at a blank line. A line ends at `\r\n`, `\n` or `\r`. A
/// network chunk can end inside a line, inside a `\r\n` pair or inside a
/// multi-byte character. So the parser keeps bytes, and it decodes a line
/// only when the line is whole. The rest waits for the next chunk.
#[derive(Debug, Default)]
pub struct SseFrames {
    /// The bytes of a line that is not whole yet.
    pending: Vec<u8>,
    /// The `data` lines of the event that is not whole yet.
    data: Vec<String>,
    /// The last chunk ended with `\r`. A `\n` at the start of the next
    /// chunk is the rest of that line end.
    after_cr: bool,
}

impl SseFrames {
    /// Add `bytes`. Returns the data of each event that they complete.
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<String> {
        if self.after_cr && !bytes.is_empty() {
            self.after_cr = false;
            bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
        }
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(at) = self.pending[start..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
        {
            let end = start + at;
            let mut next = end + 1;
            if self.pending[end] == b'\r' {
                match self.pending.get(next) {
                    Some(b'\n') => next += 1,
                    Some(_) => {}
                    // The `\n` of a `\r\n` pair can come in the next chunk.
                    None => self.after_cr = true,
                }
            }
            let line = String::from_utf8_lossy(&self.pending[start..end]).into_owned();
            self.line(&line, &mut out);
            start = next;
        }
        self.pending.drain(..start);
        out
    }

    /// The data of an event that the stream ended without a blank line.
    pub fn end(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.pending);
        self.after_cr = false;
        let mut out = Vec::new();
        if !rest.is_empty() {
            self.line(&String::from_utf8_lossy(&rest), &mut out);
        }
        self.line("", &mut out);
        out
    }

    /// Apply one whole line. A blank line ends the event.
    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_join_data_lines_and_skip_comments() {
        let mut frames = SseFrames::default();
        assert_eq!(
            frames.push(b": ping\n\nevent: a\r\ndata: 1"),
            Vec::<String>::new()
        );
        assert_eq!(frames.push(b"\r\ndata: 2\r\n\r\n"), ["1\n2"]);
        assert_eq!(frames.push(b"data: 3"), Vec::<String>::new());
        assert_eq!(frames.end(), ["3"]);
    }

    #[test]
    fn a_line_end_split_across_chunks_or_a_bare_cr_ends_a_line_once() {
        let mut frames = SseFrames::default();
        // A `\r\n` pair split over two chunks is one line end.
        assert_eq!(frames.push(b"data: 1\r"), Vec::<String>::new());
        assert_eq!(frames.push(b"\ndata: 2\r"), Vec::<String>::new());
        assert_eq!(frames.push(b"\n\r"), ["1\n2"]);
        assert_eq!(frames.push(b"\n"), Vec::<String>::new());
        // A bare `\r` ends a line, and a blank one ends the event.
        assert_eq!(frames.push(b"data: 3\rdata: 4\r\r"), ["3\n4"]);
        assert_eq!(frames.end(), Vec::<String>::new());
    }
}
