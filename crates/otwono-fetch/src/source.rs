//! The source allow-list: what this node is permitted to ask for, and from whom.
//!
//! Sources are TOML in `/etc/otwono/fetch.d/`, loaded in filename order. An absent or
//! empty directory yields an empty set, which permits nothing — the only safe way for a
//! component that governs egress to fail.
//!
//! A source is deliberately not "a URL a caller may use". It is a host, a port and a path
//! prefix, and the caller contributes only a suffix under that prefix. Everything else
//! about the request is fixed by the operator who wrote the file.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;

/// Where an image looks for the allow-list.
///
/// Not `policy.d/`, which `otwono-permd` owns: two loaders with two schemas sharing one
/// directory means each silently ignores the other's files, and a typo in a source entry
/// would read as a valid empty policy rather than an error.
pub const DEFAULT_SOURCE_DIR: &str = "/etc/otwono/fetch.d";

/// The cap on the caller-supplied part of a request.
///
/// This is the size of the covert channel a caller has through an approved source, so it
/// is a security parameter rather than an ergonomic one. 256 bytes fits every real model
/// path we know of and is small enough to state plainly in a threat model.
pub const MAX_PATH_SUFFIX_BYTES: usize = 256;

/// Total composed URL cap, so an enormous prefix cannot combine with a legal suffix.
pub const MAX_URL_BYTES: usize = 1024;

/// The one scheme. Not configurable, so no file can turn a source into cleartext.
pub const SCHEME: &str = "https";

const DEFAULT_PORT: u16 = 443;
const MAX_HOST_BYTES: usize = 253;
const MAX_ID_BYTES: usize = 64;

/// One place this node may fetch from.
/// `deny_unknown_fields` is load-bearing, not tidiness: without it `max_byte = 10` reads as
/// a cap and imposes none, and the operator who wrote it has no way to tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Stable name a caller uses. Lowercase, `[a-z0-9][a-z0-9-]*`.
    pub id: String,
    /// DNS name. Never an IP literal: the allow-list exists to be read by a human deciding
    /// whether to approve it, and a name is what TLS verifies against.
    pub host: String,
    /// Defaults to 443.
    #[serde(default)]
    pub port: Option<u16>,
    /// Absolute path prefix, `/`-terminated. Every request is under it.
    pub path_prefix: String,
    /// Hard cap on one fetched object from this source.
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    #[serde(default)]
    source: Vec<Source>,
}

