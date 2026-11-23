use super::{config::Target, types::*};
use crate::now;
use futures_util::{StreamExt, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

type Outcome<T> = std::result::Result<T, Failure>;
pub type EventSink<'a> = &'a mut dyn FnMut(StreamEvent) -> anyhow::Result<()>;

pub trait InferenceProvider {
    fn resolve<'a>(
        &'a self,
        target: &'a Target,
        hardware: &'a str,
    ) -> LocalBoxFuture<'a, Outcome<ResolvedTarget>>;
    fn generate<'a>(
        &'a self,
        target: &'a Target,
        request: &'a Request,
        generation: Generation,
        sink: EventSink<'a>,
        cancelled: &'a dyn Fn() -> bool,
    ) -> LocalBoxFuture<'a, Generation>;
}

pub struct HttpProvider {
    kind: ProviderKind,
    client: reqwest::Client,
}
impl HttpProvider {
    pub fn new(kind: ProviderKind) -> anyhow::Result<Self> {
        Ok(Self {
            kind,
            client: reqwest::Client::builder()
                .no_proxy()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .build()?,
        })
    }
    fn url(&self, target: &Target, path: &str) -> Outcome<reqwest::Url> {
        target
            .endpoint_url()
            .and_then(|url| Ok(url.join(path)?))
            .map_err(|_| Failure::new(ErrorKind::InvalidRequest, "invalid local endpoint"))
    }
    async fn get_json(&self, target: &Target, path: &str) -> Outcome<Value> {
        let url = self.url(target, path)?;
        let work = async {
            let response = self.client.get(url).send().await.map_err(transport_error)?;
            let bytes = response_bytes(response, 1048576).await?;
            serde_json::from_slice(&bytes)
                .map_err(|_| Failure::new(ErrorKind::ProtocolFailure, "invalid discovery JSON"))
        };
        tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .unwrap_or_else(|_| {
                Err(Failure::new(
                    ErrorKind::Timeout,
                    "discovery deadline exceeded",
                ))
            })
    }
    async fn resolve_inner(&self, target: &Target, hardware: &str) -> Outcome<ResolvedTarget> {
        if self.kind != target.provider {
            return Err(Failure::new(
                ErrorKind::InvalidRequest,
                "adapter/target mismatch",
            ));
        }
        let (artifact, version, provenance) = match self.kind {
            ProviderKind::Ollama => {
                let listed = self.get_json(target, "api/tags").await?;
                let models = listed["models"].as_array().ok_or_else(|| {
                    Failure::new(ErrorKind::ProtocolFailure, "missing installed model list")
                })?;
                let model = models
                    .iter()
                    .find(|m| m["name"] == target.model)
                    .ok_or_else(|| {
                        Failure::new(
                            ErrorKind::UnavailableTarget,
                            "exact installed Ollama model not found",
                        )
                    })?;
                if model.get("remote_host").is_some_and(|v| !v.is_null()) {
                    return Err(Failure::new(
                        ErrorKind::UnsupportedCapability,
                        "remote-backed Ollama model is outside local contract",
                    ));
                }
                let artifact = model["digest"]
                    .as_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_owned);
                if artifact.is_none() {
                    return Err(Failure::new(
                        ErrorKind::ProtocolFailure,
                        "installed model digest missing",
                    ));
                }
                let version = self.get_json(target, "api/version").await?["version"]
                    .as_str()
                    .map(str::to_owned);
                (
                    artifact,
                    version,
                    "Ollama installed model digest; residency is not inferred",
                )
            }
            ProviderKind::OpenaiLocal => {
                let listed = self.get_json(target, "models").await?;
                let models = listed["data"].as_array().ok_or_else(|| {
                    Failure::new(ErrorKind::ProtocolFailure, "missing model list")
                })?;
                let model = models
                    .iter()
                    .find(|m| m["id"] == target.model)
                    .ok_or_else(|| {
                        Failure::new(
                            ErrorKind::UnavailableTarget,
                            "exact OpenAI-local model id not found",
                        )
                    })?;
                let mut artifact = model["digest"]
                    .as_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(str::to_owned);
                let mut version = listed["runtime_version"].as_str().map(str::to_owned);
                let mut provenance =
                    "runtime model-list id; digest/version unavailable unless explicitly reported";
                if target.extensions.is_some() {
                    // This option is not portable OpenAI capability. Verify the named backend,
                    // exact installed artifact and local ownership before sending it.
                    let mut native = target.clone();
                    let mut root = target.endpoint_url().map_err(|_| {
                        Failure::new(ErrorKind::InvalidRequest, "invalid local endpoint")
                    })?;
                    root.set_path("/");
                    native.endpoint = root.to_string();
                    let installed = self.get_json(&native, "api/tags").await?;
                    let entry = installed["models"]
                        .as_array()
                        .and_then(|models| models.iter().find(|m| m["name"] == target.model))
                        .ok_or_else(|| {
                            Failure::new(
                                ErrorKind::UnsupportedCapability,
                                "Ollama-specific option requires the exact installed Ollama model",
                            )
                        })?;
                    if entry.get("remote_host").is_some_and(|v| !v.is_null()) {
                        return Err(Failure::new(
                            ErrorKind::UnsupportedCapability,
                            "remote-backed Ollama model is outside local contract",
                        ));
                    }
                    artifact = entry["digest"]
                        .as_str()
                        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                        .map(str::to_owned);
                    version = self.get_json(&native, "api/version").await?["version"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    if artifact.is_none() || version.is_none() {
                        return Err(Failure::new(
                            ErrorKind::ProtocolFailure,
                            "Ollama-specific option requires artifact and runtime provenance",
                        ));
                    }
                    provenance = "explicit ollama.openai extension; native installed artifact/version discovery";
                }
                (artifact, version, provenance)
            }
        };
        if target
            .expected_artifact_digest
            .as_ref()
            .is_some_and(|expected| artifact.as_ref() != Some(expected))
        {
            return Err(Failure::new(
                ErrorKind::UnavailableTarget,
                "resolved artifact differs from configured pin or is unknown",
            ));
        }
        Ok(ResolvedTarget {
            target_id: target.id.clone(),
            provider: self.kind,
            endpoint: target.endpoint.clone(),
            model: target.model.clone(),
            artifact_digest: artifact,
            runtime_version: version,
            configuration_digest: target.fingerprint(),
            hardware_id: hardware.into(),
            resolved_at: now(),
            artifact_provenance: provenance.into(),
            capabilities_provenance: format!(
                "configured/advertised: {}; tested evidence is a separate registry record",
                target.capability_source
            ),
        })
    }
    async fn stream(
        &self,
        target: &Target,
        request: &Request,
        generation: &mut Generation,
        sink: EventSink<'_>,
    ) -> Outcome<()> {
        let (path, body) = encode(target, request);
        let response = self
            .client
            .post(self.url(target, path)?)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        let response = checked_response(response).await?;
        let mut stream = response.bytes_stream();
        let mut parser = Decoder::new(self.kind, &generation.id, &target.model);
        let mut buffered = Vec::new();
        let mut total = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_error)?;
            total += chunk.len();
            if total > 4 * 1024 * 1024 {
                return Err(Failure::new(
                    ErrorKind::ResourceExhaustion,
                    "stream exceeds 4 MiB limit",
                ));
            }
            for byte in chunk {
                if byte == b'\n' {
                    for event in parser.line(&buffered)? {
                        apply(generation, &event)?;
                        sink(event).map_err(sink_error)?;
                    }
                    buffered.clear();
                } else {
                    if buffered.len() >= 256 * 1024 {
                        return Err(Failure::new(
                            ErrorKind::ResourceExhaustion,
                            "stream line exceeds 256 KiB",
                        ));
                    }
                    buffered.push(byte);
                }
            }
        }
        if !buffered.is_empty() {
            for event in parser.line(&buffered)? {
                apply(generation, &event)?;
                sink(event).map_err(sink_error)?;
            }
        }
        generation.server_completion_observed = parser.done && parser.ended;
        let (calls, reason) = parser.finish()?;
        if request.tools == ToolPolicy::Disabled && !calls.is_empty() {
            return Err(Failure::new(
                ErrorKind::UnsupportedCapability,
                "provider requested disabled tools",
            ));
        }
        if request.require_tool && calls.is_empty() {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "required tool call missing",
            ));
        }
        if calls.is_empty() && generation.text.trim().is_empty() {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "empty/reasoning-only output is not a usable answer",
            ));
        }
        if !calls.is_empty() && reason != "tool_calls" {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "tool calls without matching finish reason",
            ));
        }
        generation.tool_calls = calls;
        generation.finish_reason = Some(reason);
        Ok(())
    }
}
impl InferenceProvider for HttpProvider {
    fn resolve<'a>(
        &'a self,
        target: &'a Target,
        hardware: &'a str,
    ) -> LocalBoxFuture<'a, Outcome<ResolvedTarget>> {
        Box::pin(self.resolve_inner(target, hardware))
    }
    fn generate<'a>(
        &'a self,
        target: &'a Target,
        request: &'a Request,
        mut generation: Generation,
        mut sink: EventSink<'a>,
        cancelled: &'a dyn Fn() -> bool,
    ) -> LocalBoxFuture<'a, Generation> {
        Box::pin(async move {
            let began = Instant::now();
            let result = tokio::select! {
                biased;
                _ = async { while !cancelled() { tokio::time::sleep(Duration::from_millis(20)).await; } } => Err(Failure::new(ErrorKind::Cancelled, "cancelled; HTTP future/stream dropped; server abort unconfirmed")),
                _ = tokio::time::sleep(Duration::from_millis(request.deadline_ms)) => Err(Failure::new(ErrorKind::Timeout, "generation deadline exceeded; server abort unconfirmed")),
                result = self.stream(target, request, &mut generation, &mut sink) => result,
            };
            generation.ended_at = Some(now());
            generation.client_duration_ms = Some(began.elapsed().as_millis() as u64);
            let terminal = match result {
                Ok(()) => StreamEvent::Finish {
                    reason: generation.finish_reason.clone().unwrap(),
                },
                Err(failure) => {
                    generation.failure = Some(failure.clone());
                    StreamEvent::Error { failure }
                }
            };
            generation.terminal_events = 1;
            if sink(terminal).is_err() {
                generation.failure = Some(Failure::new(
                    ErrorKind::InternalFailure,
                    "terminal checkpoint could not be saved",
                ));
            }
            generation
        })
    }
}

