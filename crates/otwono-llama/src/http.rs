//! A very small HTTP/1.1 client that speaks over a Unix domain socket.
//!
//! # Why a Unix socket, and therefore why this file
//!
//! `llama-server` binds a Unix socket when `--host` ends in `.sock`. We use that rather
//! than a loopback TCP port, and the reason is a security boundary, not tidiness: a port
//! on `127.0.0.1` is reachable by **every local user**. On a shared machine that would let
//! any account drive the inference engine, read whatever is in flight, and inject prompts
//! of its own. A socket in a `0700` directory is protected by the filesystem, which is the
//! same boundary the rest of the control plane already relies on (ADR-0003).
//!
//! That choice is what rules out the obvious HTTP crates: they are built around TCP, and
//! bending one onto a `UnixStream` is more code — and more surprising code — than the
//! hundred lines below. This is not a general-purpose client and must not grow into one.
//! It talks to one known server, on one socket, over one connection per request.
//!
//! # What it deliberately does not do
//!
//! No redirects, no keep-alive, no TLS, no compression, no cookies, no proxy support. If
//! any of those ever become necessary, that is the signal to take a dependency rather than
//! to extend this.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Largest response body we will accumulate.
///
/// A completion response is text, but the engine is a large C++ program and this is the
/// same reasoning as the supervisor's line cap: an unbounded read lets a confused backend
/// exhaust the daemon's memory. 64 MiB is far above any real completion and far below
/// anything that would hurt.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Largest header section we will accept, and the most header lines.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 200;

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, lossily. Used for error reporting, where a panic on invalid
    /// UTF-8 from a failing engine would replace the diagnosis with a crash.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// Perform one request and read one response.
///
/// `timeout` bounds **inactivity**, not total duration: it is applied to each read from
/// the socket. For a non-streaming completion the engine sends nothing until it is
/// finished, so in practice the two are the same thing here — but a future streaming path
/// would need its own total-duration budget on top, and it would be wrong to read this as
/// one.
pub fn request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse, HttpError> {
    let stream = UnixStream::connect(socket).map_err(|e| HttpError::Connect {
        socket: socket.display().to_string(),
        reason: e.to_string(),
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(HttpError::io)?;
    stream.set_write_timeout(Some(timeout)).map_err(HttpError::io)?;

    let mut stream = stream;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(HttpError::io)?;
    if let Some(b) = body {
        stream.write_all(b).map_err(HttpError::io)?;
    }
    stream.flush().map_err(HttpError::io)?;

    read_response(BufReader::new(stream))
}

fn read_response<R: Read>(mut reader: BufReader<R>) -> Result<HttpResponse, HttpError> {
    let status_line = read_header_line(&mut reader)?;
    let status = parse_status(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut header_bytes = status_line.len();
    for _ in 0..MAX_HEADER_LINES {
        let line = read_header_line(&mut reader)?;
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(HttpError::Malformed("header section too large".into()));
        }
        if line.is_empty() {
            return read_body(reader, status, content_length, chunked);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Malformed(format!("header without a colon: {line:?}")));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| HttpError::Malformed(format!("bad Content-Length: {value:?}")))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }
    Err(HttpError::Malformed("too many header lines".into()))
}

fn read_body<R: Read>(
    mut reader: BufReader<R>,
    status: u16,
    content_length: Option<usize>,
    chunked: bool,
) -> Result<HttpResponse, HttpError> {
    let body = if chunked {
        // cpp-httplib chunks anything it streams, so this branch is not hypothetical.
        read_chunked(&mut reader)?
    } else if let Some(len) = content_length {
        if len > MAX_RESPONSE_BYTES {
            return Err(HttpError::TooLarge(len));
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).map_err(HttpError::io)?;
        buf
    } else {
        // No length and no chunking: the body runs to end of connection, which is legal
        // for `Connection: close` and is what we asked for.
        let mut buf = Vec::new();
        Read::by_ref(&mut reader)
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(HttpError::io)?;
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(HttpError::TooLarge(buf.len()));
        }
        buf
    };
    Ok(HttpResponse { status, body })
}

