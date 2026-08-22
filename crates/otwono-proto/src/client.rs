//! Local Control Plane client.

use crate::message::{Request, RequestId, Response, RpcError};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
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
            match Self::connect(path) {
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

        let mut response_line = String::new();
        if self.reader.read_line(&mut response_line)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "service closed the connection without replying",
            ));
        }

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
