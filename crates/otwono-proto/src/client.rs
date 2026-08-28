//! Local Control Plane client.

use crate::message::{Request, RequestId, Response, RpcError};
use serde_json::Value;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: AtomicI64,
    /// What the socket's read timeout was set to, so a timed-out call can say so.
    timeout: Duration,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::connect_with_timeout(path, DEFAULT_TIMEOUT)
    }

    pub fn connect_with_timeout(path: impl AsRef<Path>, timeout: Duration) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client {
            stream,
            reader,
            next_id: AtomicI64::new(1),
            timeout,
        })
    }

    /// Connect, retrying briefly while the socket appears.
    ///
    /// Starting a daemon and immediately connecting is inherently racy — the process is
    /// up before `bind()` has run. Every caller would otherwise write this loop.
    pub fn connect_waiting(path: impl AsRef<Path>, wait: Duration) -> std::io::Result<Self> {
        let path = path.as_ref();
        let deadline = std::time::Instant::now() + wait;
        loop {
            // The wait is evidence about the environment, not only about startup. A caller
            // willing to spend thirty seconds finding the socket is telling us the machine is
            // slow or busy — a boot on one emulated core with several checks contending, say
            // — and answering its calls on the fifteen-second default would time out a daemon
            // that is merely queued behind three others. So the same patience applies to the
            // answer, never shorter than the default.
            match Self::connect_with_timeout(path, wait.max(DEFAULT_TIMEOUT)) {
                Ok(c) => return Ok(c),
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// Call a method. `Err` is a transport failure; `Ok(Err)` is the service refusing.
    pub fn call(&mut self, method: &str, params: Value) -> std::io::Result<Result<Value, RpcError>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = Request::new(id, method, params);

        let mut line = serde_json::to_string(&request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;

        let response_line = read_bounded_line(&mut self.reader).map_err(|e| self.explain(e))?;

        let response: Response = serde_json::from_str(response_line.trim())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        if response.id != RequestId::Number(id) && response.id != RequestId::Null {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("response id {:?} does not match request id {id}", response.id),
            ));
        }
        Ok(response.into_result())
    }

    /// Say plainly when a call ran out of time.
    ///
    /// A read timeout on a Unix socket surfaces as `WouldBlock` — "Resource temporarily
    /// unavailable", errno 11 — which reads like a transient socket condition and is nothing
    /// of the kind: it means the daemon did not answer inside the window. That message cost a
    /// diagnosis, because a three-node run failed with it and the obvious readings were a full
    /// listen backlog or a broken socket rather than a daemon queued behind three others on
    /// one emulated core.
    ///
    /// The duration is in the message because it is the number the reader needs next: it says
    /// whether to wait longer or to go and look at the daemon.
    fn explain(&self, e: std::io::Error) -> std::io::Error {
        match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "the service did not answer within {:?}; it may be busy rather than broken",
                    self.timeout
                ),
            ),
            _ => e,
        }
    }

    /// Read one reply, refusing to allocate without limit.
    ///
    /// The server has always capped *request* lines, because callers are untrusted. This is
    /// the other direction, and it was missing: a `read_line` with no bound lets whatever is
    /// on the far end of the socket decide how much memory this process allocates.
    ///
    /// That matters here specifically because a client is not always the trusted party. The
    /// daemons that call each other over this plane include the two that parse hostile input
    /// — `otwono-netd` reads from peers and `otwono-aid` from model files — and a compromised
    /// one calling into another must not be able to exhaust it. Symmetry is the point: the
    /// cap is `MAX_LINE_BYTES` on both sides, which is also what makes
    /// `otwono_stored::MAX_INLINE_BYTES` a real limit rather than one that only bites on the
    /// way in.
    /// Call with a capability token attached.
    pub fn call_with_capability(
        &mut self,
        method: &str,
        mut params: Value,
        capability: &str,
    ) -> std::io::Result<Result<Value, RpcError>> {
        if !params.is_object() {
            params = serde_json::json!({});
        }
        params
            .as_object_mut()
            .expect("just ensured object")
            .insert("_cap".to_string(), Value::String(capability.to_string()));
        self.call(method, params)
    }

    pub fn describe(&mut self) -> std::io::Result<Result<crate::message::ServiceDescription, RpcError>> {
        Ok(match self.call("describe", serde_json::json!({}))? {
            Ok(v) => serde_json::from_value(v)
                .map_err(|e| RpcError::internal(format!("malformed describe payload: {e}"))),
            Err(e) => Err(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call that runs out of time says so, rather than "resource temporarily unavailable".
    ///
    /// That phrasing is how a Unix socket read timeout surfaces, and it reads like a
    /// transient socket condition. A three-node run failed with it and the obvious readings —
    /// a full listen backlog, a broken socket — were both wrong: a daemon was queued behind
    /// three others on one emulated core and simply took longer than the window.
    #[test]
    fn a_call_that_times_out_says_it_timed_out() {
        let dir = std::env::temp_dir().join(format!("otw-proto-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("silent.sock");

        // A server that accepts and never answers, which is what a daemon too busy to get to
        // this request looks like from here.
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let accepted = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });

        let mut client = Client::connect_with_timeout(&sock, Duration::from_millis(150)).unwrap();
        let err = client
            .call("anything", serde_json::json!({}))
            .expect_err("a silent service must not look like a success");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            err.to_string().contains("did not answer within"),
            "the message must name what happened: {err}"
        );
        let _ = accepted.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Waiting a long time to connect means waiting a long time for the answer too.
    ///
    /// The wait is evidence about the machine, not only about startup: a caller willing to
    /// spend thirty seconds finding a socket is on something slow or busy, and answering its
    /// calls on the fifteen-second default would time out a daemon that is merely queued.
    #[test]
    fn patience_connecting_carries_over_to_patience_waiting_for_an_answer() {
        let dir = std::env::temp_dir().join(format!("otw-proto-patience-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let accepted = std::thread::spawn(move || {
            let _ = listener.accept();
            std::thread::sleep(Duration::from_millis(50));
        });

        let generous = Duration::from_secs(45);
        let client = Client::connect_waiting(&sock, generous).unwrap();
        assert_eq!(client.timeout, generous);

        // And never *shorter* than the default: a caller in a hurry to connect is not asking
        // for a tighter answer deadline than everybody else gets.
        let brief = Client::connect_waiting(&sock, Duration::from_millis(5));
        if let Ok(c) = brief {
            assert_eq!(c.timeout, DEFAULT_TIMEOUT);
        }
        let _ = accepted.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_waiting_gives_up_and_reports_the_real_error() {
        let start = std::time::Instant::now();
        let err =
            Client::connect_waiting("/nonexistent/otwono/x.sock", Duration::from_millis(80)).unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(80));
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ));
    }
}

/// One newline-delimited line, or an error before the allocation gets out of hand.
fn read_bounded_line(reader: &mut impl std::io::BufRead) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // Byte at a time rather than `read_line`, because the point is to stop *before*
        // allocating, and every buffered alternative allocates first and checks after.
        let n = reader.read(&mut byte)?;
        if n == 0 {
            if line.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "service closed the connection without replying",
                ));
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > crate::MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the service sent a reply over {} bytes; refusing to buffer it",
                    crate::MAX_LINE_BYTES
                ),
            ));
        }
    }
    String::from_utf8(line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod bounded_line_tests {
    use super::*;

    #[test]
    fn a_reply_within_the_cap_is_read() {
        let line = format!("{}\n", "x".repeat(1000));
        let got = read_bounded_line(&mut line.as_bytes()).unwrap();
        assert_eq!(got.len(), 1000);
    }

    #[test]
    fn a_reply_over_the_cap_is_refused_rather_than_buffered() {
        // Defect 36: the server capped request lines and the client capped nothing, so a
        // daemon on the far end of the socket chose how much its caller allocated.
        let huge = format!("{}\n", "x".repeat(crate::MAX_LINE_BYTES + 1));
        let err = read_bounded_line(&mut huge.as_bytes()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("refusing to buffer"), "{err}");
    }

    #[test]
    fn exactly_the_cap_is_still_read() {
        let at = format!("{}\n", "x".repeat(crate::MAX_LINE_BYTES));
        assert_eq!(
            read_bounded_line(&mut at.as_bytes()).unwrap().len(),
            crate::MAX_LINE_BYTES
        );
    }

    #[test]
    fn a_closed_connection_with_nothing_sent_is_reported_as_such() {
        let err = read_bounded_line(&mut b"".as_slice()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_final_line_without_a_newline_is_still_a_line() {
        // A service that writes its reply and exits without flushing a newline has still
        // said something, and dropping it would turn a readable answer into a hang.
        assert_eq!(read_bounded_line(&mut b"{}".as_slice()).unwrap(), "{}");
    }
}
