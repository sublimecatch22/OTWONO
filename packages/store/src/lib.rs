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

/// Helper: parse a timestamp column. Rows are always written by
/// `otwono_types::ids::format_ts`, so an unparseable value means the row was
/// edited outside the application; fall back to the epoch rather than failing
/// to load the user's data.
pub(crate) fn parse_ts(raw: &str) -> otwono_types::Timestamp {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH)
}

pub(crate) fn parse_ts_opt(raw: Option<String>) -> Option<otwono_types::Timestamp> {
    raw.as_deref().map(parse_ts)
}

/// Helper: current time as a database string.
pub(crate) fn now_str() -> String {
    otwono_types::ids::format_ts(&otwono_types::now())
}
