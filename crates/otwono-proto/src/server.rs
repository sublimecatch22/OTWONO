//! Local Control Plane server.
//!
//! Thread-per-connection over a Unix domain socket. Deliberately not async: a control
//! plane handles a handful of concurrent callers, and avoiding an async runtime keeps the
//! daemons small enough to run on a T0 board (CLAUDE.md Section 5). If a subsystem ever
//! needs thousands of concurrent connections, that is the moment to revisit — with an ADR.

use crate::message::{code, Request, RequestId, Response, RpcError, ServiceDescription};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A single line may not exceed this. A local peer is authenticated but not trusted, and
/// an unbounded `read_line` is a trivial memory-exhaustion vector.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How long a connection may stay silent before the server drops it.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The identity of the calling process, from `SO_PEERCRED`.
///
/// This is kernel-supplied and unforgeable, which is what makes it usable as the subject
/// of an authorization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<i32>,
}

impl PeerIdentity {
    /// Canonical subject string used in policy rules and audit records.
    pub fn subject(&self) -> String {
        format!("uid:{}", self.uid)
    }
}

/// Everything a service needs to know about one call.
pub struct CallContext {
    pub peer: PeerIdentity,
    /// The capability token the caller presented, if any.
    pub capability: Option<String>,
}

pub trait Service: Send + Sync + 'static {
    fn describe(&self) -> ServiceDescription;
    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError>;
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

/// Lets a caller (a test, or a signal handler) stop the accept loop.
#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn trigger(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_triggered(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Server {
    /// Bind the socket, replacing a stale one left by an unclean shutdown.
    ///
    /// The socket is created 0660: the control plane is reachable by the owning user and
    /// group only. Peer credentials still decide authorization — the mode is defence in
    /// depth, not the boundary.
    pub fn bind(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A leftover socket file makes bind() fail with EADDRINUSE even though nothing is
        // listening. Removing it is safe: a live server holds the inode, not the name.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
        Ok(Server { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve until `shutdown` is triggered.
    pub fn serve<S: Service>(self, service: Arc<S>, shutdown: Shutdown) -> std::io::Result<()> {
        self.listener.set_nonblocking(true)?;
        let mut workers = Vec::new();

        while !shutdown.is_triggered() {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let service = Arc::clone(&service);
                    workers.push(std::thread::spawn(move || {
                        if let Err(e) = handle_connection(stream, service) {
                            // A client hanging up mid-request is routine, not an incident.
                            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                                eprintln!("otwono-proto: connection ended: {e}");
                            }
                        }
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
            workers.retain(|w| !w.is_finished());
        }

        for w in workers {
            let _ = w.join();
        }
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

fn handle_connection<S: Service>(stream: UnixStream, service: Arc<S>) -> std::io::Result<()> {
    let peer = peer_identity(&stream)?;
    // accept(2) does not inherit O_NONBLOCK from the listener on Linux, but be explicit:
    // a non-blocking stream here would turn every read into a busy spin.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(IDLE_TIMEOUT))?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        let n = read_line_bounded(&mut reader, &mut line)?;
        if n == 0 {
            return Ok(()); // clean hangup
        }
        if line.trim().is_empty() {
            continue;
        }

        let response = dispatch(&service, peer, &line);
        let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"unserialisable response"}}"#
                .to_string()
        });
        out.push('\n');
        writer.write_all(out.as_bytes())?;
        writer.flush()?;
    }
}

/// `read_line` with a hard cap, so one hostile line cannot exhaust memory.
fn read_line_bounded(reader: &mut impl BufRead, out: &mut String) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let mut byte = [0u8; 1];
        let read = std::io::Read::read(reader, &mut byte)?;
        if read == 0 {
            return Ok(total);
        }
        total += 1;
        if total > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("request line exceeded {MAX_LINE_BYTES} bytes"),
            ));
        }
        if byte[0] == b'\n' {
            return Ok(total);
        }
        out.push(byte[0] as char);
    }
}

