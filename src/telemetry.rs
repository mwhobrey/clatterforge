use crate::inference::{config::Target, types::Usage};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Rates are millionths of the named currency per million tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiPricing {
    pub currency: String,
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
    pub cached_input_micros_per_million: Option<u64>,
}
impl ApiPricing {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.currency.len() == 3 && self.currency.bytes().all(|b| b.is_ascii_uppercase()),
            "pricing currency must be three uppercase ASCII letters"
        );
        Ok(())
    }
    pub fn numerator(&self, usage: &Usage) -> Option<u128> {
        self.validate().ok()?;
        let input = usage.prompt_tokens?;
        let output = usage.output_tokens?;
        let cached_rate = self.cached_input_micros_per_million.or_else(|| {
            (usage.cached_tokens == Some(0)).then_some(self.input_micros_per_million)
        })?;
        // Prompt tokens include cached input. An absent cache count is not zero.
        let cached = match usage.cached_tokens {
            Some(n) if n <= input => n,
            None if cached_rate == self.input_micros_per_million => 0,
            _ => return None,
        };
        u128::from(input - cached)
            .checked_mul(self.input_micros_per_million.into())?
            .checked_add(u128::from(cached).checked_mul(cached_rate.into())?)?
            .checked_add(u128::from(output).checked_mul(self.output_micros_per_million.into())?)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timing {
    pub started_at: u64,
    pub execution_status: Option<crate::orchestration::Status>,
    pub ended_at: Option<u64>,
    pub recovery_observed_at: Option<u64>,
}

pub fn duration(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    end?.checked_sub(start?)
}

fn count(values: &[Option<u64>]) -> Value {
    let reported = values.iter().filter(|v| v.is_some()).count();
    let sum = values
        .iter()
        .flatten()
        .try_fold(0u64, |a, b| a.checked_add(*b));
    json!({
        "total": if !values.is_empty() && reported == values.len() { sum } else { None },
        "reported_sum": if reported > 0 { sum } else { None },
        "reports": reported, "requests": values.len()
    })
}

pub fn usage_summary(usages: &[&Usage]) -> Value {
    json!({
        "input": count(&usages.iter().map(|u| u.prompt_tokens).collect::<Vec<_>>()),
        "output": count(&usages.iter().map(|u| u.output_tokens).collect::<Vec<_>>()),
        "cached_input": count(&usages.iter().map(|u| u.cached_tokens).collect::<Vec<_>>())
    })
}

/// Sum unrounded per-request estimates, then round half up to currency micros.
pub fn api_cost(requests: &[(&Target, &Usage)]) -> Value {
    let estimate = (|| {
        let currency = &requests.first()?.0.api_pricing.as_ref()?.currency;
        let mut numerator = 0u128;
        for (target, usage) in requests {
            if target.provider != crate::inference::types::ProviderKind::OpenaiLocal {
                return None;
            }
            let pricing = target.api_pricing.as_ref()?;
            if &pricing.currency != currency {
                return None;
            }
            numerator = numerator.checked_add(pricing.numerator(usage)?)?;
        }
        let micros = numerator.checked_add(500_000)? / 1_000_000;
        Some(
            json!({"currency": currency, "amount": format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000), "basis": "configured token rates; estimate, not provider charges", "rounding": "half_up_to_currency_micros_after_sum"}),
        )
    })();
    estimate.unwrap_or(Value::Null)
}

pub fn review(review: Option<&crate::store::Review>) -> &'static str {
    match review {
        Some(r) if r.accepted => "accepted",
        Some(_) => "needs_work",
        None => "unavailable",
    }
}

pub fn engine_summary(task: &crate::store::Task) -> Value {
    let attempts = task
        .attempts
        .iter()
        .map(|a| {
            let protocol = a
                .process_receipt
                .as_ref()
                .and_then(|r| r.delegated.as_ref())
                .and_then(|r| r.protocol.as_ref());
            let usage = protocol.and_then(|p| p.usage.as_ref());
            json!({"execution_id": a.id, "assignment_digest": a.assignment_digest,
            "execution_status": a.execution, "started_at": a.started_at, "ended_at": a.ended_at,
            "duration_ms": duration(Some(a.started_at), a.ended_at),
            "tokens": usage_summary(&usage.into_iter().collect::<Vec<_>>()),
            "inference_requests": null, "repair_attempts": null, "estimated_api_cost": null,
            "verification": a.receipt.as_ref().map(|r| r.result.as_str()),
            "cancellation_requested": a.cancel_requested.is_some()})
        })
        .collect::<Vec<_>>();
    json!({"task_id":task.id,"status":task.status,"provider":task.assignment.backend,
        "model":task.assignment.codex.as_ref().map(|c| &c.model),"target":null,
        "billing":"unavailable; delegated CLI has no API billing contract",
        "executions": attempts,"review":review(task.review.as_ref())})
}

pub fn job_summary(job: &crate::inference::state::Job) -> Value {
    let usages = job.generations.iter().map(|g| &g.usage).collect::<Vec<_>>();
    let priced = job
        .generations
        .iter()
        .map(|g| {
            job.config
                .target(&g.resolved.target_id)
                .map(|t| (t, &g.usage))
        })
        .collect::<Result<Vec<_>>>();
    json!({"job_id":job.id,"status":job.status,"inference_requests":job.generations.len(),
        "tokens":usage_summary(&usages),"estimated_api_cost":priced.map(|r| api_cost(&r)).unwrap_or(Value::Null),
        "generations":job.generations.iter().map(|g| json!({"execution_id":g.id,"provider":g.resolved.provider,"model":g.resolved.model,"target":g.resolved.target_id,"started_at":g.started_at,"ended_at":g.ended_at,"duration_ms":g.client_duration_ms,"failure":g.failure.as_ref().map(|f| f.kind),"server_abort_confirmed":g.failure.as_ref().map(|f| f.server_abort_confirmed)})).collect::<Vec<_>>()})
}
