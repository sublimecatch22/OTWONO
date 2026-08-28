use thiserror::Error;

pub type DomainResult<T> = Result<T, DomainError>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("invalid transition: {entity} cannot move from {from} to {to}")]
    InvalidTransition {
        entity: &'static str,
        from: String,
        to: String,
    },

    #[error("validation failed for {field}: {reason}")]
    Validation { field: String, reason: String },

    #[error("unsupported package schema version {found}; this build understands {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },

    #[error("refused: {0}")]
    Refused(String),
}

impl DomainError {
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }
}
