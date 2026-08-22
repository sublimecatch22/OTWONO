//! JSON-RPC 2.0 message types and the OTWONO error taxonomy.
//!
//! Wire format is newline-delimited JSON: exactly one JSON object per line, no embedded
//! newlines. That makes the control plane debuggable with `socat` and testable with a
//! string, which is most of why it was chosen (ADR-0003).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC id. The spec allows a string, a number, or null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
    Null,
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::Text(s) => write!(f, "{s}"),
            RequestId::Null => write!(f, "null"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>, params: Value) -> Self {
        Request {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        RequestId::Text(s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: RequestId, result: Value) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: RequestId, error: RpcError) -> Self {
        Response {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Convert into a plain `Result`, which is what a caller actually wants.
    pub fn into_result(self) -> Result<Value, RpcError> {
        match (self.result, self.error) {
            (_, Some(e)) => Err(e),
            (Some(v), None) => Ok(v),
            (None, None) => Err(RpcError::internal("response carried neither result nor error")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC reserved codes, plus the OTWONO range.
///
/// The OTWONO codes exist because "the caller may not do this" and "the caller asked for
/// something impossible" are different outcomes that a client must be able to distinguish
/// without string-matching a message.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// No capability token, or one that does not verify.
    pub const UNAUTHORIZED: i32 = -32000;
    /// Authenticated, but policy refuses.
    pub const FORBIDDEN: i32 = -32001;
    /// Policy requires a human to confirm before this can proceed.
    pub const CONFIRMATION_REQUIRED: i32 = -32002;
    /// The subsystem exists but cannot serve right now.
    pub const UNAVAILABLE: i32 = -32003;
    /// Caller is over its rate or resource budget.
    pub const RATE_LIMITED: i32 = -32004;
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(m: impl Into<String>) -> Self {
        Self::new(code::PARSE_ERROR, m)
    }
    pub fn invalid_request(m: impl Into<String>) -> Self {
        Self::new(code::INVALID_REQUEST, m)
    }
    pub fn method_not_found(m: impl Into<String>) -> Self {
        Self::new(code::METHOD_NOT_FOUND, m)
    }
    pub fn invalid_params(m: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(code::INTERNAL_ERROR, m)
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(code::UNAUTHORIZED, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(code::FORBIDDEN, m)
    }
    pub fn confirmation_required(m: impl Into<String>) -> Self {
        Self::new(code::CONFIRMATION_REQUIRED, m)
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(code::UNAVAILABLE, m)
    }

    pub fn is_unauthorized(&self) -> bool {
        self.code == code::UNAUTHORIZED
    }
    pub fn is_forbidden(&self) -> bool {
        self.code == code::FORBIDDEN
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// What a service reports from the unauthenticated `describe` method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceDescription {
    pub schema_version: String,
    pub service: String,
    pub version: String,
    pub methods: Vec<MethodDescription>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MethodDescription {
    pub name: String,
    pub summary: String,
    /// Capability a caller must hold. `None` means the method is open on the local socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
}

impl MethodDescription {
    pub fn open(name: &str, summary: &str) -> Self {
        MethodDescription {
            name: name.into(),
            summary: summary.into(),
            capability: None,
        }
    }

    pub fn guarded(name: &str, summary: &str, capability: &str) -> Self {
        MethodDescription {
            name: name.into(),
            summary: summary.into(),
            capability: Some(capability.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips_as_one_line() {
        let r = Request::new(1, "hw.profile", json!({"_cap": "abc"}));
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains('\n'), "NDJSON framing requires a single line");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back.method, "hw.profile");
        assert_eq!(back.id, RequestId::Number(1));
    }

    #[test]
    fn ids_may_be_strings_or_numbers() {
        let n: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"method":"m"}"#).unwrap();
        assert_eq!(n.id, RequestId::Number(7));
        let s: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","id":"x","method":"m"}"#).unwrap();
        assert_eq!(s.id, RequestId::Text("x".into()));
    }

    #[test]
    fn a_successful_response_omits_the_error_key_entirely() {
        let line = serde_json::to_string(&Response::ok(RequestId::Number(1), json!({"a": 1}))).unwrap();
        assert!(!line.contains("error"), "got {line}");
        let line =
            serde_json::to_string(&Response::err(RequestId::Number(1), RpcError::forbidden("no"))).unwrap();
        assert!(!line.contains("result"), "got {line}");
    }

    #[test]
    fn into_result_distinguishes_the_outcomes() {
        assert!(Response::ok(RequestId::Null, json!(1)).into_result().is_ok());
        let e = Response::err(RequestId::Null, RpcError::forbidden("nope"))
            .into_result()
            .unwrap_err();
        assert!(e.is_forbidden());
        // A malformed response with neither field must not silently look like success.
        let bad = Response {
            jsonrpc: "2.0".into(),
            id: RequestId::Null,
            result: None,
            error: None,
        };
        assert_eq!(bad.into_result().unwrap_err().code, code::INTERNAL_ERROR);
    }

    #[test]
    fn error_codes_are_distinguishable_without_string_matching() {
        assert!(RpcError::unauthorized("x").is_unauthorized());
        assert!(!RpcError::unauthorized("x").is_forbidden());
        assert!(RpcError::forbidden("x").is_forbidden());
    }
}