/// Every source this node knows about.
#[derive(Debug, Clone, Default)]
pub struct SourceSet {
    sources: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The allow-list itself is wrong. Refuse to start rather than run on half a policy.
    Invalid(String),
    /// A caller asked for something the rules do not permit.
    Rejected(String),
    /// No such source id.
    UnknownSource(String),
    Io(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Invalid(m) => write!(f, "invalid source definition: {m}"),
            SourceError::Rejected(m) => write!(f, "rejected: {m}"),
            SourceError::UnknownSource(id) => {
                write!(f, "no source named {id:?} in the allow-list")
            }
            SourceError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl Source {
    /// Check that a source definition is one we are willing to act on.
    ///
    /// Called at load. A malformed entry stops the daemon starting, because the
    /// alternative is a node that silently permits something nobody wrote down.
    pub fn validate(&self) -> Result<(), SourceError> {
        validate_id(&self.id)?;
        validate_host(&self.host)?;
        if let Some(p) = self.port {
            if p == 0 {
                return Err(SourceError::Invalid(format!("{}: port 0", self.id)));
            }
        }
        validate_prefix(&self.id, &self.path_prefix)?;
        if self.max_bytes == 0 {
            return Err(SourceError::Invalid(format!(
                "{}: max_bytes is 0, which permits nothing; remove the source instead",
                self.id
            )));
        }
        Ok(())
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    /// The authority as it appears in a URL: the port is omitted when it is the default,
    /// so a composed URL and a redirect to the same place compare equal as strings.
    fn authority(&self) -> String {
        if self.port_or_default() == DEFAULT_PORT {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port_or_default())
        }
    }

    /// Compose the URL for a caller-supplied path suffix.
    ///
    /// The composed URL is parsed back and put through [`Source::admits`] before it is
    /// returned, so composition and redirect admission share one implementation. If this
    /// crate could ever build something it would not admit, that is a bug and it fails
    /// here rather than on the wire.
    pub fn url_for(&self, suffix: &str) -> Result<http::Uri, SourceError> {
        validate_path_suffix(suffix)?;
        let text = format!("{SCHEME}://{}{}{}", self.authority(), self.path_prefix, suffix);
        if text.len() > MAX_URL_BYTES {
            return Err(SourceError::Rejected(format!(
                "composed URL is {} bytes, over the {MAX_URL_BYTES}-byte cap",
                text.len()
            )));
        }
        let uri = http::Uri::from_str(&text)
            .map_err(|e| SourceError::Rejected(format!("composed URL does not parse: {e}")))?;
        self.admits(&uri)?;
        Ok(uri)
    }

    /// Would this source permit a request to `uri`?
    ///
    /// This is the redirect check. A `3xx` is a server asking us to make a different
    /// request, and it gets exactly the scrutiny the first one got.
    pub fn admits(&self, uri: &http::Uri) -> Result<(), SourceError> {
        if uri.scheme_str() != Some(SCHEME) {
            return Err(SourceError::Rejected(format!(
                "scheme is {:?}, and only {SCHEME} is permitted",
                uri.scheme_str().unwrap_or("(none)")
            )));
        }
        // `http::Uri` reports the real host for `https://evil@real/…`, so a host comparison
        // is already safe. Refusing userinfo outright anyway: a URL that needs explaining
        // is a URL we should not be fetching.
        let authority = uri
            .authority()
            .ok_or_else(|| SourceError::Rejected("URL has no host".into()))?;
        if authority.as_str().contains('@') {
            return Err(SourceError::Rejected(
                "URL carries userinfo before the host".into(),
            ));
        }
        let host = uri
            .host()
            .ok_or_else(|| SourceError::Rejected("URL has no host".into()))?;
        // Case-insensitive: `http::Uri` preserves the case it was given, so `HuggingFace.CO`
        // and `huggingface.co` arrive as different strings for the same host.
        if !host.eq_ignore_ascii_case(&self.host) {
            return Err(SourceError::Rejected(format!(
                "host {host:?} is not source {:?} ({})",
                self.id, self.host
            )));
        }
        if uri.port_u16().unwrap_or(DEFAULT_PORT) != self.port_or_default() {
            return Err(SourceError::Rejected(format!(
                "port {} is not source {:?}'s port {}",
                uri.port_u16().unwrap_or(DEFAULT_PORT),
                self.id,
                self.port_or_default()
            )));
        }
        if uri.query().is_some() {
            return Err(SourceError::Rejected(
                "URL carries a query string, which this interface never sends".into(),
            ));
        }
        let path = uri.path();
        let suffix = path.strip_prefix(&self.path_prefix).ok_or_else(|| {
            SourceError::Rejected(format!(
                "path {path:?} is not under source {:?}'s prefix {:?}",
                self.id, self.path_prefix
            ))
        })?;
        validate_path_suffix(suffix)
    }
}

/// The rules the caller-supplied part of a path must satisfy.
///
/// Restrictive on purpose. `%` is refused rather than decoded, so no encoded delimiter can
/// survive to be decoded by something downstream — `%2f..%2f` is a traversal only if
/// somebody decodes it, and the way to be sure nobody does is never to send one.
pub fn validate_path_suffix(suffix: &str) -> Result<(), SourceError> {
    if suffix.is_empty() {
        return Err(SourceError::Rejected("empty path".into()));
    }
    if suffix.len() > MAX_PATH_SUFFIX_BYTES {
        return Err(SourceError::Rejected(format!(
            "path is {} bytes, over the {MAX_PATH_SUFFIX_BYTES}-byte cap",
            suffix.len()
        )));
    }
    if suffix.starts_with('/') {
        return Err(SourceError::Rejected(
            "path is relative to the source prefix and must not start with '/'".into(),
        ));
    }
    if suffix.ends_with('/') {
        return Err(SourceError::Rejected(
            "path names a file, so it must not end with '/'".into(),
        ));
    }
    for c in suffix.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~' | '/');
        if !ok {
            return Err(SourceError::Rejected(format!(
                "path contains {c:?}; only ASCII letters, digits and . _ - ~ / are permitted"
            )));
        }
    }
    for segment in suffix.split('/') {
        if segment.is_empty() {
            return Err(SourceError::Rejected("path has an empty segment ('//')".into()));
        }
        if segment == "." || segment == ".." {
            return Err(SourceError::Rejected(format!("path has a {segment:?} segment")));
        }
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), SourceError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(SourceError::Invalid(format!(
            "source id {id:?} must be 1..={MAX_ID_BYTES} bytes"
        )));
    }
    let mut chars = id.chars();
    let first = chars.next().expect("id is non-empty");
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(SourceError::Invalid(format!(
            "source id {id:?} must start with a lowercase letter or a digit"
        )));
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(SourceError::Invalid(format!(
                "source id {id:?} contains {c:?}; use [a-z0-9-]"
            )));
        }
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), SourceError> {
    if host.is_empty() || host.len() > MAX_HOST_BYTES {
        return Err(SourceError::Invalid(format!(
            "host {host:?} must be 1..={MAX_HOST_BYTES} bytes"
        )));
    }
    if host != host.to_ascii_lowercase() {
        return Err(SourceError::Invalid(format!(
            "host {host:?} must be written lowercase, so that the file says what it means"
        )));
    }
    if !host.contains('.') {
        return Err(SourceError::Invalid(format!(
            "host {host:?} is not a fully qualified name"
        )));
    }
    // An IP literal is refused deliberately: the allow-list exists to be read by a person
    // deciding whether to approve it, and a name is also what TLS verifies against.
    if host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(SourceError::Invalid(format!(
            "host {host:?} is an IP literal; a source must be a DNS name"
        )));
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(SourceError::Invalid(format!("host {host:?} has an empty label")));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(SourceError::Invalid(format!(
                "host {host:?} has a label starting or ending with '-'"
            )));
        }
        for c in label.chars() {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                return Err(SourceError::Invalid(format!(
                    "host {host:?} contains {c:?}; use [a-z0-9-.]"
                )));
            }
        }
    }
    Ok(())
}

