use super::types::*;
use crate::{digest, now};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    pub provider: ProviderKind,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "EndpointAccess::is_loopback")]
    pub endpoint_access: EndpointAccess,
    pub model: String,
    pub expected_artifact_digest: Option<String>,
    pub context_limit: u32,
    pub output_limit: u32,
    pub capabilities: Capabilities,
    pub capability_source: String,
    pub priority: u32,
    pub resource_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<AdapterExtensions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_pricing: Option<crate::telemetry::ApiPricing>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAccess {
    #[default]
    Loopback,
    PrivateIpv4,
}
impl EndpointAccess {
    fn is_loopback(&self) -> bool {
        *self == Self::Loopback
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterExtensions {
    #[serde(rename = "ollama.openai")]
    pub ollama_openai: OllamaOpenaiOptions,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OllamaOpenaiOptions {
    pub reasoning_effort: String,
}
impl Target {
    pub fn fingerprint(&self) -> String {
        digest(&serde_json::to_vec(self).unwrap())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 128
                && !self.model.is_empty()
                && self.model.len() <= 256,
            "invalid target identity"
        );
        self.endpoint_url()?;
        if let Some(pricing) = &self.api_pricing {
            ensure!(
                self.provider == ProviderKind::OpenaiLocal,
                "API pricing requires an explicitly billed OpenAI-compatible gateway"
            );
            pricing.validate()?;
        }
        if let Some(extension) = &self.extensions {
            ensure!(
                self.provider == ProviderKind::OpenaiLocal
                    && extension.ollama_openai.reasoning_effort == "none",
                "only the explicit ollama.openai reasoning_effort=none extension is supported"
            );
        }
        ensure!(
            (256..=262144).contains(&self.context_limit) && (1..=4096).contains(&self.output_limit),
            "invalid target limits"
        );
        ensure!(
            !self.resource_id.is_empty()
                && self.resource_id.len() <= 128
                && !self.capability_source.is_empty()
                && self.capability_source.len() <= 512,
            "resource/capability provenance missing"
        );
        ensure!(
            self.expected_artifact_digest
                .as_ref()
                .is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid artifact digest"
        );
        ensure!(
            self.capabilities.streaming
                && !self.capabilities.media
                && !self.capabilities.structured_output
                && !self.capabilities.parallel_tools,
            "direct v1 supports text streaming and serial tools only"
        );
        ensure!(
            !self.capabilities.required_tool_choice || self.capabilities.tools,
            "required tool-choice capability requires tools"
        );
        ensure!(
            self.provider != ProviderKind::Ollama || !self.capabilities.required_tool_choice,
            "Ollama adapter has no forced tool-choice contract"
        );
        ensure!(
            self.provider != ProviderKind::OpenaiLocal || !self.capabilities.reasoning_control,
            "OpenAI-local reasoning control has no portable contract"
        );
        Ok(())
    }
    pub fn endpoint_url(&self) -> Result<reqwest::Url> {
        endpoint_url(&self.endpoint, self.endpoint_access)
    }
    pub fn eligible(
        &self,
        request: &Request,
        tested: Option<&TestedCapabilities>,
    ) -> std::result::Result<(), Failure> {
        if request.max_output_tokens > self.output_limit {
            return Err(Failure::new(
                ErrorKind::UnsupportedCapability,
                "requested output limit exceeds target limit",
            ));
        }
        if request.estimated_tokens() + request.max_output_tokens as u64 > self.context_limit as u64
        {
            return Err(Failure::new(
                ErrorKind::ContextOverflow,
                "estimated context plus reserved output exceeds limit; nothing was dropped",
            ));
        }
        let caps = if request.require_tested {
            &tested
                .ok_or_else(|| {
                    Failure::new(
                        ErrorKind::UnsupportedCapability,
                        "tested capabilities unavailable or stale",
                    )
                })?
                .capabilities
        } else {
            &self.capabilities
        };
        if !caps.streaming
            || (request.tools != ToolPolicy::Disabled && !caps.tools)
            || (request.require_tool && !caps.required_tool_choice)
            || (request.reasoning.is_some() && !caps.reasoning_control)
        {
            return Err(Failure::new(
                ErrorKind::UnsupportedCapability,
                "target lacks a required capability; preferences are not silently ignored",
            ));
        }
        Ok(())
    }
}

pub fn local_url(value: &str) -> Result<reqwest::Url> {
    endpoint_url(value, EndpointAccess::Loopback)
}

fn endpoint_url(value: &str, access: EndpointAccess) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/'),
        "local endpoint must be credential-free HTTP base URL ending in / without query/fragment"
    );
    let allowed = url.host_str().is_some_and(|host| {
        if host == "localhost" {
            return true;
        }
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| {
                ip.is_loopback()
                    || (access == EndpointAccess::PrivateIpv4
                        && matches!(ip, std::net::IpAddr::V4(ip) if ip.is_private()
                            || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))))
            })
    });
    ensure!(
        allowed,
        "endpoint requires loopback or explicit private_ipv4 access to an RFC1918/Tailscale IPv4 literal"
    );
    Ok(url)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub default_target: String,
    pub hardware_id: String,
    pub admission_db: PathBuf,
    pub max_active_per_resource: u32,
    pub registry_max_age_ms: u64,
    pub fallback_before_dispatch: bool,
    pub targets: Vec<Target>,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut value: Self = read_json(path)?;
        if !value.admission_db.is_absolute() {
            value.admission_db = std::fs::canonicalize(path)?
                .parent()
                .unwrap()
                .join(&value.admission_db);
        }
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == VERSION, "unsupported config version");
        ensure!(
            !self.hardware_id.is_empty()
                && self.hardware_id.len() <= 256
                && self.max_active_per_resource == 1,
            "direct v1 requires hardware provenance and one admission slot per resource"
        );
        ensure!(
            (1000..=604800000).contains(&self.registry_max_age_ms),
            "registry age must be 1 second..7 days"
        );
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 16,
            "invalid target count"
        );
        let mut ids = BTreeSet::new();
        for target in &self.targets {
            target.validate()?;
            ensure!(ids.insert(&target.id), "duplicate target id");
        }
        ensure!(ids.contains(&self.default_target), "default target missing");
        Ok(())
    }
    pub fn target(&self, id: &str) -> Result<&Target> {
        self.targets
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown target override: {id}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestedCapabilities {
    pub capabilities: Capabilities,
    pub tested_at: u64,
    pub runtime_version: String,
    pub artifact_digest: String,
    pub evidence: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub hardware_id: String,
    pub workload: String,
    pub request_digest: String,
    pub measured_at: u64,
    pub latency_ms: Option<f64>,
    pub evidence: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRecord {
    pub target_id: String,
    pub configuration_digest: String,
    pub tested: TestedCapabilities,
    pub measurement: Option<Measurement>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub records: Vec<RegistryRecord>,
}
impl Registry {
    pub fn import(path: &Path, config: &Config) -> Result<Self> {
        let registry: Self = read_json(path)?;
        registry.validate(config)?;
        Ok(registry)
    }
    pub fn validate(&self, config: &Config) -> Result<()> {
        ensure!(
            self.schema_version == VERSION && self.records.len() <= 64,
            "unsupported/oversized registry"
        );
        let mut ids = BTreeSet::new();
        for record in &self.records {
            let target = config.target(&record.target_id)?;
            ensure!(
                ids.insert(&record.target_id)
                    && record.configuration_digest == target.fingerprint(),
                "duplicate or configuration-mismatched registry target"
            );
            let tested = &record.tested;
            ensure!(
                tested.tested_at <= now()
                    && !tested.runtime_version.is_empty()
                    && tested.runtime_version.len() <= 256
                    && !tested.evidence.is_empty()
                    && tested.evidence.len() <= 1024,
                "invalid tested capability provenance"
            );
            ensure!(
                tested.artifact_digest.len() == 64
                    && tested
                        .artifact_digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit()),
                "registry requires exact tested artifact digest"
            );
            let configured = serde_json::to_value(&target.capabilities)?;
            let observed = serde_json::to_value(&tested.capabilities)?;
            ensure!(
                observed
                    .as_object()
                    .unwrap()
                    .iter()
                    .all(|(key, val)| val != &serde_json::Value::Bool(true)
                        || configured[key] == true),
                "registry cannot expand configured capabilities"
            );
            if let Some(m) = &record.measurement {
                ensure!(
                    m.hardware_id == config.hardware_id
                        && matches!(m.workload.as_str(), "text-v1" | "checked-add-v1")
                        && m.request_digest.len() == 64
                        && m.request_digest.bytes().all(|b| b.is_ascii_hexdigit())
                        && m.measured_at <= now()
                        && !m.evidence.is_empty()
                        && m.evidence.len() <= 1024
                        && m.latency_ms.is_none_or(|v| v.is_finite() && v > 0.0),
                    "invalid measurement/provenance; unknown latency must be null"
                );
            }
        }
        Ok(())
    }
    pub fn current<'a>(&'a self, target: &Target, config: &Config) -> Option<&'a RegistryRecord> {
        self.records.iter().find(|r| {
            r.target_id == target.id
                && r.configuration_digest == target.fingerprint()
                && now().saturating_sub(r.tested.tested_at) <= config.registry_max_age_ms
        })
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?.take(1048577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1048576, "JSON document exceeds 1 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}
