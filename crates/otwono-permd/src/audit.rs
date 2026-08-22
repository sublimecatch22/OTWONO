//! Append-only, hash-chained audit log.
//!
//! Every decision the broker makes is written here before the caller learns the outcome.
//! Each record carries the hash of the one before it, so removing or editing a record
//! breaks the chain at that point and every point after it. That does not make the log
//! tamper-*proof* — an attacker with write access can rewrite the whole file and recompute
//! every hash — but it makes selective, quiet edits detectable, which is the realistic
//! threat: an agent or a process trying to erase one embarrassing line.
//!
//! Real tamper-evidence needs the chain head anchored somewhere the writer cannot reach
//! (a TPM counter, or a peer). That is Phase 3 work and is tracked as such; overstating
//! what a hash chain alone gives you would itself be a security failure.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `prev` for the first record. 64 hex zeros.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub const SCHEMA_VERSION: &str = "1.0.0";

/// One line of the audit log.
///
/// Field order is part of the contract: the hash is computed over this struct's JSON
/// serialisation, which serde emits in declaration order. Reordering fields changes every
/// hash, so treat it as a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub schema_version: String,
    pub seq: u64,
    pub ts_unix_ms: u64,
    pub event: String,
    pub subject: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub outcome: String,
    pub reason: String,
    pub prev: String,
    pub hash: String,
}

/// The record fields that are hashed — everything except the hash itself.
#[derive(Serialize)]
struct Hashed<'a> {
    schema_version: &'a str,
    seq: u64,
    ts_unix_ms: u64,
    event: &'a str,
    subject: &'a str,
    action: &'a str,
    resource: Option<&'a str>,
    outcome: &'a str,
    reason: &'a str,
    prev: &'a str,
}

fn compute_hash(r: &AuditRecord) -> String {
    let hashed = Hashed {
        schema_version: &r.schema_version,
        seq: r.seq,
        ts_unix_ms: r.ts_unix_ms,
        event: &r.event,
        subject: &r.subject,
        action: &r.action,
        resource: r.resource.as_deref(),
        outcome: &r.outcome,
        reason: &r.reason,
        prev: &r.prev,
    };
    let body = serde_json::to_string(&hashed).expect("audit record must serialise");
    blake3::hash(body.as_bytes()).to_hex().to_string()
}

/// What to write. The log fills in seq, prev and hash.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub event: String,
    pub subject: String,
    pub action: String,
    pub resource: Option<String>,
    pub outcome: String,
    pub reason: String,
}

#[derive(Debug)]
struct State {
    seq: u64,
    prev: String,
}

#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    state: Mutex<State>,
}

