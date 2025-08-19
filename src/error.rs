//! Unified application error types and conversion helpers.
//! These errors are intended for internal use; transport-facing serialization
//! happens via `IoErrorPacket` in `io_server` using `Into<ClientFacingError>`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// High-level classification for mapping to client error codes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Malformed JSON / invalid schema
    Parse,
    /// Command was understood but arguments were invalid or out of range
    Validation,
    /// Referenced resource (device, stream, etc.) not found
    NotFound,
    /// Operation not permitted in current state or due to security policy
    Permission,
    /// Internal I/O failure (disk, socket, pipe)
    Io,
    /// Unexpected internal error / bug
    Internal,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            serde_json::to_string(self).unwrap_or_else(|_| "internal".into())
        )
    }
}

/// Core error enum used throughout the application.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("parse error: {0}")]
    Parse(String),
    // #[error("validation error: {0}")]
    // Validation(String),
    #[error("not found: {0}")]
    // NotFound(String),
    // #[error("permission denied: {0}")]
    // Permission(String),
    // #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            AppError::Parse(_) => ErrorKind::Parse,
            // AppError::Validation(_) => ErrorKind::Validation,
            // AppError::NotFound(_) => ErrorKind::NotFound,
            // AppError::Permission(_) => ErrorKind::Permission,
            AppError::Io(_) => ErrorKind::Io,
            AppError::Internal(_) => ErrorKind::Internal,
        }
    }

    /// Short machine error code string (snake_case) used in client packets.
    pub fn code(&self) -> &'static str {
        match self.kind() {
            ErrorKind::Parse => "parse_error",
            ErrorKind::Validation => "validation_error",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Permission => "permission_denied",
            ErrorKind::Io => "io_error",
            ErrorKind::Internal => "internal_error",
        }
    }
}

/// Simplified client-facing error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFacingError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl From<AppError> for ClientFacingError {
    fn from(err: AppError) -> Self {
        Self {
            code: err.code().to_string(),
            message: err.to_string(),
            raw: None,
        }
    }
}

impl ClientFacingError {
    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(raw.into());
        self
    }
}

/// Shorthand constructors for common errors.
pub mod err {
    use super::AppError;
    pub fn parse(msg: impl Into<String>) -> AppError {
        AppError::Parse(msg.into())
    }
    // pub fn validation(msg: impl Into<String>) -> AppError {
    //     AppError::Validation(msg.into())
    // }
    // pub fn not_found(msg: impl Into<String>) -> AppError {
    //     AppError::NotFound(msg.into())
    // }
    // pub fn permission(msg: impl Into<String>) -> AppError {
    //     AppError::Permission(msg.into())
    // }
    pub fn internal(msg: impl Into<String>) -> AppError {
        AppError::Internal(msg.into())
    }
}

/// Simple JSON error response for REST endpoints (distinct from WS IoErrorPacket)
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,   // machine code
    pub message: String, // human readable
}

impl From<AppError> for ErrorResponse {
    fn from(e: AppError) -> Self {
        Self {
            error: e.code().to_string(),
            message: e.to_string(),
        }
    }
}

/// Macro for early-returning an AppError in functions returning Result<T, AppError>
#[macro_export]
macro_rules! bail_app {
    ($variant:ident, $($arg:tt)*) => { return Err($crate::error::AppError::$variant(format!($($arg)*))) };
}
