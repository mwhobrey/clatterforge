use crate::{digest, now};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const VERSION: u32 = 1;
// Transport/storage guard, independent of token admission against the selected model.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TEXT: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    OpenaiLocal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub streaming: bool,
    pub tools: bool,
    pub required_tool_choice: bool,
    pub parallel_tools: bool,
    pub structured_output: bool,
    pub media: bool,
    pub reasoning_control: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Part {
    Text { text: String },
    Image { url: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
}
impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![Part::Text { text: text.into() }],
            tool_calls: vec![],
            tool_call_id: None,
            reasoning: None,
        }
    }
    pub fn content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    Disabled,
    CheckedAdd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub messages: Vec<Message>,
    pub tools: ToolPolicy,
    pub require_tool: bool,
    pub require_tested: bool,
    pub max_output_tokens: u32,
    pub temperature: f64,
    pub stop: Vec<String>,
    pub deadline_ms: u64,
    pub reasoning: Option<bool>,
}
impl Request {
    pub fn fingerprint(&self) -> String {
        digest(&serde_json::to_vec(self).unwrap())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == VERSION,
            "unsupported inference contract version"
        );
        ensure!(
            !self.messages.is_empty() && self.messages.len() <= 32,
            "message count outside bounds"
        );
        ensure!(
            (1..=4096).contains(&self.max_output_tokens)
                && (100..=120000).contains(&self.deadline_ms),
            "output/deadline outside bounds"
        );
        ensure!(
            self.temperature.is_finite() && (0.0..=2.0).contains(&self.temperature),
            "invalid temperature"
        );
        ensure!(
            self.stop.len() <= 4 && self.stop.iter().all(|s| !s.is_empty() && s.len() <= 128),
            "invalid stop rules"
        );
        ensure!(
            !self.require_tool || self.tools != ToolPolicy::Disabled,
            "required tool has no definition"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_REQUEST_BYTES,
            "request exceeds 4 MiB transport bound; history is never trimmed"
        );
        validate_messages(&self.messages, false)
    }
    pub fn estimated_tokens(&self) -> u64 {
        // A declared conservative byte estimate, not an exact tokenizer measurement.
        serde_json::to_vec(&(&self.messages, tool_definitions(self.tools)))
            .unwrap()
            .len() as u64
            + 256 * self.messages.len() as u64
    }
    pub fn workload(&self) -> &'static str {
        if self.tools == ToolPolicy::Disabled {
            "text-v1"
        } else {
            "checked-add-v1"
        }
    }
}

pub fn validate_messages(messages: &[Message], allow_pending: bool) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for message in messages {
        ensure!(
            message.parts.iter().all(|p| matches!(p, Part::Text { .. })),
            "media is unsupported by direct job v1"
        );
        ensure!(
            message
                .reasoning
                .as_ref()
                .is_none_or(|s| s.len() <= MAX_TEXT),
            "reasoning exceeds bounds"
        );
        ensure!(
            message.reasoning.is_none() || message.role == Role::Assistant,
            "reasoning belongs only to assistant state"
        );
        if message.role == Role::Tool {
            let id = message
                .tool_call_id
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tool result needs call id"))?;
            ensure!(pending.remove(id), "orphan or duplicate tool result");
            ensure!(
                message.tool_calls.is_empty() && message.reasoning.is_none(),
                "invalid tool result"
            );
        } else {
            ensure!(
                pending.is_empty(),
                "unresolved tool calls before another message"
            );
            ensure!(
                message.tool_call_id.is_none(),
                "call id belongs only to a tool result"
            );
        }
        ensure!(
            message.tool_calls.is_empty() || message.role == Role::Assistant,
            "only assistant can request tools"
        );
        for call in &message.tool_calls {
            ensure!(
                !call.id.is_empty() && call.id.len() <= 128 && seen.insert(call.id.clone()),
                "duplicate/invalid tool call id"
            );
            checked_add(&call.name, &call.arguments)?;
            pending.insert(call.id.clone());
        }
    }
    ensure!(allow_pending || pending.is_empty(), "tool results missing");
    Ok(())
}