fn sink_error(_: anyhow::Error) -> Failure {
    Failure::new(
        ErrorKind::InternalFailure,
        "stream checkpoint could not be saved",
    )
}
fn transport_error(error: reqwest::Error) -> Failure {
    Failure::new(
        if error.is_timeout() {
            ErrorKind::Timeout
        } else if error.is_connect() {
            ErrorKind::UnavailableTarget
        } else {
            ErrorKind::ProtocolFailure
        },
        &format!("local HTTP transport failed; no automatic retry: {error:?}"),
    )
}
async fn checked_response(response: reqwest::Response) -> Outcome<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let fallback = match status {
        404 => ErrorKind::UnavailableTarget,
        429 | 503 | 507 => ErrorKind::ResourceExhaustion,
        400 | 422 => ErrorKind::InvalidRequest,
        _ => ErrorKind::InternalFailure,
    };
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if bytes.len() + chunk.len() > 16384 {
            return Err(Failure::new(
                ErrorKind::ResourceExhaustion,
                "runtime error response exceeds 16 KiB diagnostic bound",
            ));
        }
        bytes.extend(chunk);
    }
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let kind = runtime_error_kind(&body, fallback);
    // The full error response (status + body) was read to completion over
    // an un-dropped connection, so the provider's handler for this exact
    // request has already returned - unlike a timeout/cancellation/stream
    // drop, there is no ambiguity about whether generation continues
    // server-side after we see this error.
    Err(Failure::new(
        kind,
        &format!("runtime HTTP status {status}; provider body omitted to avoid diagnostic leakage"),
    )
    .with_server_abort_confirmed())
}
fn runtime_error_kind(value: &Value, fallback: ErrorKind) -> ErrorKind {
    let error = &value["error"];
    let text = error
        .as_str()
        .or_else(|| error["message"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let code = error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or("");
    match code {
        "context_length_exceeded" => ErrorKind::ContextOverflow,
        "model_not_found" => ErrorKind::UnavailableTarget,
        "unsupported_parameter" | "unsupported_capability" => ErrorKind::UnsupportedCapability,
        "rate_limit_exceeded" | "out_of_memory" => ErrorKind::ResourceExhaustion,
        _ if text.contains("context length") || text.contains("context window") => {
            ErrorKind::ContextOverflow
        }
        _ if text.contains("out of memory") => ErrorKind::ResourceExhaustion,
        _ => fallback,
    }
}
async fn response_bytes(response: reqwest::Response, cap: usize) -> Outcome<Vec<u8>> {
    let mut stream = checked_response(response).await?.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if bytes.len() + chunk.len() > cap {
            return Err(Failure::new(
                ErrorKind::ResourceExhaustion,
                "discovery response exceeds bounds",
            ));
        }
        bytes.extend(chunk);
    }
    Ok(bytes)
}

fn encode(target: &Target, request: &Request) -> (&'static str, Value) {
    let messages: Vec<_> = request.messages.iter().map(|message| {
        let mut value = json!({"role":message.role,"content":message.content()});
        if !message.tool_calls.is_empty() {
            value["tool_calls"] = Value::Array(message.tool_calls.iter().enumerate().map(|(index, c)| match target.provider {
                ProviderKind::Ollama => json!({"type":"function","function":{"index":index,"name":c.name,"arguments":c.arguments}}),
                ProviderKind::OpenaiLocal => json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments.to_string()}}),
            }).collect());
        }
        if let Some(id) = &message.tool_call_id {
            match target.provider {
                ProviderKind::Ollama => value["tool_name"] = json!("checked_add"),
                ProviderKind::OpenaiLocal => value["tool_call_id"] = json!(id),
            }
        }
        if target.provider == ProviderKind::Ollama
            && let Some(reasoning) = &message.reasoning
        {
            value["thinking"] = json!(reasoning);
        }
        value
    }).collect();
    let mut value = json!({"model":target.model,"messages":messages,"stream":true});
    if request.tools != ToolPolicy::Disabled {
        value["tools"] = json!(tool_definitions(request.tools));
    }
    match target.provider {
        ProviderKind::Ollama => {
            value["options"] = json!({"num_ctx":target.context_limit,"num_predict":request.max_output_tokens,"temperature":request.temperature,"stop":request.stop});
            if let Some(reasoning) = request.reasoning {
                value["think"] = json!(reasoning);
            }
            ("api/chat", value)
        }
        ProviderKind::OpenaiLocal => {
            if let Some(extension) = &target.extensions {
                value["reasoning_effort"] = json!(extension.ollama_openai.reasoning_effort);
            }
            value["max_tokens"] = json!(request.max_output_tokens);
            value["temperature"] = json!(request.temperature);
            value["stop"] = json!(request.stop);
            value["stream_options"] = json!({"include_usage":true});
            if request.tools != ToolPolicy::Disabled {
                value["tool_choice"] = json!(if request.require_tool {
                    "required"
                } else {
                    "auto"
                });
                value["parallel_tool_calls"] = json!(false);
            }
            ("chat/completions", value)
        }
    }
}

