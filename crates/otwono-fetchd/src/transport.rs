//! The HTTP transport, behind a trait.
//!
//! The trait exists so that everything above it — redirect admission, resumption, byte
//! budgets, the spool state machine — is testable without a network, a TLS stack, or a
//! remote host willing to misbehave on cue. A real server does not return a `3xx` to an
//! attacker-chosen host when you ask it to; a test double does.
//!
//! # What the client is not allowed to decide
//!
//! Redirects are disabled at the client. A `3xx` is a server proposing a different
//! request, and whether that request is permitted is a question about the operator's
//! allow-list — which is this daemon's business and not a library's. Similarly there is
//! one fixed `User-Agent` and no way for a caller to add a header, because every byte a
//! caller can put on the wire is a byte that leaves this node (ADR-0014).

use std::io::{Read, Write};
use std::time::Duration;

/// A fixed identity. Not caller-settable: a `User-Agent` is a free-text field going out
/// over the network, which is exactly the sort of channel this design closes.
pub const USER_AGENT: &str = concat!("otwono-fetchd/", env!("CARGO_PKG_VERSION"));

/// How many `3xx` hops we will consider before giving up. Each is admission-checked; the
/// cap is against a server that redirects in a circle.
pub const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub uri: &'a http::Uri,
    /// Resume offset. Sent as `Range: bytes=<n>-`; zero means no `Range` header at all.
    pub range_from: u64,
    /// Wall-clock budget for this one request.
    pub timeout: Duration,
}

/// What a response said, separated from what it contained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub etag: Option<String>,
    /// Bytes in the whole object, when the response says so — from `Content-Range` on a
    /// `206`, or `Content-Length` on a `200`.
    pub total_bytes: Option<u64>,
    /// `Location`, verbatim. Resolving and admitting it is the caller's job.
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Could not reach the host, TLS failed, or the connection died.
    Unreachable(String),
    /// The exchange started and then went wrong.
    Io(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Unreachable(m) => write!(f, "cannot reach the source: {m}"),
            TransportError::Io(m) => write!(f, "transfer failed: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

pub trait Transport: Send + Sync + 'static {
    /// Issue one GET and hand back the response head and an unread body.
    ///
    /// Head and body are separate because the head decides where the body belongs. A
    /// server that ignores our `Range` answers `200` with the object from byte zero, and
    /// those bytes must replace the partial rather than extend it — a decision that has to
    /// be made before the first byte lands, not after. For a non-2xx the body is empty:
    /// error text is diagnostic and has no business in the spool.
    fn start(&self, request: &Request) -> Result<(Head, Box<dyn std::io::Read + Send>), TransportError>;
}

/// The real one.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout: Duration) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // The daemon follows redirects itself, after admission-checking each one.
            .max_redirects(0)
            // A 404 is an answer, not an exception. Statuses are interpreted above.
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .user_agent(USER_AGENT)
            // The system trust store, not the roots bundled into this binary. An operator
            // running a mirror behind their own CA installs it in /etc/ssl/certs and it
            // works; with the bundled roots it would not, and nothing in the image would
            // explain why.
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        UreqTransport { agent }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        UreqTransport::new(Duration::from_secs(60))
    }
}

