//! `conductor-protocol`: the shared MCP tool contract for Dev Conductor's M12.0 edge
//! architecture - DTOs, the `ConductorBackend` trait, and `BackendError` - with no
//! dependency on `rmcp`, `rusqlite`, `axum`, or any execution engine. Both
//! `dev-conductor` (the `LocalBackend` implementation, used by stdio `mcp-serve` and the
//! `/api/edge/tools/call` HTTP route) and `dev-conductor-edge` (`RemoteBackend`, an HTTP
//! client implementing the identical trait) depend on this crate and nothing heavier for
//! their shared schema - see `docs/m12/M12_0_ARCHITECTURE.md`.

mod backend;
mod error;
mod types;

pub use backend::ConductorBackend;
pub use error::{BackendError, ErrorKind};
pub use types::*;
