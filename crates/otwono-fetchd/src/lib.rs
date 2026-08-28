//! OTWONO fetch daemon.
//!
//! The only component in the system that makes outbound client connections to hosts
//! outside the mesh (ADR-0014). `otwono-netd` still fetches ONM content from peers over
//! its own transport; this daemon does client HTTPS to hosts an operator named, and
//! nothing else.
//!
//! # Callers do not supply URLs
//!
//! A caller names a source from the allow-list and a path suffix under that source's
//! prefix. It cannot choose the scheme, the host, the port, the query string or a header,
//! so the only bytes it contributes to what leaves this node are a bounded path. That
//! residue is a covert channel, and it is bounded and logged rather than denied.
//!
//! # It holds no keys
//!
//! Deliberately the only network-facing process in OTWONO with nothing to steal. A
//! compromise here costs an attacker the ability to fetch from hosts the operator already
//! approved, and access to a spool directory whose contents nobody trusts yet.
//!
//! # Nothing it fetches is trusted
//!
//! Bytes land in a spool and a path comes back. Verification — digest, signature,
//! provenance — happens in the caller, with the caller's code. `otwono-aid` re-hashes
//! every blob it installs, and this daemon's opinion of what it downloaded is not an
//! input to that.

#![forbid(unsafe_code)]

pub mod transport;

