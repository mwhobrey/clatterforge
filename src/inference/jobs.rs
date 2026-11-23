use super::{
    config::{Config, Registry, Target},
    provider::{HttpProvider, InferenceProvider},
    state::{Coordinator, Job, JobStatus, JobStore},
    types::*,
};
use crate::{now, process::current_identity};
use anyhow::{Result, ensure};
use serde_json::json;
use std::path::Path;

type RouteResult = std::result::Result<(Target, ResolvedTarget, String), Failure>;

fn tested_record<'a>(
    registry: Option<&'a Registry>,
    target: &Target,
    config: &Config,
    resolved: Option<&ResolvedTarget>,
) -> Option<&'a super::config::RegistryRecord> {
    registry
        .and_then(|r| r.current(target, config))
        .filter(|r| {
            resolved.is_none_or(|v| {
                v.runtime_version.as_ref() == Some(&r.tested.runtime_version)
                    && v.artifact_digest.as_ref() == Some(&r.tested.artifact_digest)
            })
        })
}

pub async fn route(
    config: &Config,
    registry: Option<&Registry>,
    request: &Request,
    override_id: Option<&str>,
) -> RouteResult {
    let mut candidates: Vec<_> = config.targets.iter().collect();
    candidates.sort_by_key(|t| (t.id != config.default_target, t.priority, t.id.clone()));
    if let Some(id) = override_id {
        candidates.retain(|t| t.id == id);
    } else if !config.fallback_before_dispatch {
        candidates.retain(|t| t.id == config.default_target);
    }
    let mut available = vec![];
    let mut failure = Failure::new(
        ErrorKind::UnavailableTarget,
        "no configured target matches selection",
    );
    for target in candidates {
        let tested = tested_record(registry, target, config, None).map(|r| &r.tested);
        if let Err(error) = target.eligible(request, tested) {
            failure = error;
            continue;
        }
        let provider = HttpProvider::new(target.provider)
            .map_err(|_| Failure::new(ErrorKind::InternalFailure, "HTTP client setup failed"))?;
        match provider.resolve(target, &config.hardware_id).await {
            Ok(resolved) => {
                if request.require_tested
                    && tested_record(registry, target, config, Some(&resolved)).is_none()
                {
                    failure = Failure::new(
                        ErrorKind::UnsupportedCapability,
                        "tested capability artifact/runtime differs from resolved target",
                    );
                    continue;
                }
                available.push((target.clone(), resolved));
            }
            Err(error) => failure = error,
        }
    }
    if available.is_empty() {
        return Err(failure);
    }
    let request_digest = request.fingerprint();
    let scores: Option<Vec<f64>> = available
        .iter()
        .map(|(target, resolved)| {
            let record = tested_record(registry, target, config, Some(resolved))?;
            let measurement = record.measurement.as_ref()?;
            if measurement.hardware_id != config.hardware_id
                || measurement.workload != request.workload()
                || measurement.request_digest != request_digest
                || now().saturating_sub(measurement.measured_at) > config.registry_max_age_ms
            {
                return None;
            }
            measurement.latency_ms
        })
        .collect();
    let (index, basis) = if override_id.is_some() {
        (0, "explicit exact target override")
    } else if let Some(scores) = scores {
        (
            scores
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0,
            "fresh comparable measurements for every eligible available target",
        )
    } else {
        (
            0,
            "manual default/priority; measurements absent, stale, unknown or incomparable",
        )
    };
    let (target, resolved) = available.swap_remove(index);
    Ok((target, resolved, basis.into()))
}

struct Reservation {
    coordinator: Coordinator,
    resource: String,
    job: String,
    finished: bool,
}
impl Reservation {
    fn finish(&mut self, uncertain: bool) -> Result<()> {
        if uncertain {
            self.coordinator.quarantine_job(&self.job)?;
        } else {
            self.coordinator.release(&self.resource, &self.job)?;
        }
        self.finished = true;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.coordinator.quarantine_job(&self.job);
        }
    }
}