fn read_chunked<R: Read>(reader: &mut BufReader<R>) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = read_header_line(reader)?;
        // A chunk size may carry extensions after a semicolon; we ignore them.
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Malformed(format!("bad chunk size: {size_text:?}")))?;
        if size == 0 {
            // Trailers, then a blank line. Read until blank rather than assuming none.
            for _ in 0..MAX_HEADER_LINES {
                if read_header_line(reader)?.is_empty() {
                    return Ok(body);
                }
            }
            return Err(HttpError::Malformed("too many trailer lines".into()));
        }
        if body.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err(HttpError::TooLarge(body.len() + size));
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk).map_err(HttpError::io)?;
        body.extend_from_slice(&chunk);
        // The CRLF that terminates the chunk.
        let trailing = read_header_line(reader)?;
        if !trailing.is_empty() {
            return Err(HttpError::Malformed("chunk not terminated by CRLF".into()));
        }
    }
}

/// Read one CRLF-terminated line, returned without its terminator.
fn read_header_line<R: Read>(reader: &mut BufReader<R>) -> Result<String, HttpError> {
    let mut line = Vec::new();
    let read = Read::by_ref(reader)
        .take(MAX_HEADER_BYTES as u64)
        .read_until(b'\n', &mut line)
        .map_err(HttpError::io)?;
    if read == 0 {
        return Err(HttpError::Closed);
    }
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| HttpError::Malformed("header line is not UTF-8".into()))
}

fn parse_status(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.split(' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Malformed(format!(
            "not an HTTP/1.x response: {line:?}"
        )));
    }
    parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::Malformed(format!("no status code in {line:?}")))
}

#[derive(Debug)]
pub enum HttpError {
    Connect {
        socket: String,
        reason: String,
    },
    Io(String),
    /// The peer closed the connection before a complete response.
    Closed,
    Malformed(String),
    TooLarge(usize),
}

impl HttpError {
    fn io(e: std::io::Error) -> Self {
        HttpError::Io(e.to_string())
    }

    /// Whether this looks like "the server is not listening yet", which is the normal
    /// state while an engine is still starting up and must not be treated as a failure.
    pub fn is_not_listening(&self) -> bool {
        matches!(self, HttpError::Connect { .. })
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Connect { socket, reason } => write!(f, "cannot connect to {socket}: {reason}"),
            HttpError::Io(e) => write!(f, "socket error: {e}"),
            HttpError::Closed => write!(f, "the engine closed the connection mid-response"),
            HttpError::Malformed(what) => write!(f, "malformed HTTP response: {what}"),
            HttpError::TooLarge(n) => write!(
                f,
                "response body of {n} bytes exceeds the {MAX_RESPONSE_BYTES} byte cap"
            ),
        }
    }
}

impl std::error::Error for HttpError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<HttpResponse, HttpError> {
        read_response(BufReader::new(std::io::Cursor::new(raw.as_bytes().to_vec())))
    }

    #[test]
    fn a_content_length_response_is_read_exactly() {
        let r = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert!(r.is_success());
    }

    #[test]
    fn a_chunked_response_is_reassembled() {
        // cpp-httplib chunks streamed bodies, so this is the shape we actually meet.
        let r = parse(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn chunk_extensions_are_ignored_rather_than_rejected() {
        let r = parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n0\r\n\r\n")
            .unwrap();
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn a_body_that_runs_to_end_of_connection_is_accepted() {
        let r = parse("HTTP/1.1 200 OK\r\n\r\nno length here").unwrap();
        assert_eq!(r.body, b"no length here");
    }

    #[test]
    fn an_error_status_is_returned_rather_than_raised() {
        // The engine's 4xx bodies carry the reason a model would not load. Turning them
        // into a transport error here would throw that away.
        let r = parse("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 16\r\n\r\n{\"error\":\"busy\"}")
            .unwrap();
        assert_eq!(r.status, 503);
        assert!(!r.is_success());
        assert!(r.text().contains("busy"));
    }

    #[test]
    fn a_truncated_body_is_an_error_not_a_short_read() {
        let err = parse("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort").unwrap_err();
        assert!(matches!(err, HttpError::Io(_)), "{err:?}");
    }

    #[test]
    fn a_non_http_greeting_is_rejected_immediately() {
        // What a wrapper script's error message looks like on the wire.
        let err = parse("bash: llama-server: command not found\r\n").unwrap_err();
        assert!(matches!(err, HttpError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn an_oversized_content_length_is_refused_without_allocating_it() {
        let err = parse(&format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            u32::MAX
        ))
        .unwrap_err();
        assert!(matches!(err, HttpError::TooLarge(_)), "{err:?}");
    }

    #[test]
    fn an_empty_response_is_a_closed_connection_not_a_parse_error() {
        assert!(matches!(parse("").unwrap_err(), HttpError::Closed));
    }
}