fn validate_prefix(id: &str, prefix: &str) -> Result<(), SourceError> {
    if !prefix.starts_with('/') {
        return Err(SourceError::Invalid(format!(
            "{id}: path_prefix {prefix:?} must start with '/'"
        )));
    }
    if !prefix.ends_with('/') {
        return Err(SourceError::Invalid(format!(
            "{id}: path_prefix {prefix:?} must end with '/', so that a suffix cannot extend \
             the last segment into a different one"
        )));
    }
    // "/" is both the leading and the trailing slash, so slicing it as `[1..len-1]` would
    // be a reversed range. It means "the whole host", which is legitimate.
    if prefix.len() == 1 {
        return Ok(());
    }
    let interior = &prefix[1..prefix.len() - 1];
    if interior.is_empty() {
        return Ok(());
    }
    validate_path_suffix(interior)
        .map_err(|e| SourceError::Invalid(format!("{id}: path_prefix {prefix:?}: {e}")))
}

impl SourceSet {
    pub fn new(sources: Vec<Source>) -> Result<Self, SourceError> {
        let mut seen = BTreeSet::new();
        for s in &sources {
            s.validate()?;
            if !seen.insert(s.id.clone()) {
                return Err(SourceError::Invalid(format!(
                    "source id {:?} is defined more than once",
                    s.id
                )));
            }
        }
        Ok(SourceSet { sources })
    }

    /// Parse one TOML document. Exposed so tests need no filesystem.
    pub fn parse(text: &str) -> Result<Self, SourceError> {
        let file: SourceFile =
            toml::from_str(text).map_err(|e| SourceError::Invalid(format!("not valid source TOML: {e}")))?;
        SourceSet::new(file.source)
    }