impl Transport for UreqTransport {
    fn start(&self, request: &Request) -> Result<(Head, Box<dyn std::io::Read + Send>), TransportError> {
        let mut call = self.agent.get(request.uri.clone());
        if request.range_from > 0 {
            call = call.header("Range", format!("bytes={}-", request.range_from));
        }
        let response = call
            .call()
            .map_err(|e| TransportError::Unreachable(e.to_string()))?;

        let status = response.status().as_u16();
        let header = |name: &str| -> Option<String> {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let head = Head {
            status,
            etag: header("etag"),
            total_bytes: total_from_headers(
                status,
                header("content-range").as_deref(),
                header("content-length").as_deref(),
                request.range_from,
            ),
            location: header("location"),
        };

        // A non-2xx body is diagnostic text we have no use for and every reason not to
        // spool. Hand back nothing to read.
        if !(200..300).contains(&status) {
            return Ok((head, Box::new(std::io::empty())));
        }

        Ok((head, Box::new(response.into_body().into_reader())))
    }
}

/// How large is the whole object?
///
/// On a `206` the authority is `Content-Range: bytes <first>-<last>/<total>`; `*` for the
/// total means the server will not say. On a `200` the body is the whole object, so
/// `Content-Length` is the total — but only when we did not ask for a range, because a
/// server that ignores `Range` and sends `200` is sending the whole thing from zero.
pub fn total_from_headers(
    status: u16,
    content_range: Option<&str>,
    content_length: Option<&str>,
    range_from: u64,
) -> Option<u64> {
    if status == 206 {
        let range = content_range?;
        let total = range.rsplit_once('/')?.1.trim();
        return total.parse::<u64>().ok();
    }
    if status == 200 {
        let _ = range_from;
        return content_length?.trim().parse::<u64>().ok();
    }
    None
}

/// Copy at most `budget` bytes, and report how many.
///
/// `std::io::copy` has no budget, and an unbounded copy from a remote host into a file on
/// an 8 GB eMMC is the whole problem.
pub fn copy_bounded(
    reader: &mut (dyn Read + Send),
    sink: &mut dyn Write,
    budget: u64,
) -> Result<u64, TransportError> {
    const CHUNK: usize = 64 * 1024;
    let mut buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    while written < budget {
        let want = std::cmp::min(budget - written, CHUNK as u64) as usize;
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| TransportError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n])
            .map_err(|e| TransportError::Io(format!("writing to the spool: {e}")))?;
        written += n as u64;
    }
    sink.flush()
        .map_err(|e| TransportError::Io(format!("flushing the spool: {e}")))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_response_reports_the_whole_object_size() {
        assert_eq!(
            total_from_headers(206, Some("bytes 10-19/100"), Some("10"), 10),
            Some(100)
        );
    }

    #[test]
    fn a_server_that_will_not_say_the_total_is_not_guessed_at() {
        assert_eq!(
            total_from_headers(206, Some("bytes 10-19/*"), Some("10"), 10),
            None
        );
        assert_eq!(total_from_headers(206, None, Some("10"), 10), None);
    }

    #[test]
    fn a_full_response_takes_its_total_from_content_length() {
        assert_eq!(total_from_headers(200, None, Some("4096"), 0), Some(4096));
    }

    #[test]
    fn a_server_ignoring_our_range_still_reports_the_whole_object() {
        // 200 in reply to a Range request means "here is all of it from zero", so the
        // length is the total — the caller's job is to restart from zero, not to add.
        assert_eq!(total_from_headers(200, None, Some("4096"), 1000), Some(4096));
    }

    #[test]
    fn a_nonsense_length_is_no_length_rather_than_a_panic() {
        assert_eq!(total_from_headers(200, None, Some("banana"), 0), None);
        assert_eq!(total_from_headers(206, Some("bytes 0-1/banana"), None, 0), None);
        assert_eq!(total_from_headers(304, None, None, 0), None);
    }

    #[test]
    fn a_copy_stops_at_its_budget_and_says_so() {
        let src = vec![7u8; 10_000];
        let mut out = Vec::new();
        let n = copy_bounded(&mut src.as_slice(), &mut out, 4_096).expect("copy");
        assert_eq!(n, 4_096);
        assert_eq!(out.len(), 4_096);
    }

    #[test]
    fn a_copy_shorter_than_its_budget_reports_what_there_was() {
        let src = vec![7u8; 100];
        let mut out = Vec::new();
        let n = copy_bounded(&mut src.as_slice(), &mut out, 4_096).expect("copy");
        assert_eq!(n, 100);
    }

    #[test]
    fn a_zero_budget_reads_nothing() {
        let src = vec![7u8; 100];
        let mut out = Vec::new();
        assert_eq!(copy_bounded(&mut src.as_slice(), &mut out, 0).expect("copy"), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn the_user_agent_names_this_daemon_and_is_not_settable() {
        assert!(USER_AGENT.starts_with("otwono-fetchd/"));
    }
}