impl AuditLog {
    /// Open or create the log, resuming the chain from whatever is already on disk.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (seq, prev) = match read_records(&path) {
            Ok(records) => match records.last() {
                Some(last) => (last.seq, last.hash.clone()),
                None => (0, GENESIS_HASH.to_string()),
            },
            Err(AuditError::Io(e)) if e.contains("No such file") => (0, GENESIS_HASH.to_string()),
            // A corrupt tail must not be silently overwritten — that would destroy the
            // evidence the log exists to preserve.
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "refusing to append to a damaged audit log at {}: {e}",
                    path.display()
                )))
            }
        };
        Ok(AuditLog {
            path,
            state: Mutex::new(State { seq, prev }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: AuditEntry) -> std::io::Result<AuditRecord> {
        self.append_at(crate::token::now_unix_ms(), entry)
    }

    pub fn append_at(&self, now_ms: u64, entry: AuditEntry) -> std::io::Result<AuditRecord> {
        let mut state = self.state.lock().expect("audit log poisoned");
        let mut record = AuditRecord {
            schema_version: SCHEMA_VERSION.to_string(),
            seq: state.seq + 1,
            ts_unix_ms: now_ms,
            event: entry.event,
            subject: entry.subject,
            action: entry.action,
            resource: entry.resource,
            outcome: entry.outcome,
            reason: entry.reason,
            prev: state.prev.clone(),
            hash: String::new(),
        };
        record.hash = compute_hash(&record);

        let mut line = serde_json::to_string(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        // The audit record must reach disk before the caller is told the outcome;
        // otherwise a crash loses exactly the evidence of what just happened.
        file.sync_data()?;

        state.seq = record.seq;
        state.prev = record.hash.clone();
        Ok(record)
    }

    /// Verify the whole chain on disk.
    pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport, AuditError> {
        let records = read_records(path.as_ref())?;
        let mut prev = GENESIS_HASH.to_string();
        for (i, r) in records.iter().enumerate() {
            let expected_seq = (i as u64) + 1;
            if r.seq != expected_seq {
                return Ok(VerifyReport::broken(
                    records.len(),
                    r.seq,
                    format!("sequence jumped: expected {expected_seq}, found {}", r.seq),
                ));
            }
            if r.prev != prev {
                return Ok(VerifyReport::broken(
                    records.len(),
                    r.seq,
                    "prev hash does not match the previous record".to_string(),
                ));
            }
            if compute_hash(r) != r.hash {
                return Ok(VerifyReport::broken(
                    records.len(),
                    r.seq,
                    "record contents do not match its hash".to_string(),
                ));
            }
            prev = r.hash.clone();
        }
        Ok(VerifyReport {
            records: records.len(),
            intact: true,
            first_bad_seq: None,
            detail: None,
        })
    }
}

use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub records: usize,
    pub intact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_bad_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl VerifyReport {
    fn broken(records: usize, seq: u64, detail: String) -> Self {
        VerifyReport {
            records,
            intact: false,
            first_bad_seq: Some(seq),
            detail: Some(detail),
        }
    }
}

fn read_records(path: &Path) -> Result<Vec<AuditRecord>, AuditError> {
    let file = std::fs::File::open(path).map_err(|e| AuditError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| AuditError::Io(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let record: AuditRecord = serde_json::from_str(&line).map_err(|e| AuditError::Malformed {
            line: i + 1,
            detail: e.to_string(),
        })?;
        out.push(record);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditError {
    Io(String),
    Malformed { line: usize, detail: String },
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditError::Io(e) => write!(f, "{e}"),
            AuditError::Malformed { line, detail } => write!(f, "line {line} is malformed: {detail}"),
        }
    }
}

impl std::error::Error for AuditError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otwono-audit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(action: &str, outcome: &str) -> AuditEntry {
        AuditEntry {
            event: "decision".into(),
            subject: "uid:0".into(),
            action: action.into(),
            resource: None,
            outcome: outcome.into(),
            reason: "test".into(),
        }
    }

    #[test]
    fn an_empty_log_verifies() {
        let d = tmpdir("empty");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        drop(log);
        std::fs::write(&p, "").unwrap();
        let r = AuditLog::verify(&p).unwrap();
        assert!(r.intact);
        assert_eq!(r.records, 0);
    }

    #[test]
    fn a_written_chain_verifies() {
        let d = tmpdir("chain");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        for i in 0..5 {
            log.append_at(1000 + i, entry("hw.read", "allow")).unwrap();
        }
        let r = AuditLog::verify(&p).unwrap();
        assert!(r.intact, "{r:?}");
        assert_eq!(r.records, 5);
    }

    #[test]
    fn the_first_record_chains_from_genesis() {
        let d = tmpdir("genesis");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        let rec = log.append_at(1, entry("hw.read", "allow")).unwrap();
        assert_eq!(rec.seq, 1);
        assert_eq!(rec.prev, GENESIS_HASH);
    }

    #[test]
    fn editing_a_record_breaks_the_chain() {
        // The property the whole design exists for: a quiet edit is detectable.
        let d = tmpdir("tamper");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        for i in 0..4 {
            log.append_at(1000 + i, entry("fs.delete", "deny")).unwrap();
        }
        assert!(AuditLog::verify(&p).unwrap().intact);

        // Flip one outcome from deny to allow, exactly what someone covering their tracks
        // would do, leaving every other byte intact.
        let text = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines[2] = lines[2].replace(r#""outcome":"deny""#, r#""outcome":"allow""#);
        assert!(
            lines[2].contains(r#""outcome":"allow""#),
            "the edit must actually apply"
        );
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();

        let r = AuditLog::verify(&p).unwrap();
        assert!(!r.intact);
        assert_eq!(r.first_bad_seq, Some(3));
    }

    #[test]
    fn deleting_a_record_breaks_the_chain() {
        let d = tmpdir("delete");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        for i in 0..4 {
            log.append_at(1000 + i, entry("hw.read", "allow")).unwrap();
        }
        let text = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines.remove(1);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();

        let r = AuditLog::verify(&p).unwrap();
        assert!(!r.intact);
        assert_eq!(r.first_bad_seq, Some(3), "removal shows up as a sequence jump");
    }

    #[test]
    fn reopening_resumes_the_chain_rather_than_restarting_it() {
        let d = tmpdir("resume");
        let p = d.join("audit.jsonl");
        {
            let log = AuditLog::open(&p).unwrap();
            log.append_at(1, entry("hw.read", "allow")).unwrap();
            log.append_at(2, entry("hw.read", "allow")).unwrap();
        }
        {
            let log = AuditLog::open(&p).unwrap();
            let rec = log.append_at(3, entry("hw.read", "allow")).unwrap();
            assert_eq!(rec.seq, 3, "sequence must continue across a restart");
        }
        assert!(AuditLog::verify(&p).unwrap().intact);
    }

    #[test]
    fn a_damaged_log_is_not_silently_appended_to() {
        let d = tmpdir("damaged");
        let p = d.join("audit.jsonl");
        std::fs::write(&p, "this is not json\n").unwrap();
        let err = AuditLog::open(&p).unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
    }

    #[test]
    fn the_log_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("mode");
        let p = d.join("audit.jsonl");
        let log = AuditLog::open(&p).unwrap();
        log.append_at(1, entry("hw.read", "allow")).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "audit log must not be world-readable");
    }
}
