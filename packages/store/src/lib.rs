//! Persistence for OTWONO AI: SQLite schema, migrations, backups, repositories
//! and secret storage.
//!
//! Nothing in this crate reaches the network. Nothing in this crate writes
//! outside the application data directory.

pub mod db;
pub mod migrations;
pub mod paths;
pub mod repo;
pub mod secrets;

pub use db::{Conn, Db};
pub use migrations::MigrationOutcome;
pub use secrets::{SecretBackend, SecretStore};

/// Helper: read a JSON column into a value, tolerating legacy nulls.
pub(crate) fn json_column<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> T {
    raw.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Helper: serialise a value for a JSON column.
pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}
