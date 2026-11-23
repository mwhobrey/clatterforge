use crate::store::{Store, Task};
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct EngineCapabilities {
    pub engine: &'static str,
    pub worktree_isolation: bool,
    pub owned_process_cancellation: bool,
    pub evidence_capture: bool,
    pub resumable_inference: bool,
    pub internal_inference: &'static str,
    pub tool_execution_owner: &'static str,
}

pub trait ExecutionEngine {
    fn capabilities(&self) -> EngineCapabilities;
    fn execute(&self, store: &mut Store, task: &str, revision: u64) -> Result<Task>;
}

pub struct CodexEngine;
impl ExecutionEngine for CodexEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            engine: "codex-exec-v1",
            worktree_isolation: true,
            owned_process_cancellation: true,
            evidence_capture: true,
            resumable_inference: false,
            internal_inference: "opaque; delegated to Codex CLI",
            tool_execution_owner: "Codex CLI; independent verification owned by conductor",
        }
    }
    fn execute(&self, store: &mut Store, task: &str, revision: u64) -> Result<Task> {
        ensure!(
            store.task(task)?.assignment.backend == "codex-exec-v1",
            "wrong execution engine"
        );
        store.run(task, revision)
    }
}
