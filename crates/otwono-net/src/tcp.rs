//! A TCP link adapter.
//!
//! The IP-network case: LAN peers today, and the path an Internet gateway will use later.
//! TCP is a byte stream and [`LinkAdapter`] is message-oriented, so this adds a 4-byte
//! big-endian length prefix — the one place in the stack where framing is synthesised,
//! because every other medium is already packet-based.

use crate::link::{LinkAdapter, LinkError, LinkProperties};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Frames larger than this are refused before allocating. A peer is unauthenticated until
/// the handshake completes, so it must not be able to make us reserve arbitrary memory.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

pub struct TcpLink {
    stream: TcpStream,
    properties: LinkProperties,
}

impl TcpLink {
    pub fn connect(addr: impl ToSocketAddrs, timeout: Duration) -> Result<Self, LinkError> {
        let addr = addr
            .to_socket_addrs()
            .map_err(|e| LinkError::Io(e.to_string()))?
            .next()
            .ok_or_else(|| LinkError::Io("no address resolved".into()))?;
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| LinkError::Io(e.to_string()))?;
        Self::from_stream(stream)
    }

    pub fn from_stream(stream: TcpStream) -> Result<Self, LinkError> {
        stream
            .set_nodelay(true)
            .map_err(|e| LinkError::Io(e.to_string()))?;
        Ok(TcpLink {
            stream,
            properties: LinkProperties::internet(),
        })
    }

    pub fn listen(addr: impl ToSocketAddrs) -> Result<TcpListener, LinkError> {
        TcpListener::bind(addr).map_err(|e| LinkError::Io(e.to_string()))
    }

    pub fn set_timeout(&self, timeout: Option<Duration>) -> Result<(), LinkError> {
        self.stream
            .set_read_timeout(timeout)
            .map_err(|e| LinkError::Io(e.to_string()))?;
        self.stream
            .set_write_timeout(timeout)
            .map_err(|e| LinkError::Io(e.to_string()))
    }

    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.stream.peer_addr().ok()
    }
}

impl LinkAdapter for TcpLink {
    fn properties(&self) -> LinkProperties {
        self.properties.clone()
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.properties.permits_payload(frame.len())?;
        let len = u32::try_from(frame.len()).map_err(|_| LinkError::Io("frame length exceeds u32".into()))?;
        self.stream
            .write_all(&len.to_be_bytes())
            .map_err(|e| LinkError::Io(e.to_string()))?;
        self.stream
            .write_all(frame)
            .map_err(|e| LinkError::Io(e.to_string()))?;
        self.stream.flush().map_err(|e| LinkError::Io(e.to_string()))
    }

    fn recv(&mut self) -> Result<Vec<u8>, LinkError> {
        let mut header = [0u8; 4];
        self.stream
            .read_exact(&mut header)
            .map_err(|e| LinkError::Io(e.to_string()))?;
        let len = u32::from_be_bytes(header) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(LinkError::Io(format!(
                "peer announced a {len}-byte frame, above the {MAX_FRAME_BYTES}-byte cap"
            )));
        }
        let mut frame = vec![0u8; len];
        self.stream
            .read_exact(&mut frame)
            .map_err(|e| LinkError::Io(e.to_string()))?;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_over_a_real_socket() {
        let listener = TcpLink::listen("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut link = TcpLink::from_stream(stream).unwrap();
            let got = link.recv().unwrap();
            link.send(&got).unwrap();
        });

        let mut client = TcpLink::connect(addr, Duration::from_secs(5)).unwrap();
        client.send(b"round trip").unwrap();
        assert_eq!(client.recv().unwrap(), b"round trip");
        server.join().unwrap();
    }

    #[test]
    fn message_boundaries_survive_stream_coalescing() {
        // The reason framing exists: TCP may deliver three sends as one read.
        let listener = TcpLink::listen("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut link = TcpLink::from_stream(stream).unwrap();
            for expected in [&b"one"[..], b"two", b"three"] {
                assert_eq!(link.recv().unwrap(), expected);
            }
        });

        let mut client = TcpLink::connect(addr, Duration::from_secs(5)).unwrap();
        for m in [&b"one"[..], b"two", b"three"] {
            client.send(m).unwrap();
        }
        server.join().unwrap();
    }

    #[test]
    fn an_absurd_announced_length_is_refused_before_allocating() {
        let listener = TcpLink::listen("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Claim 4 GiB without sending it.
            stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });

        let mut client = TcpLink::connect(addr, Duration::from_secs(5)).unwrap();
        let err = client.recv().unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
        server.join().unwrap();
    }
}
