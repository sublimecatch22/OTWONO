use chrono::{DateTime, SecondsFormat, Utc};

pub type Timestamp = DateTime<Utc>;

/// Identifiers are opaque, URL-safe and generated locally.
pub fn new_id(prefix: &str) -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &raw[..24])
}

pub fn now() -> Timestamp {
    Utc::now()
}

/// Stable, sortable textual form used for database columns and API payloads.
pub fn format_ts(ts: &Timestamp) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_unique() {
        let a = new_id("agt");
        let b = new_id("agt");
        assert!(a.starts_with("agt_"));
        assert_eq!(a.len(), 28);
        assert_ne!(a, b);
    }

    #[test]
    fn timestamps_render_as_utc_rfc3339() {
        let s = format_ts(&now());
        assert!(s.ends_with('Z'), "expected UTC marker in {s}");
    }
}