use otwono_fetch::source::validate_path_suffix;
use otwono_fetch::spool::{ensure_room, SpoolEntry, SpoolError};
use otwono_fetch::{Source, SourceError, SourceSet};
use otwono_proto::{
    unknown_method, CallContext, Client, MethodDescription, RpcError, Service, ServiceDescription,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use transport::{copy_bounded, Head, Request, Transport, MAX_REDIRECTS};

pub const SERVICE_NAME: &str = "otwono-fetchd";
pub const DESCRIBE_SCHEMA_VERSION: &str = "1.0.0";

/// The one capability this daemon requires. Registered separately from `net.egress` so
/// that policy can grant a node the ability to fetch from approved sources without
/// granting a general egress oracle — the same narrowing ADR-0010 made for signing.
pub const CAPABILITY_FETCH: &str = "net.fetch";

/// How much one `fetch.get` will transfer before returning.
///
/// A 4 GB model does not fit inside a control-plane call, so a large object is fetched by
/// repeated calls and this is the size of one. Small enough to return promptly on a slow
/// link; large enough that a model does not take thousands of round trips.
pub const DEFAULT_CALL_BYTES: u64 = 64 * 1024 * 1024;

/// Wall-clock budget for one request.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Held back from every fetch, so filling the spool cannot also stop the node writing its
/// audit log.
pub const DEFAULT_SLACK_BYTES: u64 = 256 * 1024 * 1024;

/// A `Location` header is remote text. Bound it before it is parsed.
const MAX_LOCATION_BYTES: usize = 2048;

pub struct FetchService {
    sources: SourceSet,
    spool_dir: PathBuf,
    perm_socket: PathBuf,
    transport: Box<dyn Transport>,
    call_bytes: u64,
    call_timeout: Duration,
    slack_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct FetchParams {
    source: String,
    path: String,
    /// Optional per-call transfer budget, capped by the daemon's own.
    #[serde(default)]
    max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DiscardParams {
    source: String,
    path: String,
}

impl FetchService {
    pub fn new(
        sources: SourceSet,
        spool_dir: PathBuf,
        perm_socket: PathBuf,
        transport: Box<dyn Transport>,
    ) -> Self {
        FetchService {
            sources,
            spool_dir,
            perm_socket,
            transport,
            call_bytes: DEFAULT_CALL_BYTES,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            slack_bytes: DEFAULT_SLACK_BYTES,
        }
    }

    pub fn with_budgets(mut self, call_bytes: u64, call_timeout: Duration, slack: u64) -> Self {
        self.call_bytes = call_bytes;
        self.call_timeout = call_timeout;
        self.slack_bytes = slack;
        self
    }

    /// Ask the broker whether this caller may do this. Fail closed if it cannot be reached.
    ///
    /// The resource is the **source id**, so a policy can grant one caller `net.fetch` on
    /// the model host and not on the update host. Without it every grant would be "may
    /// fetch from anywhere in the allow-list", which is a coarser thing than the allow-list
    /// itself already expresses.
    fn authorize(&self, ctx: &CallContext, resource: Option<&str>) -> Result<(), RpcError> {
        let token = ctx.capability.as_deref().ok_or_else(|| {
            RpcError::unauthorized(format!(
                "{CAPABILITY_FETCH} requires a capability token; request one from otwono-permd"
            ))
        })?;
        let mut client = Client::connect(&self.perm_socket).map_err(|e| {
            RpcError::unavailable(format!(
                "cannot reach the permission broker at {}: {e}",
                self.perm_socket.display()
            ))
        })?;
        client
            .call(
                "perm.verify",
                json!({
                    "token": token,
                    "action": CAPABILITY_FETCH,
                    "subject": ctx.peer.subject(),
                    "resource": resource,
                }),
            )
            .map_err(|e| RpcError::unavailable(format!("permission broker call failed: {e}")))?
            .map(|_| ())
    }

    fn handle_sources(&self) -> Result<Value, RpcError> {
        let listed: Vec<Value> = self
            .sources
            .all()
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "host": s.host,
                    "port": s.port_or_default(),
                    "path_prefix": s.path_prefix,
                    "max_bytes": s.max_bytes,
                })
            })
            .collect();
        Ok(json!({
            "schema_version": DESCRIBE_SCHEMA_VERSION,
            "sources": listed,
            "spool_dir": self.spool_dir.display().to_string(),
            "call_bytes": self.call_bytes,
        }))
    }

    fn handle_discard(&self, p: DiscardParams) -> Result<Value, RpcError> {
        // Validate before touching the spool: an unknown source or an illegal path is a
        // caller error, and answering it identically whether or not a partial exists means
        // discard cannot be used to probe what this node has been downloading.
        let source = self.sources.get(&p.source).map_err(rpc_from_source)?;
        source.url_for(&p.path).map_err(rpc_from_source)?;
        let entry = SpoolEntry::new(&self.spool_dir, &p.source, &p.path);
        entry.discard().map_err(rpc_from_spool)?;
        Ok(json!({ "source": p.source, "path": p.path, "discarded": true }))
    }

    fn handle_get(&self, p: FetchParams) -> Result<Value, RpcError> {
        validate_path_suffix(&p.path).map_err(rpc_from_source)?;
        let source = self.sources.get(&p.source).map_err(rpc_from_source)?;
        let uri = source.url_for(&p.path).map_err(rpc_from_source)?;
        let entry = SpoolEntry::new(&self.spool_dir, &p.source, &p.path);

        if entry.is_complete() {
            let have = std::fs::metadata(entry.blob_path()).map(|m| m.len()).unwrap_or(0);
            return Ok(progress(&p, &uri, true, have, Some(have), Some(&entry), false));
        }

        let budget = p
            .max_bytes
            .map(|n| n.min(self.call_bytes))
            .unwrap_or(self.call_bytes)
            .max(1);

        let outcome = self.transfer(source, &entry, &uri, budget)?;
        Ok(progress(
            &p,
            &outcome.uri,
            outcome.complete,
            outcome.have,
            outcome.total,
            outcome.complete.then_some(&entry),
            outcome.restarted,
        ))
    }

    /// One round: follow admitted redirects, then move some bytes.
    fn transfer(
        &self,
        source: &Source,
        entry: &SpoolEntry,
        uri: &http::Uri,
        budget: u64,
    ) -> Result<Outcome, RpcError> {
        let mut have = entry.have_bytes();
        if have > source.max_bytes {
            // The partial is larger than the source is allowed to serve, so it cannot be a
            // prefix of anything legitimate. Drop it rather than reason about it.
            entry.reset().map_err(rpc_from_spool)?;
            have = 0;
        }

        let mut current = uri.clone();
        for hop in 0..=MAX_REDIRECTS {
            let request = Request {
                uri: &current,
                range_from: have,
                timeout: self.call_timeout,
            };
            let (head, mut body) = self
                .transport
                .start(&request)
                .map_err(|e| RpcError::unavailable(e.to_string()))?;

            if is_redirect(head.status) {
                if hop == MAX_REDIRECTS {
                    return Err(RpcError::unavailable(format!(
                        "source {:?} redirected more than {MAX_REDIRECTS} times",
                        source.id
                    )));
                }
                current = self.next_hop(source, &current, &head)?;
                continue;
            }

            return self.receive(source, entry, &current, head, &mut body, have, budget);
        }
        unreachable!("the redirect loop returns or errors on its last iteration")
    }

    /// Resolve a `Location` and decide whether we are willing to follow it.
    ///
    /// This is where a redirect stops being the server's decision. The resolved URL goes
    /// through the same admission the original request passed: same scheme, same host,
    /// same port, same prefix, same path rules.
    fn next_hop(&self, source: &Source, current: &http::Uri, head: &Head) -> Result<http::Uri, RpcError> {
        let location = head.location.as_deref().ok_or_else(|| {
            RpcError::unavailable(format!(
                "source {:?} answered {} with no Location",
                source.id, head.status
            ))
        })?;
        if location.len() > MAX_LOCATION_BYTES {
            return Err(RpcError::unavailable(format!(
                "source {:?} sent a Location of {} bytes",
                source.id,
                location.len()
            )));
        }
        let absolute = if location.starts_with("https://") {
            location.to_string()
        } else if location.starts_with('/') {
            // Root-relative. Anything else — a bare path, a scheme-relative "//host/x" —
            // is refused rather than resolved: the shapes we accept are the shapes we can
            // reason about.
            let authority = current
                .authority()
                .ok_or_else(|| RpcError::unavailable("current URL has no authority"))?;
            format!("https://{authority}{location}")
        } else {
            return Err(RpcError::unavailable(format!(
                "source {:?} redirected to {location:?}, which is neither an https URL nor a \
                 root-relative path",
                source.id
            )));
        };
        let next: http::Uri = absolute
            .parse()
            .map_err(|e| RpcError::unavailable(format!("redirect target does not parse: {e}")))?;
        source.admits(&next).map_err(|e| {
            RpcError::unavailable(format!("refused a redirect off source {:?}: {e}", source.id))
        })?;
        Ok(next)
    }

    /// Take delivery of a response body.
    #[allow(clippy::too_many_arguments)]
    fn receive(
        &self,
        source: &Source,
        entry: &SpoolEntry,
        uri: &http::Uri,
        head: Head,
        body: &mut (dyn std::io::Read + Send),
        have: u64,
        budget: u64,
    ) -> Result<Outcome, RpcError> {
        // A partial we already hold is only usable if the object has not changed. The
        // caller's digest would catch it later; noticing here saves re-downloading
        // gigabytes to fail at the end.
        let stored = entry.read_meta().map_err(rpc_from_spool)?;
        let etag_changed = match (&stored, &head.etag) {
            (Some(m), Some(now)) => m.etag.as_deref().is_some_and(|was| was != now),
            _ => false,
        };

        let mut restarted = false;
        let start_at = match head.status {
            // The server ignored our Range, or we asked for none: this is the whole object
            // from byte zero, so it replaces the partial rather than extending it.
            200 => {
                restarted = have > 0;
                0
            }
            206 if etag_changed => {
                return Err(RpcError::unavailable(format!(
                    "source {:?} changed under a resumed download; discard it and start again",
                    source.id
                )))
            }
            206 => have,
            416 => {
                // "Range not satisfiable". If we already hold the whole object, this is
                // what a finished download looks like on a resumed call.
                if let Some(total) = stored.as_ref().and_then(|m| m.total_bytes) {
                    if have == total {
                        let _ = entry.finish().map_err(rpc_from_spool)?;
                        return Ok(Outcome {
                            uri: uri.clone(),
                            complete: true,
                            have,
                            total: Some(total),
                            restarted: false,
                        });
                    }
                }
                entry.reset().map_err(rpc_from_spool)?;
                return Err(RpcError::unavailable(format!(
                    "source {:?} refused the resume range; the partial has been discarded, \
                     so a retry starts from the beginning",
                    source.id
                )));
            }
            status => {
                return Err(RpcError::unavailable(format!(
                    "source {:?} answered {status}",
                    source.id
                )))
            }
        };

        if etag_changed && start_at > 0 {
            entry.reset().map_err(rpc_from_spool)?;
            return Err(RpcError::unavailable(format!(
                "source {:?} changed under a resumed download; the partial has been discarded",
                source.id
            )));
        }

        let total = head.total_bytes;
        if let Some(t) = total {
            if t > source.max_bytes {
                return Err(RpcError::invalid_params(format!(
                    "object is {t} bytes, over source {:?}'s max_bytes of {}",
                    source.id, source.max_bytes
                )));
            }
        }

        // Refuse before filling the disk rather than after. When the server will not say
        // how big the object is, reserve for this call's budget.
        let need = total.map(|t| t.saturating_sub(start_at)).unwrap_or(budget);
        ensure_room(&self.spool_dir, need, self.slack_bytes).map_err(rpc_from_spool)?;

        let allowed = source.max_bytes.saturating_sub(start_at);
        let budget = budget.min(allowed);

        std::fs::create_dir_all(&self.spool_dir)
            .map_err(|e| RpcError::internal(format!("{}: {e}", self.spool_dir.display())))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(start_at > 0)
            .truncate(start_at == 0)
            .open(entry.part_path())
            .map_err(|e| RpcError::internal(format!("{}: {e}", entry.part_path().display())))?;

        let written =
            copy_bounded(body, &mut file, budget).map_err(|e| RpcError::unavailable(e.to_string()))?;
        let now = start_at + written;

        entry
            .write_meta(&entry.meta_for(&uri.to_string(), head.etag.as_deref(), total))
            .map_err(rpc_from_spool)?;

        if let Some(t) = total {
            if now > t {
                entry.reset().map_err(rpc_from_spool)?;
                return Err(RpcError::unavailable(format!(
                    "source {:?} sent {now} bytes for an object it said was {t}; the partial \
                     has been discarded",
                    source.id
                )));
            }
        }

        // Complete when the object's stated size is reached, or — when the server would
        // not state one — when the body ended before this call's budget did.
        let complete = match total {
            Some(t) => now == t,
            None => written < budget,
        };

        // A partial we cannot resume is worse than no partial. Without a stated size there
        // is nothing to resume *to*: the next call would ask for a range the server may
        // refuse, and a caller looping on "not complete yet" would loop forever. Found by
        // pointing this at a real host that answers without a Content-Length.
        if !complete && total.is_none() {
            entry.reset().map_err(rpc_from_spool)?;
            return Err(RpcError::unavailable(format!(
                "source {:?} did not say how large this object is, so the transfer cannot be                  resumed; it must fit one call. Retry with a larger max_bytes (this call                  moved {written} bytes and stopped at its budget). The partial has been                  discarded.",
                source.id
            )));
        }

        if complete {
            entry.finish().map_err(rpc_from_spool)?;
        }
        Ok(Outcome {
            uri: uri.clone(),
            complete,
            have: now,
            total,
            restarted,
        })
    }
}