fn dispatch<S: Service>(service: &Arc<S>, peer: PeerIdentity, line: &str) -> Response {
    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Response::err(
                RequestId::Null,
                RpcError::parse_error(format!("malformed JSON: {e}")),
            )
        }
    };

    if request.jsonrpc != crate::message::JSONRPC_VERSION {
        return Response::err(
            request.id,
            RpcError::invalid_request(format!("unsupported jsonrpc version {:?}", request.jsonrpc)),
        );
    }

    // `describe` is deliberately open: a caller must be able to discover what a service
    // offers, and which capability each method needs, before it can ask for one.
    if request.method == "describe" {
        return match serde_json::to_value(service.describe()) {
            Ok(v) => Response::ok(request.id, v),
            Err(e) => Response::err(request.id, RpcError::internal(e.to_string())),
        };
    }

    let (capability, params) = extract_capability(request.params);
    let ctx = CallContext { peer, capability };

    match service.call(&ctx, &request.method, params) {
        Ok(v) => Response::ok(request.id, v),
        Err(e) => Response::err(request.id, e),
    }
}

/// Pull `_cap` out of the params so services never see it as a domain parameter.
fn extract_capability(mut params: Value) -> (Option<String>, Value) {
    let cap = params
        .as_object_mut()
        .and_then(|m| m.remove("_cap"))
        .and_then(|v| v.as_str().map(str::to_string));
    (cap, params)
}

fn peer_identity(stream: &UnixStream) -> std::io::Result<PeerIdentity> {
    let cred = rustix::net::sockopt::get_socket_peercred(stream)
        .map_err(|e| std::io::Error::other(format!("SO_PEERCRED failed: {e}")))?;
    Ok(PeerIdentity {
        uid: cred.uid.as_raw(),
        gid: cred.gid.as_raw(),
        pid: Some(cred.pid.as_raw_nonzero().get()),
    })
}

/// Convenience for a method table that only needs a capability lookup.
pub fn required_capability(desc: &ServiceDescription, method: &str) -> Option<String> {
    desc.methods
        .iter()
        .find(|m| m.name == method)
        .and_then(|m| m.capability.clone())
}

/// Standard "this method does not exist" error carrying the method name.
pub fn unknown_method(method: &str) -> RpcError {
    RpcError::new(code::METHOD_NOT_FOUND, format!("unknown method: {method}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capability_is_stripped_from_params() {
        let (cap, rest) = extract_capability(json!({"_cap": "tok", "path": "/x"}));
        assert_eq!(cap.as_deref(), Some("tok"));
        assert_eq!(rest, json!({"path": "/x"}), "services must never see _cap");
    }

    #[test]
    fn missing_capability_is_none_not_an_error() {
        let (cap, rest) = extract_capability(json!({"path": "/x"}));
        assert_eq!(cap, None);
        assert_eq!(rest, json!({"path": "/x"}));
    }

    #[test]
    fn non_object_params_survive_capability_extraction() {
        let (cap, rest) = extract_capability(json!([1, 2, 3]));
        assert_eq!(cap, None);
        assert_eq!(rest, json!([1, 2, 3]));
    }

    #[test]
    fn subject_string_is_the_uid() {
        let p = PeerIdentity {
            uid: 1000,
            gid: 1000,
            pid: Some(42),
        };
        assert_eq!(p.subject(), "uid:1000");
    }

    #[test]
    fn a_line_longer_than_the_cap_is_rejected() {
        let huge = format!("{}\n", "a".repeat(MAX_LINE_BYTES + 10));
        let mut reader = BufReader::new(huge.as_bytes());
        let mut out = String::new();
        let err = read_line_bounded(&mut reader, &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_read_handles_eof_without_a_trailing_newline() {
        let mut reader = BufReader::new(&b"abc"[..]);
        let mut out = String::new();
        assert_eq!(read_line_bounded(&mut reader, &mut out).unwrap(), 3);
        assert_eq!(out, "abc");
    }
}
