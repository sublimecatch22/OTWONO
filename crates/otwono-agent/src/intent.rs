//! What the user asked for, once it has a name and typed arguments.
//!
//! An `Intent` is deliberately inert. It says what was asked; it does not say that it may
//! happen, and holding one confers nothing. Every intent still has to reach the daemon that
//! owns the operation, over the control plane, with a capability token — the same path a
//! `otwono-storectl` invocation takes. The assistant is not a privileged caller and must
//! never become one (CLAUDE.md §2.5).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One typed argument to an intent.
///
/// A small closed set rather than `serde_json::Value`. The grammar's whole claim is that it
/// is deterministic, and a free-form value type would let a verb accept something its
/// handler then has to interpret — which is where determinism goes to die.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Argument {
    /// A filesystem path, as the user typed it. Not canonicalised here: resolving a path
    /// against the caller's working directory is the caller's job, and doing it in the
    /// parser would make the grammar's behaviour depend on where it was invoked from.
    Path(String),
    /// A 64-character lowercase hex content id.
    ContentId(String),
    /// A visibility label, already validated against the four the system has.
    Visibility(String),
    /// A bare word from a fixed set the verb defines.
    Word(String),
}

impl Argument {
    pub fn as_str(&self) -> &str {
        match self {
            Argument::Path(s) | Argument::ContentId(s) | Argument::Visibility(s) | Argument::Word(s) => s,
        }
    }
}

/// A named operation with its arguments, ready to be authorized and dispatched.
///
/// `method` is a control-plane method name, not a new vocabulary. The grammar's job is to
/// get from what a person said to a call the system already knows how to authorize — not to
/// invent a second layer of names that would then need its own mapping, its own drift, and
/// its own bugs (CLAUDE.md §2.3 applied inside the system rather than to a dependency).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// The verb the user used, kept for explaining back what was understood.
    pub verb: String,
    /// The control-plane method this verb means, e.g. `store.put`.
    pub method: String,
    /// The capability the caller must hold. `None` for methods open on the local socket.
    pub capability: Option<String>,
    pub arguments: BTreeMap<String, Argument>,
    /// Whether performing this would change something. Carried so a caller can show a
    /// confirmation for the ones that do without consulting a second table.
    pub mutates: bool,
}

impl Intent {
    /// What this intent will do, in a sentence, for showing back before it runs.
    ///
    /// Written out rather than derived from the method name: "store.put" is what the system
    /// calls it and "save a file into your store" is what a person calls it, and the point
    /// of an assistant is to be on the person's side of that.
    pub fn explain(&self) -> String {
        let args: Vec<String> = self
            .arguments
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str()))
            .collect();
        if args.is_empty() {
            format!("{} ({})", self.verb, self.method)
        } else {
            format!("{} ({}) with {}", self.verb, self.method, args.join(", "))
        }
    }
}
