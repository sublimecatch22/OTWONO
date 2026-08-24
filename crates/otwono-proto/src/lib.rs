//! OTWONO Local Control Plane.
//!
//! Every OTWONO daemon speaks JSON-RPC 2.0 over a Unix domain socket, one JSON object per
//! line (ADR-0003). This crate is that transport: the message types, a thread-per-connection
//! server, and a blocking client.
//!
//! Three properties the rest of the system depends on:
//!
//! * **`describe` is unauthenticated.** A caller can always ask what a service offers and
//!   which capability each method needs. Everything else requires a token.
//! * **The caller's identity comes from the kernel**, via `SO_PEERCRED`, not from anything
//!   the caller says about itself.
//! * **`_cap` never reaches a service as a domain parameter.** The server strips it.
//!
//! ```no_run
//! use otwono_proto::{Client, Server, Shutdown};
//! # use std::sync::Arc;
//! # fn demo<S: otwono_proto::Service>(svc: Arc<S>) -> std::io::Result<()> {
//! let server = Server::bind("/run/otwono/hw.sock")?;
//! let shutdown = Shutdown::new();
//! std::thread::spawn(move || server.serve(svc, shutdown));
//!
//! let mut client = Client::connect("/run/otwono/hw.sock")?;
//! let description = client.describe()?;
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod client;
pub mod message;
pub mod server;

pub use client::Client;
pub use message::{
    code, MethodDescription, Request, RequestId, Response, RpcError, ServiceDescription, JSONRPC_VERSION,
};
pub use server::{
    required_capability, unknown_method, CallContext, PeerIdentity, Server, Service, Shutdown, MAX_LINE_BYTES,
};

/// Default directory for control-plane sockets. Overridable so tests (and a developer
/// running two stacks side by side) never touch the real one.
pub const DEFAULT_SOCKET_DIR: &str = "/run/otwono";

/// Resolve the socket directory: `$OTWONO_SOCKET_DIR` if set, else the default.
pub fn socket_dir() -> std::path::PathBuf {
    std::env::var_os("OTWONO_SOCKET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_SOCKET_DIR))
}

/// Path for a named service's socket, e.g. `socket_path("hw")`.
pub fn socket_path(service: &str) -> std::path::PathBuf {
    socket_dir().join(format!("{service}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_follow_the_naming_rule() {
        // Not using the env var here: CLAUDE.md Section 4.3 fixes the on-disk layout.
        std::env::remove_var("OTWONO_SOCKET_DIR");
        assert_eq!(socket_path("hw"), std::path::Path::new("/run/otwono/hw.sock"));
        assert_eq!(socket_path("perm"), std::path::Path::new("/run/otwono/perm.sock"));
    }
}
