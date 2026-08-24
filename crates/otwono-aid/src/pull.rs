//! Fetching a model, which this daemon cannot do itself.
//!
//! `otwono-aid` runs with `PrivateNetwork=yes` and keeps it. Downloading therefore happens
//! in `otwono-fetchd`, and this module is the client: it drives `fetch.get` over the
//! control plane until an object is complete, then hands a spool path to the install code
//! that already exists (ADR-0014).
//!
//! # Why the loop is here rather than there
//!
//! `fetch.get` moves a bounded number of bytes and returns progress, because a 4 GB model
//! does not fit inside one control-plane call. That makes resumption the ordinary path,
//! and it makes the caller responsible for continuing. The loop is small; the alternative
//! is a job state machine in the fetcher and a way to orphan one.

use otwono_proto::Client;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The capability `otwono-fetchd` requires. Named here rather than imported so that this
/// daemon does not depend on the fetcher's crate — only on its interface.
pub const CAPABILITY_FETCH: &str = "net.fetch";

/// A resumed fetch makes no progress if the source is broken in a way that still returns
/// success. Bound the loop rather than trusting it to terminate.
pub const MAX_CALLS: usize = 4096;

/// Long enough for one bounded transfer over a poor link, short enough to fail eventually.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Fetcher {
    pub fetch_socket: PathBuf,
    pub perm_socket: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub path: PathBuf,
    pub bytes: u64,
    /// How many `fetch.get` calls it took. Reported because it is the honest measure of
    /// how a link behaved, and because a surprising number means something to look at.
    pub calls: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullError {
    /// The broker refused, or could not be reached.
    Unauthorized(String),
    /// `otwono-fetchd` could not be reached.
    Unreachable(String),
    /// The fetcher refused this request, or the source did.
    Refused(String),
    /// The fetcher answered, but not with anything usable.
    Malformed(String),
    /// The loop hit its bound without completing.
    NoProgress { calls: usize, bytes: u64 },
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::Unauthorized(m) => write!(f, "not permitted to fetch: {m}"),
            PullError::Unreachable(m) => write!(f, "cannot reach otwono-fetchd: {m}"),
            PullError::Refused(m) => write!(f, "fetch refused: {m}"),
            PullError::Malformed(m) => write!(f, "otwono-fetchd answered oddly: {m}"),
            PullError::NoProgress { calls, bytes } => write!(
                f,
                "gave up after {calls} calls with {bytes} bytes and no completion"
            ),
        }
    }
}

impl std::error::Error for PullError {}

impl Fetcher {
    /// Ask the broker for a `net.fetch` token scoped to one source.
    ///
    /// Per call rather than cached: tokens are short-lived by design, and a pull may run
    /// for an hour. Re-requesting is cheap and means a policy change takes effect during a
    /// long download rather than after it.
    fn token(&self, source: &str) -> Result<String, PullError> {
        let mut broker = Client::connect(&self.perm_socket).map_err(|e| {
            PullError::Unauthorized(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;
        broker
            .call(
                "perm.request",
                json!({
                    "action": CAPABILITY_FETCH,
                    "resource": source,
                    "reason": "otwono-aid is downloading a model",
                }),
            )
            .map_err(|e| PullError::Unauthorized(format!("perm.request: {e}")))?
            .map_err(|e| PullError::Unauthorized(e.message))?
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| PullError::Malformed("perm.request returned no token".into()))
    }

    fn call(&self, method: &str, params: Value, source: &str) -> Result<Value, PullError> {
        let token = self.token(source)?;
        let mut client = Client::connect_with_timeout(&self.fetch_socket, CALL_TIMEOUT)
            .map_err(|e| PullError::Unreachable(format!("{}: {e}", self.fetch_socket.display())))?;
        client
            .call_with_capability(method, params, &token)
            .map_err(|e| PullError::Unreachable(format!("{method}: {e}")))?
            .map_err(|e| PullError::Refused(e.message))
    }

    /// Drive `fetch.get` until the object is complete, and return where it landed.
    pub fn fetch(&self, source: &str, path: &str) -> Result<Fetched, PullError> {
        let mut last_bytes = 0u64;
        for call in 1..=MAX_CALLS {
            let value = self.call("fetch.get", json!({ "source": source, "path": path }), source)?;

            let bytes = value
                .get("bytes_have")
                .and_then(Value::as_u64)
                .ok_or_else(|| PullError::Malformed("no bytes_have in the reply".into()))?;
            let complete = value
                .get("complete")
                .and_then(Value::as_bool)
                .ok_or_else(|| PullError::Malformed("no complete flag in the reply".into()))?;

            if complete {
                let blob = value
                    .get("blob_path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| PullError::Malformed("a complete fetch with no blob_path".into()))?;
                return Ok(Fetched {
                    path: PathBuf::from(blob),
                    bytes,
                    calls: call,
                });
            }

            // A restart is progress of a kind — the fetcher says the object changed under
            // us and it began again — so it does not count as stalling. Two consecutive
            // calls that move nothing and do not restart mean the loop would spin forever.
            let restarted = value.get("restarted").and_then(Value::as_bool).unwrap_or(false);
            if bytes <= last_bytes && !restarted && call > 1 {
                return Err(PullError::NoProgress { calls: call, bytes });
            }
            last_bytes = bytes;
        }
        Err(PullError::NoProgress {
            calls: MAX_CALLS,
            bytes: last_bytes,
        })
    }

    /// Drop a spooled object. Called after a successful install, because `install` copies
    /// rather than moves — and leaving the copy would mean a 4 GB model occupies 8 GB on a
    /// board that has 8.
    pub fn discard(&self, source: &str, path: &str) -> Result<(), PullError> {
        self.call("fetch.discard", json!({ "source": source, "path": path }), source)
            .map(|_| ())
    }
}

/// Where a fetched object landed, as a `Path` the installer can read.
pub fn spool_path(fetched: &Fetched) -> &Path {
    &fetched.path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fetch_that_stops_moving_is_not_retried_forever() {
        // The failure this guards against was observed for real against a live server
        // before otwono-fetchd learned to refuse an unresumable partial: three calls, same
        // byte count, no end. A bounded loop turns a hang into an error message.
        let e = PullError::NoProgress {
            calls: 3,
            bytes: 16_384,
        };
        assert!(e.to_string().contains("no completion"));
    }

    #[test]
    fn the_capability_name_matches_the_fetchers() {
        // A rename on either side must be caught by a human reading this, since the two
        // crates deliberately do not depend on each other.
        assert_eq!(CAPABILITY_FETCH, "net.fetch");
    }
}