pub fn tool_definitions(policy: ToolPolicy) -> Vec<Value> {
    match policy {
        ToolPolicy::Disabled => vec![],
        ToolPolicy::CheckedAdd => vec![
            json!({"type":"function","function":{"name":"checked_add","description":"Add two signed 64-bit integers; pure application-owned operation","parameters":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}}}),
        ],
    }
}
pub fn checked_add(name: &str, args: &Value) -> Result<i64> {
    ensure!(name == "checked_add", "tool is not allowed by this job");
    let object = args
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool arguments must be object"))?;
    ensure!(
        object.len() == 2,
        "tool schema rejects missing/extra arguments"
    );
    let a = args["a"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("a must be i64"))?;
    let b = args["b"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("b must be i64"))?;
    a.checked_add(b)
        .ok_or_else(|| anyhow::anyhow!("tool arithmetic overflow"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub native_timings_ns: Value,
}

impl Usage {
    pub fn merge_report(&mut self, report: &Self) {
        if report.prompt_tokens.is_some() {
            self.prompt_tokens = report.prompt_tokens;
        }
        if report.output_tokens.is_some() {
            self.output_tokens = report.output_tokens;
        }
        if report.cached_tokens.is_some() {
            self.cached_tokens = report.cached_tokens;
        }
        if !report.native_timings_ns.is_null() {
            self.native_timings_ns = report.native_timings_ns.clone();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    UnavailableTarget,
    UnsupportedCapability,
    InvalidRequest,
    ContextOverflow,
    Timeout,
    Cancelled,
    ResourceExhaustion,
    ProtocolFailure,
    InternalFailure,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub kind: ErrorKind,
    pub diagnostic: String,
    pub server_abort_confirmed: bool,
}
impl Failure {
    pub fn new(kind: ErrorKind, message: &str) -> Self {
        Self {
            kind,
            diagnostic: message
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect(),
            server_abort_confirmed: false,
        }
    }
    /// Marks this failure as one where the provider's HTTP response for
    /// this exact request was read to completion (a definite status code
    /// plus a fully-received body), as opposed to a cancellation, timeout,
    /// or dropped connection where the server's own handling is unknown.
    /// A fully-read response means that request's handler has already
    /// returned on the provider side - M9's fix for the M8-observed
    /// quarantine-after-HTTP-500 pattern relies on this distinction to
    /// auto-release instead of requiring a human to confirm provider
    /// idleness by hand every time.
    pub fn with_server_abort_confirmed(mut self) -> Self {
        self.server_abort_confirmed = true;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: String,
        arguments: String,
    },
    UsageUpdate {
        usage: Usage,
    },
    Finish {
        reason: String,
    },
    Error {
        failure: Failure,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedTarget {
    pub target_id: String,
    pub provider: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub artifact_digest: Option<String>,
    pub runtime_version: Option<String>,
    pub configuration_digest: String,
    pub hardware_id: String,
    pub resolved_at: u64,
    pub artifact_provenance: String,
    pub capabilities_provenance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Generation {
    pub id: String,
    pub resolved: ResolvedTarget,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub estimated_prompt_tokens: u64,
    pub token_estimate_provenance: String,
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
    pub failure: Option<Failure>,
    pub terminal_events: u32,
    pub server_completion_observed: bool,
    pub client_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
}
impl Generation {
    pub fn new(resolved: ResolvedTarget, request: &Request) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            resolved,
            started_at: now(),
            ended_at: None,
            estimated_prompt_tokens: request.estimated_tokens(),
            token_estimate_provenance:
                "serialized_utf8_bytes_plus_256_per_message_v1; estimate, not native token count"
                    .into(),
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![],
            usage: Usage::default(),
            finish_reason: None,
            failure: None,
            terminal_events: 0,
            server_completion_observed: false,
            client_duration_ms: None,
            request_digest: Some(request.fingerprint()),
        }
    }
    pub fn evidence_digest(&self) -> String {
        digest(&serde_json::to_vec(self).unwrap())
    }
}