fn apply(generation: &mut Generation, event: &StreamEvent) -> Outcome<()> {
    match event {
        StreamEvent::TextDelta { text } => generation.text.push_str(text),
        StreamEvent::ReasoningDelta { text } => generation.reasoning.push_str(text),
        StreamEvent::UsageUpdate { usage } => generation.usage.merge_report(usage),
        _ => (),
    }
    if generation.text.len() + generation.reasoning.len() > 65536 {
        return Err(Failure::new(
            ErrorKind::ResourceExhaustion,
            "generation exceeds 64 KiB retained output",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct CallParts {
    id: Option<String>,
    name: String,
    arguments: String,
}
pub struct Decoder {
    kind: ProviderKind,
    generation: String,
    model: String,
    done: bool,
    ended: bool,
    reason: Option<String>,
    calls: BTreeMap<usize, CallParts>,
    events: usize,
}
impl Decoder {
    pub fn new(kind: ProviderKind, generation: &str, model: &str) -> Self {
        Self {
            kind,
            generation: generation.into(),
            model: model.into(),
            done: false,
            ended: false,
            reason: None,
            calls: BTreeMap::new(),
            events: 0,
        }
    }
    pub fn line(&mut self, bytes: &[u8]) -> Outcome<Vec<StreamEvent>> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Failure::new(ErrorKind::ProtocolFailure, "invalid stream UTF-8"))?
            .trim();
        if text.is_empty() || (self.kind == ProviderKind::OpenaiLocal && text.starts_with(':')) {
            return Ok(vec![]);
        }
        self.events += 1;
        if self.events > 8192 || text.len() > 256 * 1024 {
            return Err(Failure::new(
                ErrorKind::ResourceExhaustion,
                "event count/size limit exceeded",
            ));
        }
        if self.ended {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "data after terminal marker",
            ));
        }
        let data = if self.kind == ProviderKind::OpenaiLocal {
            text.strip_prefix("data:")
                .ok_or_else(|| {
                    Failure::new(
                        ErrorKind::ProtocolFailure,
                        "unsupported SSE field/multiline event",
                    )
                })?
                .trim()
        } else {
            text
        };
        if data == "[DONE]" {
            if !self.done {
                return Err(Failure::new(
                    ErrorKind::ProtocolFailure,
                    "SSE ended without finish reason",
                ));
            }
            self.ended = true;
            return Ok(vec![]);
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| Failure::new(ErrorKind::ProtocolFailure, "malformed stream JSON"))?;
        if value.get("error").is_some() {
            return Err(Failure::new(
                runtime_error_kind(&value, ErrorKind::ProtocolFailure),
                "runtime stream reported error",
            ));
        }
        if value["model"]
            .as_str()
            .is_some_and(|model| model != self.model)
        {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "stream model differs from resolved target",
            ));
        }
        let mut events = Vec::new();
        match self.kind {
            ProviderKind::Ollama => {
                if self.done {
                    return Err(Failure::new(
                        ErrorKind::ProtocolFailure,
                        "duplicate Ollama terminal response",
                    ));
                }
                let message = &value["message"];
                push_text(&mut events, message, "content", false)?;
                push_text(&mut events, message, "thinking", true)?;
                if message
                    .get("tool_calls")
                    .is_some_and(|v| !v.is_null() && !v.is_array())
                {
                    return Err(Failure::new(
                        ErrorKind::ProtocolFailure,
                        "invalid native tool-call shape",
                    ));
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    if calls.len() > 1 {
                        return Err(Failure::new(
                            ErrorKind::UnsupportedCapability,
                            "parallel tools unsupported; no tool executed",
                        ));
                    }
                    for (fallback, call) in calls.iter().enumerate() {
                        let index = call["function"]["index"]
                            .as_u64()
                            .unwrap_or(fallback as u64)
                            as usize;
                        let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                        let arguments =
                            serde_json::to_string(&call["function"]["arguments"]).unwrap();
                        self.add_call(index, None, &name, &arguments)?;
                        events.push(StreamEvent::ToolCallDelta {
                            index,
                            id: None,
                            name,
                            arguments,
                        });
                    }
                }
                if value["done"] == true {
                    self.done = true;
                    self.ended = true;
                    let native_reason = value["done_reason"].as_str().unwrap_or("stop");
                    self.reason = Some(if !self.calls.is_empty() && native_reason == "stop" {
                        "tool_calls".into()
                    } else {
                        native_reason.into()
                    });
                    events.push(StreamEvent::UsageUpdate {usage:Usage {prompt_tokens:usage_count(&value["prompt_eval_count"])? ,output_tokens:usage_count(&value["eval_count"])? ,cached_tokens:usage_count(&value["prompt_eval_cached_count"])? ,native_timings_ns:json!({"total_duration":value["total_duration"],"load_duration":value["load_duration"],"prompt_eval_duration":value["prompt_eval_duration"],"eval_duration":value["eval_duration"]})}});
                }
            }
            ProviderKind::OpenaiLocal => {
                if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
                    if !usage.is_object() {
                        return Err(Failure::new(
                            ErrorKind::ProtocolFailure,
                            "invalid usage shape",
                        ));
                    }
                    events.push(StreamEvent::UsageUpdate {
                        usage: Usage {
                            prompt_tokens: usage_count(&usage["prompt_tokens"])?,
                            output_tokens: usage_count(&usage["completion_tokens"])?,
                            cached_tokens: usage_count(
                                &usage["prompt_tokens_details"]["cached_tokens"],
                            )?,
                            native_timings_ns: Value::Null,
                        },
                    });
                }
                let choices = value["choices"].as_array().ok_or_else(|| {
                    Failure::new(ErrorKind::ProtocolFailure, "missing SSE choices")
                })?;
                if choices.len() > 1 {
                    return Err(Failure::new(
                        ErrorKind::UnsupportedCapability,
                        "multiple completion choices unsupported",
                    ));
                }
                if let Some(choice) = choices.first() {
                    if self.done || choice["index"].as_u64() != Some(0) {
                        return Err(Failure::new(
                            ErrorKind::ProtocolFailure,
                            "unexpected choice/duplicate finish",
                        ));
                    }
                    let delta = &choice["delta"];
                    push_text(&mut events, delta, "content", false)?;
                    push_text(&mut events, delta, "reasoning_content", true)?;
                    if delta.get("reasoning_content").is_none() {
                        push_text(&mut events, delta, "reasoning", true)?;
                    }
                    if delta
                        .get("tool_calls")
                        .is_some_and(|v| !v.is_null() && !v.is_array())
                    {
                        return Err(Failure::new(
                            ErrorKind::ProtocolFailure,
                            "invalid compatible tool-call shape",
                        ));
                    }
                    if let Some(calls) = delta["tool_calls"].as_array() {
                        for call in calls {
                            let index = call["index"].as_u64().ok_or_else(|| {
                                Failure::new(ErrorKind::ProtocolFailure, "tool delta index missing")
                            })? as usize;
                            let id = call["id"].as_str().map(str::to_owned);
                            let name = call["function"]["name"].as_str().unwrap_or("").to_owned();
                            let arguments = call["function"]["arguments"]
                                .as_str()
                                .unwrap_or("")
                                .to_owned();
                            self.add_call(index, id.as_deref(), &name, &arguments)?;
                            events.push(StreamEvent::ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments,
                            });
                        }
                    }
                    if let Some(reason) = choice["finish_reason"].as_str() {
                        self.done = true;
                        self.reason = Some(reason.into());
                    }
                }
            }
        }
        Ok(events)
    }
    fn add_call(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: &str,
        arguments: &str,
    ) -> Outcome<()> {
        if index != 0 {
            return Err(Failure::new(
                ErrorKind::UnsupportedCapability,
                "parallel tools unsupported; no tool executed",
            ));
        }
        let call = self.calls.entry(index).or_default();
        if let Some(id) = id {
            if id.is_empty() || id.len() > 128 || call.id.as_ref().is_some_and(|v| v != id) {
                return Err(Failure::new(
                    ErrorKind::ProtocolFailure,
                    "invalid/changing tool id",
                ));
            }
            call.id = Some(id.into());
        }
        call.name.push_str(name);
        call.arguments.push_str(arguments);
        if call.name.len() > 128 || call.arguments.len() > 16384 {
            return Err(Failure::new(
                ErrorKind::ResourceExhaustion,
                "tool delta exceeds bounds",
            ));
        }
        Ok(())
    }
    pub fn finish(self) -> Outcome<(Vec<ToolCall>, String)> {
        if !self.done || !self.ended {
            return Err(Failure::new(
                ErrorKind::ProtocolFailure,
                "stream ended without a complete terminal frame",
            ));
        }
        let reason = self.reason.unwrap();
        if !matches!(reason.as_str(), "stop" | "tool_calls") {
            // M10.1 Phase 4: a trustworthy provider-reported finish reason is
            // preserved verbatim, not collapsed into one generic message - an
            // operator reading `task.error`/a round's generation failure
            // needs to be able to tell "the model ran out of output budget"
            // apart from "the provider's safety filter cut this off" apart
            // from "some other provider outcome this adapter doesn't model."
            // Never widens `max_output_tokens` or retries here - that stays
            // the operator's call, which is exactly why the recommendation
            // text below names it instead of acting on it.
            let diagnostic = match reason.as_str() {
                "length" => "output token limit reached: the provider stopped generation after \
                     exhausting the configured max_output_tokens before producing a complete \
                     response; this is a truncated, unusable completion. Increase \
                     max_output_tokens for this task or target, or split the task into smaller \
                     steps - this orchestrator never raises the budget or retries \
                     automatically."
                    .to_string(),
                "content_filter" => {
                    "provider content filter: the provider's safety/content filter stopped \
                     generation before it completed a usable response."
                        .to_string()
                }
                other => format!(
                    "unsupported completion outcome: provider reported finish reason \
                     {other:?}, which is neither a complete answer (\"stop\") nor a tool \
                     call (\"tool_calls\")."
                ),
            };
            return Err(Failure::new(ErrorKind::ProtocolFailure, &diagnostic));
        }
        let mut calls = Vec::new();
        for (index, part) in self.calls {
            let arguments: Value = serde_json::from_str(&part.arguments).map_err(|_| {
                Failure::new(
                    ErrorKind::ProtocolFailure,
                    "tool arguments are not complete JSON",
                )
            })?;
            checked_add(&part.name, &arguments).map_err(|_| {
                Failure::new(
                    ErrorKind::InvalidRequest,
                    "requested tool failed allowed schema/arithmetic validation",
                )
            })?;
            let id = match (part.id, self.kind) {
                (Some(id), _) => id,
                (None, ProviderKind::Ollama) => format!("{}:{index}", self.generation),
                _ => {
                    return Err(Failure::new(
                        ErrorKind::ProtocolFailure,
                        "provider tool call id missing",
                    ));
                }
            };
            calls.push(ToolCall {
                id,
                name: part.name,
                arguments,
            });
        }
        Ok((calls, reason))
    }
}
fn usage_count(value: &Value) -> Outcome<Option<u64>> {
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| {
        Failure::new(
            ErrorKind::ProtocolFailure,
            "usage count must be a nonnegative integer or unknown",
        )
    })
}

fn push_text(
    events: &mut Vec<StreamEvent>,
    message: &Value,
    key: &str,
    reasoning: bool,
) -> Outcome<()> {
    if let Some(value) = message.get(key).filter(|v| !v.is_null()) {
        let text = value.as_str().ok_or_else(|| {
            Failure::new(ErrorKind::ProtocolFailure, "non-text delta unsupported")
        })?;
        if !text.is_empty() {
            events.push(if reasoning {
                StreamEvent::ReasoningDelta { text: text.into() }
            } else {
                StreamEvent::TextDelta { text: text.into() }
            });
        }
    }
    Ok(())
}
