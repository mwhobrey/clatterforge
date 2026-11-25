//! `ConductorBackend`: the trait both `clatterforge`'s in-process `LocalBackend` (stdio
//! `mcp-serve`, and the new `/api/edge/tools/call` HTTP route) and `clatterforge-edge`'s
//! `RemoteBackend` (an authenticated HTTP client talking to that same route) implement.
//! One tool, one method, one request/response shape - see `M12_0_ARCHITECTURE.md` section 2
//! for why this is a trait rather than a second hand-copied adapter.
//!
//! `async fn` in this trait is intentionally not `Send`-bounded via `async-trait` or an
//! explicit associated-future type: neither implementation is ever used as `dyn
//! ConductorBackend` (each binary picks one concrete backend at compile time), so the
//! `async_fn_in_trait` auto-trait warning this would otherwise produce doesn't apply here.

use crate::{error::BackendError, types::*};

#[allow(async_fn_in_trait)]
pub trait ConductorBackend {
    async fn conductor_capabilities(&self) -> Result<CapabilitiesResponse, BackendError>;

    async fn conductor_list_repositories(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListRepositoriesResponse, BackendError>;

    async fn conductor_list_tasks(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListTasksResponse, BackendError>;

    async fn conductor_get_task(&self, req: GetTaskRequest) -> Result<TaskResponse, BackendError>;

    async fn conductor_propose_task(
        &self,
        req: ProposeTaskRequest,
    ) -> Result<ProposeTaskResponse, BackendError>;

    async fn conductor_submit_task(
        &self,
        req: SubmitTaskRequest,
    ) -> Result<SubmitTaskResponse, BackendError>;

    async fn conductor_schedule_task(
        &self,
        req: ScheduleTaskRequest,
    ) -> Result<ScheduleTaskResponse, BackendError>;

    async fn conductor_list_schedules(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListSchedulesResponse, BackendError>;

    async fn conductor_cancel_schedule(
        &self,
        req: CancelScheduleRequest,
    ) -> Result<ListSchedulesResponse, BackendError>;

    async fn conductor_get_execution(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError>;

    async fn conductor_get_evidence(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError>;

    async fn conductor_get_approval_requirements(
        &self,
        req: TaskIdRequest,
    ) -> Result<ApprovalRequirementsResponse, BackendError>;

    async fn conductor_approve(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError>;

    async fn conductor_reject(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError>;

    async fn conductor_resume(&self, req: ResumeRequest) -> Result<ResumeResponse, BackendError>;

    async fn conductor_cancel_task(
        &self,
        req: CancelTaskRequest,
    ) -> Result<DecisionResponse, BackendError>;

    async fn conductor_global_stop_status(
        &self,
        req: RequestIdOnly,
    ) -> Result<StopStatusResponse, BackendError>;

    async fn conductor_get_evidence_summary(
        &self,
        req: EvidenceSummaryRequest,
    ) -> Result<EvidenceSummaryResponse, BackendError>;

    async fn conductor_get_artifact(
        &self,
        req: GetArtifactRequest,
    ) -> Result<GetArtifactResponse, BackendError>;
}
