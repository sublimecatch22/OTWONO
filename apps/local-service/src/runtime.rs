//! The handshake between the desktop shell and the service.
//!
//! The service binds a loopback port chosen by the operating system and mints a
//! 256-bit bearer token. Both are written to a file only the current user can
//! read. The shell reads that file and injects the token into the web view.
//! Nothing is broadcast, and no fixed port can be squatted by another process.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHandshake {
    pub version: String,
    pub address: String,
    pub port: u16,
    pub token: String,
    pub started_at: String,
    pub pid: u32,
}

impl RuntimeHandshake {
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.address, self.port)
    }
}

/// A fresh 256-bit token, URL-safe so it can travel in a header without
/// escaping.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare two tokens without leaking their contents through timing.
pub fn tokens_match(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in expected.bytes().zip(presented.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

pub fn write_handshake(path: &Path, handshake: &RuntimeHandshake) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(handshake)?)
        .with_context(|| format!("writing {}", path.display()))?;
    otwono_store::paths::restrict_to_owner(path)?;
    Ok(())
}

pub fn read_handshake(path: &Path) -> Result<RuntimeHandshake> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn remove_handshake(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Bind a loopback port. Passing 0 lets the operating system choose.
pub async fn bind(preferred_port: u16) -> Result<(tokio::net::TcpListener, SocketAddr)> {
    let address: SocketAddr = format!("127.0.0.1:{preferred_port}").parse()?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

pub fn handshake_path() -> Result<PathBuf> {
    otwono_store::paths::runtime_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_and_url_safe() {
        let a = mint_token();
        let b = mint_token();
        assert_ne!(a, b);
        assert!(
            a.len() >= 43,
            "a 256-bit token should be at least 43 characters"
        );
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "token {a} is not URL-safe"
        );
    }

    #[test]
    fn token_comparison_rejects_wrong_and_truncated_values() {
        let token = mint_token();
        assert!(tokens_match(&token, &token));
        assert!(!tokens_match(&token, &token[..token.len() - 1]));
        assert!(!tokens_match(&token, ""));
        assert!(!tokens_match(&token, &mint_token()));
    }

    #[test]
    fn the_handshake_file_round_trips_and_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.json");
        let handshake = RuntimeHandshake {
            version: "0.1.0".into(),
            address: "127.0.0.1".into(),
            port: 51234,
            token: mint_token(),
            started_at: "2026-01-01T00:00:00.000Z".into(),
            pid: std::process::id(),
        };
        write_handshake(&path, &handshake).unwrap();

        let read = read_handshake(&path).unwrap();
        assert_eq!(read.token, handshake.token);
        assert_eq!(read.base_url(), "http://127.0.0.1:51234");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "the handshake file must not be world-readable"
            );
        }

        remove_handshake(&path);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn binding_uses_loopback_and_an_operating_system_chosen_port() {
        let (listener, address) = bind(0).await.unwrap();
        assert!(
            address.ip().is_loopback(),
            "the service must not listen on a public interface"
        );
        assert_ne!(address.port(), 0);
        drop(listener);
    }
}