    /// Load every `*.toml` under `dir`, in filename order.
    ///
    /// A missing directory is an empty set, not an error: a node with no allow-list is a
    /// node that fetches nothing, which is a supported and safe state.
    pub fn load_dir(dir: &Path) -> Result<Self, SourceError> {
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SourceSet::default()),
            Err(e) => return Err(SourceError::Io(format!("{}: {e}", dir.display()))),
        };
        files.sort();

        let mut all = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| SourceError::Io(format!("{}: {e}", path.display())))?;
            let file: SourceFile = toml::from_str(&text).map_err(|e| {
                SourceError::Invalid(format!("{}: not valid source TOML: {e}", path.display()))
            })?;
            all.extend(file.source);
        }
        SourceSet::new(all)
    }

    pub fn get(&self, id: &str) -> Result<&Source, SourceError> {
        self.sources
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| SourceError::UnknownSource(id.to_string()))
    }

    pub fn all(&self) -> &[Source] {
        &self.sources
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hf() -> Source {
        Source {
            id: "huggingface".into(),
            host: "huggingface.co".into(),
            port: None,
            path_prefix: "/".into(),
            max_bytes: 1 << 30,
        }
    }

    fn scoped() -> Source {
        Source {
            id: "mirror".into(),
            host: "models.example.org".into(),
            port: Some(8443),
            path_prefix: "/otwono/models/".into(),
            max_bytes: 1 << 20,
        }
    }

    fn uri(s: &str) -> http::Uri {
        http::Uri::from_str(s).expect("test URL should parse")
    }

    #[test]
    fn a_composed_url_is_the_prefix_plus_the_suffix() {
        let s = scoped();
        assert_eq!(
            s.url_for("q4/model.gguf").unwrap().to_string(),
            "https://models.example.org:8443/otwono/models/q4/model.gguf"
        );
    }

    #[test]
    fn the_default_port_is_left_out_so_urls_compare_as_written() {
        // A redirect to "https://huggingface.co:443/x" and one to "https://huggingface.co/x"
        // are the same request; the composed form must not gratuitously differ from either.
        let s = hf();
        assert_eq!(s.url_for("x").unwrap().to_string(), "https://huggingface.co/x");
        assert!(s.admits(&uri("https://huggingface.co:443/x")).is_ok());
    }

    #[test]
    fn a_caller_cannot_climb_out_of_the_prefix() {
        let s = scoped();
        for bad in ["../../etc/passwd", "a/../../b", "..", "a/.."] {
            assert!(
                matches!(s.url_for(bad), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn percent_encoding_is_refused_rather_than_decoded() {
        // Measured: http::Uri leaves %2f alone, so a downstream decode would turn this into
        // a traversal. The way to be certain nobody decodes it is never to send one.
        let s = scoped();
        assert!(matches!(s.url_for("a%2f..%2fb"), Err(SourceError::Rejected(_))));
    }

    #[test]
    fn a_caller_cannot_append_a_query_or_a_fragment() {
        let s = hf();
        for bad in ["model.gguf?token=secret", "model.gguf#frag"] {
            assert!(
                matches!(s.url_for(bad), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_caller_cannot_switch_host_by_starting_with_a_slash() {
        let s = hf();
        for bad in ["/evil.com/x", "//evil.com/x"] {
            assert!(
                matches!(s.url_for(bad), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn the_suffix_cap_bounds_the_covert_channel() {
        let s = hf();
        let just_fits = "a".repeat(MAX_PATH_SUFFIX_BYTES);
        assert!(s.url_for(&just_fits).is_ok());
        let one_over = "a".repeat(MAX_PATH_SUFFIX_BYTES + 1);
        assert!(matches!(s.url_for(&one_over), Err(SourceError::Rejected(_))));
    }

    #[test]
    fn a_redirect_off_the_host_is_a_denial() {
        let s = hf();
        assert!(s.admits(&uri("https://huggingface.co/a/b")).is_ok());
        for bad in [
            "https://evil.example.com/a",
            "https://huggingface.co.evil.example.com/a",
            "https://huggingface.co:8443/a",
            "http://huggingface.co/a",
        ] {
            assert!(
                matches!(s.admits(&uri(bad)), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_redirect_host_is_compared_case_insensitively() {
        // Measured: http::Uri preserves the case it was given, so a redirect to
        // "HuggingFace.CO" would not match "huggingface.co" under a byte comparison — and
        // rejecting it would break real servers rather than any attacker.
        let s = hf();
        assert!(s.admits(&uri("https://HuggingFace.CO/a")).is_ok());
    }

    #[test]
    fn a_redirect_carrying_userinfo_is_refused() {
        // http::Uri reports host = huggingface.co here, so this would otherwise pass. It is
        // refused because a URL that needs explaining is not one to fetch.
        let s = hf();
        assert!(matches!(
            s.admits(&uri("https://evil.example.com@huggingface.co/a")),
            Err(SourceError::Rejected(_))
        ));
    }

    #[test]
    fn a_redirect_to_an_ip_literal_never_matches_a_named_source() {
        let s = hf();
        for bad in ["https://127.0.0.1/a", "https://[::1]/a", "https://10.0.0.5/a"] {
            assert!(
                matches!(s.admits(&uri(bad)), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_redirect_is_held_to_the_same_path_rules_as_the_request() {
        // The composed URL never contains "..", but a server can put one in a Location.
        // http::Uri does not normalise it away, so this check is the only thing between
        // that header and the wire.
        let s = scoped();
        for bad in [
            "https://models.example.org:8443/otwono/models/../../etc/passwd",
            "https://models.example.org:8443/otwono/models//x",
            "https://models.example.org:8443/elsewhere/x",
            "https://models.example.org:8443/otwono/models/x?t=1",
        ] {
            assert!(
                matches!(s.admits(&uri(bad)), Err(SourceError::Rejected(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_prefix_must_end_in_a_slash_so_a_suffix_cannot_extend_its_last_segment() {
        // "/otwono/model" + "s-evil/x" would otherwise reach /otwono/models-evil/x.
        let s = Source {
            path_prefix: "/otwono/model".into(),
            ..scoped()
        };
        assert!(matches!(s.validate(), Err(SourceError::Invalid(_))));
    }

    #[test]
    fn a_source_must_be_a_name_not_an_address() {
        for bad in ["127.0.0.1", "10.0.0.5"] {
            let s = Source {
                host: bad.into(),
                ..hf()
            };
            assert!(
                matches!(s.validate(), Err(SourceError::Invalid(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn an_uppercase_host_in_the_file_is_a_configuration_error() {
        // Comparison is case-insensitive, so this is not a security hole. It is refused so
        // that the file a human reviews reads the same way the code compares it.
        let s = Source {
            host: "HuggingFace.co".into(),
            ..hf()
        };
        assert!(matches!(s.validate(), Err(SourceError::Invalid(_))));
    }

    #[test]
    fn an_empty_allow_list_permits_nothing() {
        let set = SourceSet::default();
        assert!(set.is_empty());
        assert!(matches!(
            set.get("huggingface"),
            Err(SourceError::UnknownSource(_))
        ));
    }

    #[test]
    fn a_duplicate_id_is_refused_rather_than_shadowed() {
        // First-wins or last-wins are both defensible and both surprising. An operator who
        // defines a source twice gets told.
        let text = r#"
            [[source]]
            id = "a"
            host = "one.example.org"
            path_prefix = "/"
            max_bytes = 1024

            [[source]]
            id = "a"
            host = "two.example.org"
            path_prefix = "/"
            max_bytes = 1024
        "#;
        assert!(matches!(SourceSet::parse(text), Err(SourceError::Invalid(_))));
    }

    #[test]
    fn a_zero_byte_cap_is_a_configuration_error() {
        let s = Source { max_bytes: 0, ..hf() };
        assert!(matches!(s.validate(), Err(SourceError::Invalid(_))));
    }

    #[test]
    fn a_source_file_round_trips() {
        let text = r#"
            [[source]]
            id = "huggingface"
            host = "huggingface.co"
            path_prefix = "/"
            max_bytes = 21474836480
        "#;
        let set = SourceSet::parse(text).expect("valid");
        let s = set.get("huggingface").expect("present");
        assert_eq!(s.port_or_default(), 443);
        assert_eq!(s.max_bytes, 21474836480);
    }

    #[test]
    fn everything_we_compose_is_something_we_would_admit() {
        // The property that keeps the two code paths honest: if composition could ever
        // build a URL that admission rejects, the bug surfaces here and not on the wire.
        let sources = [hf(), scoped()];
        let suffixes = [
            "a",
            "a/b",
            "model.Q4_K_M.gguf",
            "TheBloke/Model-GGUF/resolve/main/x.gguf",
            "a~b",
            "a_b-c.d",
        ];
        for s in &sources {
            for suffix in suffixes {
                let uri = s.url_for(suffix).unwrap_or_else(|e| {
                    panic!("{}/{suffix} should compose: {e}", s.id);
                });
                s.admits(&uri)
                    .unwrap_or_else(|e| panic!("{uri} was composed but not admitted: {e}"));
            }
        }
    }
}