struct Outcome {
    uri: http::Uri,
    complete: bool,
    have: u64,
    total: Option<u64>,
    restarted: bool,
}

fn progress(
    p: &FetchParams,
    uri: &http::Uri,
    complete: bool,
    have: u64,
    total: Option<u64>,
    entry: Option<&SpoolEntry>,
    restarted: bool,
) -> Value {
    json!({
        "schema_version": DESCRIBE_SCHEMA_VERSION,
        "source": p.source,
        "path": p.path,
        "url": uri.to_string(),
        "complete": complete,
        "bytes_have": have,
        "bytes_total": total,
        "restarted": restarted,
        "blob_path": entry.map(|e| e.blob_path().display().to_string()),
    })
}

pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn rpc_from_source(e: SourceError) -> RpcError {
    match e {
        SourceError::UnknownSource(_) | SourceError::Rejected(_) => RpcError::invalid_params(e.to_string()),
        SourceError::Invalid(_) | SourceError::Io(_) => RpcError::internal(e.to_string()),
    }
}

fn rpc_from_spool(e: SpoolError) -> RpcError {
    match e {
        SpoolError::NoSpace { .. } => RpcError::unavailable(e.to_string()),
        _ => RpcError::internal(e.to_string()),
    }
}

impl Service for FetchService {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription {
            schema_version: DESCRIBE_SCHEMA_VERSION.to_string(),
            service: SERVICE_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            methods: vec![
                MethodDescription::open("describe", "Describe this service"),
                MethodDescription::guarded(
                    "fetch.sources",
                    "List the sources this node may fetch from",
                    CAPABILITY_FETCH,
                ),
                MethodDescription::guarded(
                    "fetch.get",
                    "Fetch some of an object from an allow-listed source into the spool",
                    CAPABILITY_FETCH,
                ),
                MethodDescription::guarded(
                    "fetch.discard",
                    "Drop a spooled object, complete or partial",
                    CAPABILITY_FETCH,
                ),
            ],
        }
    }

    fn call(&self, ctx: &CallContext, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            // The source id is read before authorization because it *is* the resource the
            // broker decides about. Nothing is acted on until the token verifies.
            "fetch.sources" => {
                self.authorize(ctx, None)?;
                self.handle_sources()
            }
            "fetch.get" => {
                let p: FetchParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::invalid_params(format!("fetch.get: {e}")))?;
                self.authorize(ctx, Some(&p.source))?;
                self.handle_get(p)
            }
            "fetch.discard" => {
                let p: DiscardParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::invalid_params(format!("fetch.discard: {e}")))?;
                self.authorize(ctx, Some(&p.source))?;
                self.handle_discard(p)
            }
            other => Err(unknown_method(other)),
        }
    }
}