fn complete_tools(store: &mut JobStore, id: &str, owner: &str) -> Result<Job> {
    let job = store.job(id)?;
    validate_messages(&job.request.messages, true)?;
    let pending = job
        .request
        .messages
        .last()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.tool_calls.clone())
        .unwrap_or_default();
    if pending.is_empty() {
        return Ok(job);
    }
    ensure!(
        job.request.tools == ToolPolicy::CheckedAdd,
        "tool policy disabled"
    );
    let mut results = vec![];
    for call in pending {
        let value = checked_add(&call.name, &call.arguments)?;
        let mut result = Message::text(Role::Tool, value.to_string());
        result.tool_call_id = Some(call.id);
        results.push(result);
    }
    store.mutate(
        id,
        None,
        Some(owner),
        "pure_tools_completed",
        json!({"results":results}),
        |j| {
            j.request.messages.extend(results);
            j.request.require_tool = false;
            validate_messages(&j.request.messages, false)?;
            Ok(())
        },
    )
}

async fn cancellation_signal(monitor: &JobStore, id: &str) {
    while !monitor.job(id).map(|j| j.cancel_requested).unwrap_or(true) {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub async fn run(
    root: &Path,
    id: &str,
    revision: u64,
    target_override: Option<&str>,
    one_turn: bool,
) -> Result<Job> {
    let mut store = JobStore::open(root)?;
    let original = store.job(id)?;
    ensure!(original.revision == revision, "job revision conflict");
    ensure!(
        matches!(original.status, JobStatus::Ready | JobStatus::Checkpoint)
            && !original.cancel_requested,
        "only ready or safe-checkpoint jobs can run; uncertain/terminal jobs never replay"
    );
    ensure!(
        original.owner_token.is_none()
            || original.owner.as_ref().is_some_and(|o| !matches!(
                crate::process::presence(o),
                crate::process::Presence::Alive | crate::process::Presence::Unverifiable
            )),
        "checkpoint still has a live/uninspectable owner"
    );
    ensure!(
        original.generations.len() < original.max_generations,
        "bounded generation budget exhausted"
    );
    original.config.validate()?;
    if let Some(registry) = &original.registry {
        registry.validate(&original.config)?;
    }
    let owner = current_identity()?;
    let monitor = JobStore::open(root)?;
    let token = uuid::Uuid::new_v4().to_string();
    store.mutate(
        id,
        Some(revision),
        None,
        "owner_claimed",
        json!({"owner":owner}),
        |j| {
            j.owner = Some(owner.clone());
            j.owner_token = Some(token.clone());
            j.status = JobStatus::Preparing;
            Ok(())
        },
    )?;
    let mut job = complete_tools(&mut store, id, &token)?;
    if job.request.validate().is_err() {
        let failure = Failure::new(
            ErrorKind::InvalidRequest,
            "canonical checkpoint exceeds bounded request contract; nothing was trimmed",
        );
        return store.mutate(
            id,
            None,
            Some(&token),
            "preflight_failed",
            json!({"failure":failure}),
            |j| {
                j.status = if j.cancel_requested {
                    JobStatus::Cancelled
                } else {
                    JobStatus::Failed
                };
                j.failure = Some(failure);
                Ok(())
            },
        );
    }
    let selection = target_override
        .or(job.selected_target.as_deref())
        .or(job.target_override.as_deref());
    let routed = tokio::select! {
        biased;
        _ = cancellation_signal(&monitor, id) => Err(Failure::new(ErrorKind::Cancelled, "cancelled during discovery; no generation dispatched")),
        result = route(&job.config, job.registry.as_ref(), &job.request, selection) => result,
    };
    let (target, resolved, basis) = match routed {
        Ok(value) => value,
        Err(failure) => {
            return store.mutate(
                id,
                None,
                Some(&token),
                "preflight_failed",
                json!({"failure":failure}),
                |j| {
                    j.status = if j.cancel_requested || failure.kind == ErrorKind::Cancelled {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Failed
                    };
                    j.failure = Some(failure);
                    Ok(())
                },
            );
        }
    };
    let mut coordinator = Coordinator::open(&job.config.admission_db)?;
    if let Err(error) = coordinator.acquire(&target.resource_id, id, &owner) {
        // Bounded, conservative recovery: a *prior* job may have left this
        // resource quarantined after its own owner process already died.
        // Try exactly once, only releasing when that owner is confirmed
        // not alive/unverifiable (the same bar manual acknowledgement
        // uses) - never when the owner's status is itself unknown.
        let recovery = coordinator.try_automatic_release(&target.resource_id)?;
        let retried = if recovery == super::state::AutoRelease::Released {
            coordinator.acquire(&target.resource_id, id, &owner)
        } else {
            Err(error)
        };
        if let Err(error) = retried {
            store.mutate(
                id,
                None,
                Some(&token),
                "admission_denied",
                json!({"resource":target.resource_id,"automatic_recovery":recovery}),
                |j| {
                    j.status = if j.cancel_requested {
                        JobStatus::Cancelled
                    } else if j.generations.is_empty() {
                        JobStatus::Ready
                    } else {
                        JobStatus::Checkpoint
                    };
                    j.owner = None;
                    j.owner_token = None;
                    Ok(())
                },
            )?;
            return Err(error);
        }
        store.mutate(
            id,
            None,
            Some(&token),
            "admission_auto_recovered",
            json!({"resource":target.resource_id}),
            |_| Ok(()),
        )?;
    }
    let mut reservation = Reservation {
        coordinator,
        resource: target.resource_id.clone(),
        job: id.into(),
        finished: false,
    };
    let provider = HttpProvider::new(target.provider)?;
    let pinned = resolved;
    loop {
        job = store.job(id)?;
        if job.cancel_requested {
            reservation.finish(false)?;
            return store.mutate(
                id,
                None,
                Some(&token),
                "cancelled_before_dispatch",
                json!({}),
                |j| {
                    j.status = JobStatus::Cancelled;
                    Ok(())
                },
            );
        }
        if job.request.validate().is_err() {
            reservation.finish(false)?;
            let failure = Failure::new(
                ErrorKind::InvalidRequest,
                "canonical checkpoint exceeds bounded request contract; nothing was trimmed",
            );
            return store.mutate(
                id,
                None,
                Some(&token),
                "request_failed",
                json!({"failure":failure}),
                |j| {
                    j.status = JobStatus::Failed;
                    j.failure = Some(failure);
                    Ok(())
                },
            );
        }
        let current = tokio::select! {
            biased;
            _ = cancellation_signal(&monitor, id) => Err(Failure::new(ErrorKind::Cancelled, "cancelled during target revalidation; no new generation dispatched")),
            result = provider.resolve(&target, &job.config.hardware_id) => result,
        };
        if !matches!(&current,Ok(v) if v.model==pinned.model && v.artifact_digest==pinned.artifact_digest && v.runtime_version==pinned.runtime_version)
        {
            reservation.finish(false)?;
            let failure = current.err().unwrap_or_else(|| {
                Failure::new(
                    ErrorKind::UnavailableTarget,
                    "resolved model/runtime changed at checkpoint; no dispatch",
                )
            });
            return store.mutate(
                id,
                None,
                Some(&token),
                "target_resolution_failed",
                json!({"failure":failure}),
                |j| {
                    j.status = if j.cancel_requested || failure.kind == ErrorKind::Cancelled {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Failed
                    };
                    j.failure = Some(failure);
                    Ok(())
                },
            );
        }
        if let Err(failure) = target.eligible(
            &job.request,
            tested_record(job.registry.as_ref(), &target, &job.config, Some(&pinned))
                .map(|r| &r.tested),
        ) {
            reservation.finish(false)?;
            return store.mutate(
                id,
                None,
                Some(&token),
                "capability_failed",
                json!({"failure":failure}),
                |j| {
                    j.status = JobStatus::Failed;
                    j.failure = Some(failure);
                    Ok(())
                },
            );
        }
        let generation = Generation::new(pinned.clone(), &job.request);
        let request = job.request.clone();
        let intent = store.mutate(
            id,
            None,
            Some(&token),
            "generation_intent",
            json!({"resolved":pinned,"generation_id":generation.id}),
            |j| {
                if j.cancel_requested {
                    j.status = JobStatus::Cancelled;
                    return Ok(());
                }
                j.status = JobStatus::Running;
                j.selected_target = Some(target.id.clone());
                j.routing_basis = Some(basis.clone());
                j.generations.push(generation.clone());
                Ok(())
            },
        )?;
        if intent.status == JobStatus::Cancelled {
            reservation.finish(false)?;
            return Ok(intent);
        }
        let mut sink = |event| store.stream_event(id, &token, event).map(|_| ());
        let cancelled = || monitor.job(id).map(|j| j.cancel_requested).unwrap_or(true);
        let generation = provider
            .generate(&target, &request, generation, &mut sink, &cancelled)
            .await;
        let failure = generation.failure.clone();
        job = store.mutate(
            id,
            None,
            Some(&token),
            "generation_checkpoint",
            json!({"evidence_digest":generation.evidence_digest(),"failure":failure}),
            |j| {
                *j.generations.last_mut().unwrap() = generation.clone();
                if let Some(failure) = &failure {
                    j.status = match failure.kind {
                        ErrorKind::Cancelled => JobStatus::Cancelled,
                        ErrorKind::Timeout => JobStatus::TimedOut,
                        _ => JobStatus::Failed,
                    };
                    j.failure = Some(failure.clone());
                } else {
                    let mut message = Message::text(Role::Assistant, &generation.text);
                    message.tool_calls = generation.tool_calls.clone();
                    if !generation.reasoning.is_empty() {
                        message.reasoning = Some(generation.reasoning.clone());
                    }
                    j.request.messages.push(message);
                    validate_messages(&j.request.messages, true)?;
                    j.status = if j.cancel_requested {
                        JobStatus::Cancelled
                    } else if generation.tool_calls.is_empty() {
                        JobStatus::Completed
                    } else {
                        JobStatus::Checkpoint
                    };
                }
                Ok(())
            },
        )?;
        if let Some(failure) = &failure {
            // A generation that errored can still leave the resource safe
            // to release immediately rather than quarantined, when the
            // provider's own response for this exact request was read to
            // completion (`server_abort_confirmed`) - e.g. a clean HTTP
            // 500. That is a positive confirmation the provider is done
            // with this request, distinct from a timeout/cancellation/
            // dropped stream, where whether generation continues
            // server-side is genuinely unknown and quarantine is correct.
            let uncertain =
                !generation.server_completion_observed && !failure.server_abort_confirmed;
            reservation.finish(uncertain)?;
            return Ok(job);
        }
        if job.status != JobStatus::Checkpoint {
            reservation.finish(false)?;
            return Ok(job);
        }
        job = complete_tools(&mut store, id, &token)?;
        if one_turn || job.generations.len() == job.max_generations {
            reservation.finish(false)?;
            return store.mutate(
                id,
                None,
                Some(&token),
                "safe_checkpoint_released",
                json!({}),
                |j| {
                    if j.generations.len() == j.max_generations {
                        j.status = JobStatus::Failed;
                        j.failure = Some(Failure::new(ErrorKind::ResourceExhaustion,
                            "bounded generation budget exhausted before final answer; canonical tool results retained; no replay"));
                    }
                    j.owner = None;
                    j.owner_token = None;
                    Ok(())
                },
            );
        }
    }
}
