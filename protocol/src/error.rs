//! The one error shape every `ConductorBackend` method returns, so a caller on either side
//! of the edge boundary (the in-process `LocalBackend`, or `RemoteBackend` deserializing an
//! HTTP error body) can branch on the same `kind` instead of parsing prose. This is a
//! verbatim carry-over of `mcp.rs`'s pre-M12 `to_mcp_error` classification (M11 Phase 1/2),
//! just moved here so it has no dependency on `rmcp` and can be shared by a backend that
//! isn't talking MCP at all on its own side (`RemoteBackend` speaks plain HTTP to the main
//! Conductor; only the edge's own stdio surface speaks MCP).

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    Conflict,
    InvalidParams,
    /// Authenticated, but not authorized: a missing/revoked enrollment is a credential
    /// problem (the caller gets a plain 401, never a `BackendError` body at all - see
    /// `serve.rs`'s edge auth), but a *valid, unrevoked* enrollment whose
    /// `allowed_operations`/`repository_scope` doesn't cover this call is this kind -
    /// added in M12.0 Phase 2, the first place this distinction existed to make.
    Forbidden,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl BackendError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::NotFound,
            message: message.into(),
            data: None,
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Conflict,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidParams,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params_with(message: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            kind: ErrorKind::InvalidParams,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: message.into(),
            data: None,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Forbidden,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BackendError {}

/// Classifies an underlying engine error (`TaskService`/`Orchestrator`/`Store`/`GlobalStop`,
/// all `anyhow::Error`) by the same message substrings `mcp.rs::to_mcp_error` and
/// `serve.rs::error_response` already use — three independent call sites agreeing on the
/// same handful of substrings was the actual drift risk this consolidates, not a new rule.
impl From<anyhow::Error> for BackendError {
    fn from(err: anyhow::Error) -> Self {
        let message = err.to_string();
        let kind = if message.contains("no task id matches")
            || message.contains("ambiguous task id prefix")
            || message.contains("unknown repository")
            || message.contains("unknown policy id")
        {
            ErrorKind::NotFound
        } else if message.contains("revision conflict")
            || message.contains("global stop is engaged")
            || message.contains("already revoked")
            || message.contains("is not engaged")
            || message.contains("requires an actor")
        {
            ErrorKind::Conflict
        } else {
            ErrorKind::Internal
        };
        Self {
            kind,
            data: Some(serde_json::json!({"kind": kind_str(kind), "detail": message.clone()})),
            message,
        }
    }
}

fn kind_str(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::Conflict => "conflict",
        ErrorKind::InvalidParams => "invalid_params",
        ErrorKind::Forbidden => "forbidden",
        ErrorKind::Internal => "internal",
    }
}
