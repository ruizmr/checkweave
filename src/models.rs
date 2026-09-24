//! Model provider boundary: local Python worker lifecycle and explicit Jev HTTP.
//!
//! The kernel calls [`ModelProvider`]; this module does not edit shared types.
//! Stdout of the local worker is newline-delimited JSON, version 1, max 8 MiB.
//! Stderr is diagnostics only. Local failure never opens a hosted provider.

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex,
    task::JoinHandle,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const ADAPTER_VERSION: &str = "checkweave-model-v1";
pub const GLINER_ADAPTER_VERSION: &str = "checkweave-gliner2-1";
pub const SEMIF_ADAPTER_VERSION: &str = "checkweave-semif-1";
/// Lightweight profile. Not the local default.
pub const LOCAL_MODEL_ID: &str = "fastino/gliner2.5-base-v1";
pub const LOCAL_MODEL_REVISION: &str = "1a8bc24e00dc7300b9017c81d63e3dcdabb26596";
pub const SEMIF_MODEL_ID: &str = "Qwen/Qwen3.5-4B";
pub const SEMIF_MODEL_REVISION: &str = "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a";
pub const SEMIF_CODE_REVISION: &str = "1f2dea3e25379f9dfc98cb83c324f00ab5deda37";
pub const SEMIF_GGUF_REPO: &str = "bartowski/Qwen_Qwen3.5-4B-GGUF";
pub const SEMIF_GGUF_REVISION: &str = "4168f45a16a1290d65a4ec0fa312ae917a4c15d6";
pub const SEMIF_GGUF_FILE: &str = "Qwen_Qwen3.5-4B-Q4_K_M.gguf";
pub const SEMIF_GGUF_BYTES: u64 = 3_013_027_808;
pub const JEV_MODEL_ID: &str = "jev-1.13.0";
pub const JEV_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_SOURCE_FINGERPRINT_MATERIAL: &str = "checkweave.model.source.v1\ndefault-local";

const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_ENV_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const DEFAULT_INSTALL_TIMEOUT_MS: u64 = 15 * 60 * 1000;
const JEV_STATE_CHAR_BUDGET: usize = 32_000;
const JEV_TOTAL_CHAR_BUDGET: usize = 64_000;
const RETRY_STATUSES: [u16; 3] = [429, 503, 529];

fn default_device() -> String {
    "auto".into()
}
fn default_threads() -> u32 {
    4
}
fn default_max_input_tokens() -> u32 {
    4096
}
fn default_idle_timeout_ms() -> u64 {
    600_000
}
fn default_startup_timeout_ms() -> u64 {
    600_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelProviderKind {
    #[default]
    Local,
    Jev,
}

/// Local weight profile. Omitted config selects [`LocalProfile::Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocalProfile {
    /// SemIf option-logit on pinned Qwen3.5-4B. BF16 when one GPU can hold it; otherwise pinned Q4 GGUF on llama.cpp.
    Default,
    /// Optional GLiNER2.5 base. Predicates are unsupported on this profile.
    Lightweight,
}

/// `[model]` table from `checkweave.toml`. Invalid combinations are errors.
/// Credentials are an environment variable name, never a stored secret.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default)]
    pub provider: ModelProviderKind,
    /// `default` (SemIf) or `lightweight` (GLiNER2.5). Absent until normalization.
    #[serde(default)]
    pub profile: Option<LocalProfile>,
    #[serde(default)]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default = "default_threads")]
    pub threads: u32,
    #[serde(default)]
    pub model: Option<String>,
    /// Name of the environment variable holding the Jev bearer token.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Explicit endpoint override. Production Jev uses [`JEV_ENDPOINT`].
    /// Intended for a local mock server; local providers reject this field.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: u32,
    /// Idle time before the managed local worker is reaped. `0` disables the timer.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default)]
    pub offline: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: ModelProviderKind::Local,
            profile: Some(LocalProfile::Default),
            checkpoint: Some(SEMIF_MODEL_ID.into()),
            revision: Some(SEMIF_MODEL_REVISION.into()),
            device: default_device(),
            threads: default_threads(),
            model: None,
            api_key_env: None,
            endpoint: None,
            max_input_tokens: default_max_input_tokens(),
            idle_timeout_ms: default_idle_timeout_ms(),
            startup_timeout_ms: default_startup_timeout_ms(),
            offline: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChoiceLabel {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrdinalLevel {
    pub label: String,
    pub description: String,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelQuestion {
    Choice {
        id: String,
        labels: Vec<ChoiceLabel>,
    },
    Ordinal {
        id: String,
        levels: Vec<OrdinalLevel>,
    },
    Predicate {
        id: String,
        statement: String,
    },
}

impl ModelQuestion {
    pub fn id(&self) -> &str {
        match self {
            Self::Choice { id, .. } | Self::Ordinal { id, .. } | Self::Predicate { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelState {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Resolved,
    Unresolved,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelResult {
    pub state_id: String,
    pub question_id: String,
    pub status: ModelStatus,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    /// Number for ordinal expected level and Jev yes-probability. Boolean for a SemIf predicate.
    pub value: Option<Value>,
    #[serde(default)]
    pub scores: Option<BTreeMap<String, f64>>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Provider-native numeric score when it is not `value` (Jev expected index).
    #[serde(default)]
    pub provider_value: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelProvenance {
    pub provider: String,
    pub model: String,
    pub revision: String,
    pub adapter_version: String,
    pub device: String,
    pub precision: String,
    /// String for older mocks, or the worker's object (`choice`, `ordinal`, …).
    pub score_semantics: Value,
    pub input_policy: String,
    #[serde(default)]
    pub runtime_versions: BTreeMap<String, Value>,
    #[serde(default)]
    pub usage: Option<ModelUsage>,
    #[serde(default)]
    pub device_requested: Option<String>,
    #[serde(default)]
    pub device_fallback: Option<String>,
    #[serde(default)]
    pub accelerator: Option<String>,
    #[serde(default)]
    pub parameter_device: Option<String>,
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default)]
    pub threads: Option<u32>,
    #[serde(default)]
    pub offline: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub gguf_sha256: Option<String>,
    #[serde(default)]
    pub code_revision: Option<String>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelResponse {
    pub id: String,
    pub results: Vec<ModelResult>,
    pub provenance: ModelProvenance,
}

/// Declared settings that affect reuse. Actual device and runtime versions
/// come from [`ModelProvenance`] after a call and belong in [`result_cache_key`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelSettingsIdentity {
    pub adapter_version: String,
    pub api_key_env: Option<String>,
    pub device: String,
    pub code_revision: Option<String>,
    pub endpoint_origin: Option<String>,
    pub gguf_file: Option<String>,
    pub gguf_repo: Option<String>,
    pub gguf_revision: Option<String>,
    pub input_policy: String,
    pub max_input_tokens: u32,
    pub model: String,
    pub profile: Option<String>,
    pub offline: bool,
    pub precision: String,
    pub protocol_version: u32,
    pub provider: String,
    pub revision: String,
    pub score_semantics: String,
    pub threads: u32,
}

#[derive(Debug, Clone)]
pub struct LoadedModelConfig {
    pub config: ModelConfig,
    pub source_fingerprint: String,
    pub settings_fingerprint: String,
}

pub fn load_model_config(root: &Path) -> Result<LoadedModelConfig> {
    ensure!(
        root.is_dir(),
        "workspace root is not a directory: {}",
        root.display()
    );
    let path = root.join("checkweave.toml");
    if !path.exists() {
        let config = ModelConfig::default();
        return Ok(finish_load(config, default_source_fingerprint()));
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_workspace_toml(&text)
}

pub fn parse_workspace_toml(text: &str) -> Result<LoadedModelConfig> {
    let root_value: toml::Value = toml::from_str(text).context("parse checkweave.toml")?;
    let Some(model_value) = root_value.get("model") else {
        return Ok(finish_load(
            ModelConfig::default(),
            default_source_fingerprint(),
        ));
    };
    let source_fingerprint = fingerprint_bytes(canonical_toml_json(model_value)?.as_bytes());
    let parsed = ModelConfig::deserialize(model_value.clone())
        .map_err(|err| anyhow::anyhow!("invalid [model]: {err}"))?;
    Ok(finish_load(parsed.normalized()?, source_fingerprint))
}

fn finish_load(config: ModelConfig, source_fingerprint: String) -> LoadedModelConfig {
    let settings_fingerprint = settings_fingerprint(&config);
    LoadedModelConfig {
        config,
        source_fingerprint,
        settings_fingerprint,
    }
}

impl ModelConfig {
    pub fn normalized(self) -> Result<Self> {
        validate_device(&self.device)?;
        ensure!((1..=256).contains(&self.threads), "threads must be 1..=256");
        ensure!(
            (1..=100_000).contains(&self.max_input_tokens),
            "max_input_tokens must be 1..=100000"
        );
        ensure!(
            self.idle_timeout_ms <= 86_400_000,
            "idle_timeout_ms exceeds 24h"
        );
        ensure!(
            (1..=1_800_000).contains(&self.startup_timeout_ms),
            "startup_timeout_ms must be 1..=1800000"
        );
        match self.provider {
            ModelProviderKind::Local => self.normalize_local(),
            ModelProviderKind::Jev => self.normalize_jev(),
        }
    }

    fn normalize_local(mut self) -> Result<Self> {
        ensure!(
            self.endpoint.is_none(),
            "local provider rejects endpoint; it never routes to a hosted API"
        );
        ensure!(
            self.api_key_env.is_none(),
            "local provider does not take api_key_env"
        );
        ensure!(
            self.model.is_none(),
            "local provider uses checkpoint and revision, not model"
        );
        let profile = resolve_local_profile(self.profile, self.checkpoint.as_deref())?;
        let (model, revision) = match profile {
            LocalProfile::Default => (SEMIF_MODEL_ID, SEMIF_MODEL_REVISION),
            LocalProfile::Lightweight => (LOCAL_MODEL_ID, LOCAL_MODEL_REVISION),
        };
        if let Some(id) = self.checkpoint.as_deref() {
            ensure!(
                id == model,
                "checkpoint {id} does not match profile {}",
                profile_name(profile)
            );
        }
        if let Some(rev) = self.revision.as_deref() {
            ensure!(
                rev == revision,
                "revision {rev} does not match profile {}",
                profile_name(profile)
            );
        }
        self.profile = Some(profile);
        self.checkpoint = Some(model.into());
        self.revision = Some(revision.into());
        Ok(self)
    }

    pub fn local_profile(&self) -> Option<LocalProfile> {
        self.profile
    }

    fn normalize_jev(mut self) -> Result<Self> {
        ensure!(
            self.checkpoint.is_none(),
            "jev provider does not take a local checkpoint"
        );
        ensure!(
            self.revision.is_none(),
            "jev provider pins the concrete model id, not a local revision"
        );
        let key_env = self
            .api_key_env
            .clone()
            .context("jev provider requires api_key_env")?;
        ensure!(
            is_env_name(&key_env),
            "api_key_env must be an environment variable name"
        );
        self.api_key_env = Some(key_env);
        match self.model.as_deref() {
            None => self.model = Some(JEV_MODEL_ID.into()),
            Some(model) if is_concrete_jev_model(model) => {}
            Some(model) => bail!("jev model {model} is not a pinned jev-MAJOR.MINOR.PATCH id"),
        }
        if self.endpoint.is_none() {
            self.endpoint = Some(JEV_ENDPOINT.into());
        }
        let endpoint = self.endpoint.clone().unwrap_or_default();
        validate_endpoint(&endpoint)?;
        self.endpoint = Some(endpoint);
        Ok(self)
    }
}

pub fn settings_identity(config: &ModelConfig) -> ModelSettingsIdentity {
    match config.provider {
        ModelProviderKind::Local => {
            let profile = config.profile.unwrap_or(LocalProfile::Default);
            let (
                model,
                revision,
                adapter,
                precision,
                policy,
                semantics,
                code,
                gguf_repo,
                gguf_rev,
                gguf_file,
            ) = match profile {
                LocalProfile::Default => (
                    SEMIF_MODEL_ID,
                    SEMIF_MODEL_REVISION,
                    SEMIF_ADAPTER_VERSION,
                    "bf16_or_q4_k_m",
                    SEMIF_INPUT_POLICY,
                    "semif_option_logit_softmax",
                    Some(SEMIF_CODE_REVISION.to_string()),
                    Some(SEMIF_GGUF_REPO.to_string()),
                    Some(SEMIF_GGUF_REVISION.to_string()),
                    Some(SEMIF_GGUF_FILE.to_string()),
                ),
                LocalProfile::Lightweight => (
                    LOCAL_MODEL_ID,
                    LOCAL_MODEL_REVISION,
                    GLINER_ADAPTER_VERSION,
                    "float32",
                    GLINER_INPUT_POLICY,
                    "exclusive_softmax",
                    None,
                    None,
                    None,
                    None,
                ),
            };
            ModelSettingsIdentity {
                adapter_version: adapter.into(),
                api_key_env: None,
                code_revision: code,
                device: config.device.clone(),
                endpoint_origin: None,
                gguf_file,
                gguf_repo,
                gguf_revision: gguf_rev,
                input_policy: policy.into(),
                max_input_tokens: config.max_input_tokens,
                model: model.into(),
                offline: config.offline,
                precision: precision.into(),
                profile: Some(profile_name(profile).into()),
                protocol_version: PROTOCOL_VERSION,
                provider: "local".into(),
                revision: revision.into(),
                score_semantics: semantics.into(),
                threads: config.threads,
            }
        }
        ModelProviderKind::Jev => ModelSettingsIdentity {
            adapter_version: ADAPTER_VERSION.into(),
            api_key_env: config.api_key_env.clone(),
            code_revision: None,
            device: "remote".into(),
            endpoint_origin: config.endpoint.as_deref().map(endpoint_origin),
            gguf_file: None,
            gguf_repo: None,
            gguf_revision: None,
            input_policy: JEV_INPUT_POLICY.into(),
            max_input_tokens: config.max_input_tokens,
            model: config.model.clone().unwrap_or_else(|| JEV_MODEL_ID.into()),
            offline: config.offline,
            precision: "provider-managed".into(),
            profile: None,
            protocol_version: PROTOCOL_VERSION,
            provider: "jev".into(),
            revision: config.model.clone().unwrap_or_else(|| JEV_MODEL_ID.into()),
            score_semantics:
                "jev_noul_bernoulli_yes;choice_exclusive_probabilities;score_expected_index".into(),
            threads: 0,
        },
    }
}

pub fn settings_fingerprint(config: &ModelConfig) -> String {
    let identity = settings_identity(config);
    let value = serde_json::to_value(identity).unwrap_or(Value::Null);
    fingerprint_bytes(canonical_json(&value).as_bytes())
}

pub fn result_cache_key(settings_fingerprint: &str, provenance: &ModelProvenance) -> String {
    let mut map = Map::new();
    map.insert(
        "accelerator".into(),
        serde_json::to_value(&provenance.accelerator).unwrap_or(Value::Null),
    );
    map.insert(
        "adapter_version".into(),
        json_string(&provenance.adapter_version),
    );
    map.insert(
        "backend".into(),
        serde_json::to_value(&provenance.backend).unwrap_or(Value::Null),
    );
    map.insert(
        "code_revision".into(),
        serde_json::to_value(&provenance.code_revision).unwrap_or(Value::Null),
    );
    map.insert("device".into(), json_string(&provenance.device));
    map.insert(
        "device_fallback".into(),
        serde_json::to_value(&provenance.device_fallback).unwrap_or(Value::Null),
    );
    map.insert(
        "device_requested".into(),
        serde_json::to_value(&provenance.device_requested).unwrap_or(Value::Null),
    );
    map.insert(
        "gguf_sha256".into(),
        serde_json::to_value(&provenance.gguf_sha256).unwrap_or(Value::Null),
    );
    map.insert("input_policy".into(), json_string(&provenance.input_policy));
    map.insert("model".into(), json_string(&provenance.model));
    map.insert("precision".into(), json_string(&provenance.precision));
    map.insert(
        "profile".into(),
        serde_json::to_value(&provenance.profile).unwrap_or(Value::Null),
    );
    map.insert("provider".into(), json_string(&provenance.provider));
    map.insert("revision".into(), json_string(&provenance.revision));
    map.insert(
        "runtime_versions".into(),
        serde_json::to_value(&provenance.runtime_versions).unwrap_or(Value::Null),
    );
    map.insert("score_semantics".into(), provenance.score_semantics.clone());
    map.insert(
        "settings_fingerprint".into(),
        json_string(settings_fingerprint),
    );
    map.insert(
        "extra".into(),
        serde_json::to_value(&provenance.extra).unwrap_or(Value::Null),
    );
    map.insert(
        "parameter_device".into(),
        serde_json::to_value(&provenance.parameter_device).unwrap_or(Value::Null),
    );
    map.insert(
        "input_device".into(),
        serde_json::to_value(&provenance.input_device).unwrap_or(Value::Null),
    );
    map.insert(
        "threads".into(),
        serde_json::to_value(provenance.threads).unwrap_or(Value::Null),
    );
    map.insert(
        "offline".into(),
        serde_json::to_value(provenance.offline).unwrap_or(Value::Null),
    );
    map.insert(
        "language".into(),
        serde_json::to_value(&provenance.language).unwrap_or(Value::Null),
    );
    fingerprint_bytes(canonical_json(&Value::Object(map)).as_bytes())
}

const GLINER_INPUT_POLICY: &str = "full_serialized_input_including_schema_and_labels; reject_over_limit; no_truncation; omitted_nothing; predicate_unsupported";
const SEMIF_INPUT_POLICY: &str = "semif_option_logit; no_text_sampling; pinned_label_order; reject_over_limit; no_truncation; q4_cpu_quality_unverified";
const JEV_INPUT_POLICY: &str =
    "reject_over_character_budget;text_only;no_low_confidence_abstention;no_silent_truncation";

fn profile_name(profile: LocalProfile) -> &'static str {
    match profile {
        LocalProfile::Default => "default",
        LocalProfile::Lightweight => "lightweight",
    }
}

fn backend_flag(profile: LocalProfile) -> &'static str {
    match profile {
        LocalProfile::Default => "semif",
        LocalProfile::Lightweight => "gliner2",
    }
}

fn resolve_local_profile(
    profile: Option<LocalProfile>,
    checkpoint: Option<&str>,
) -> Result<LocalProfile> {
    match (profile, checkpoint) {
        (None, None)
        | (Some(LocalProfile::Default), None)
        | (None, Some(SEMIF_MODEL_ID))
        | (Some(LocalProfile::Default), Some(SEMIF_MODEL_ID)) => Ok(LocalProfile::Default),
        (None, Some(LOCAL_MODEL_ID))
        | (Some(LocalProfile::Lightweight), None)
        | (Some(LocalProfile::Lightweight), Some(LOCAL_MODEL_ID)) => Ok(LocalProfile::Lightweight),
        (Some(LocalProfile::Default), Some(LOCAL_MODEL_ID)) => bail!(
            "profile default is SemIf/Qwen3.5-4B; checkpoint {LOCAL_MODEL_ID} is the lightweight profile"
        ),
        (Some(LocalProfile::Lightweight), Some(SEMIF_MODEL_ID)) => bail!(
            "profile lightweight is GLiNER2.5; checkpoint {SEMIF_MODEL_ID} is the default SemIf profile"
        ),
        (_, Some(other)) => bail!("unsupported local checkpoint {other}"),
    }
}

fn json_string(value: &str) -> Value {
    Value::String(value.to_string())
}

fn default_source_fingerprint() -> String {
    fingerprint_bytes(DEFAULT_SOURCE_FINGERPRINT_MATERIAL.as_bytes())
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

fn canonical_toml_json(value: &toml::Value) -> Result<String> {
    let json = toml_to_json(value);
    Ok(canonical_json(&json))
}

fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(n) => Value::from(*n),
        toml::Value::Float(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(flag) => Value::Bool(*flag),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            let mut map = Map::new();
            for (key, item) in table {
                map.insert(key.clone(), toml_to_json(item));
            }
            Value::Object(map)
        }
    }
}

fn validate_device(device: &str) -> Result<()> {
    if device == "auto" || device == "cpu" || device == "mps" {
        return Ok(());
    }
    if let Some(rest) = device.strip_prefix("cuda") {
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(index) = rest.strip_prefix(':')
            && !index.is_empty()
            && index.chars().all(|ch| ch.is_ascii_digit())
        {
            return Ok(());
        }
    }
    bail!("device must be auto, cpu, mps, cuda, or cuda:<index>");
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') && name.len() <= 128
}

fn is_concrete_jev_model(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("jev-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !major.is_empty()
        && major.chars().all(|ch| ch.is_ascii_digit())
        && !minor.is_empty()
        && minor.chars().all(|ch| ch.is_ascii_digit())
        && !patch.is_empty()
        && patch.chars().all(|ch| ch.is_ascii_digit())
}

fn validate_endpoint(url: &str) -> Result<()> {
    ensure!(
        url.starts_with("https://") || url.starts_with("http://"),
        "endpoint must be an http(s) URL"
    );
    ensure!(!url.contains('@'), "endpoint must not embed userinfo");
    ensure!(url.len() <= 2048, "endpoint exceeds 2048 bytes");
    ensure!(
        !url.chars().any(|ch| ch.is_control()),
        "endpoint contains a control character"
    );
    Ok(())
}

pub fn endpoint_origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

fn validate_batch(
    states: &[ModelState],
    questions: &[ModelQuestion],
    provider: ModelProviderKind,
) -> Result<()> {
    ensure!(!states.is_empty(), "evaluate requires at least one state");
    ensure!(
        !questions.is_empty(),
        "evaluate requires at least one question"
    );
    ensure!(states.len() <= 256, "at most 256 states per evaluate");
    ensure!(questions.len() <= 64, "at most 64 questions per evaluate");
    let mut state_ids = BTreeSet::new();
    for state in states {
        ensure!(
            valid_id(&state.id),
            "state id must be 1..=256 characters without newlines"
        );
        ensure!(
            state_ids.insert(state.id.clone()),
            "duplicate state id {}",
            state.id
        );
    }
    let mut question_ids = BTreeSet::new();
    for question in questions {
        ensure!(
            valid_id(question.id()),
            "question id must be 1..=256 characters without newlines"
        );
        ensure!(
            question_ids.insert(question.id().to_string()),
            "duplicate question id {}",
            question.id()
        );
        match question {
            ModelQuestion::Choice { labels, .. } => {
                let limit = if provider == ModelProviderKind::Jev {
                    255
                } else {
                    64
                };
                ensure!(
                    !labels.is_empty() && labels.len() <= limit,
                    "choice requires 1..={limit} labels"
                );
                let mut seen = BTreeSet::new();
                for label in labels {
                    ensure!(
                        valid_id(&label.label),
                        "choice label must be a short single-line id"
                    );
                    ensure!(
                        seen.insert(label.label.clone()),
                        "duplicate choice label {}",
                        label.label
                    );
                    ensure!(
                        label.description.len() <= 8_192,
                        "choice description exceeds 8192 bytes"
                    );
                }
            }
            ModelQuestion::Ordinal { levels, .. } => {
                let max = if provider == ModelProviderKind::Jev {
                    10
                } else {
                    32
                };
                ensure!(
                    (2..=max).contains(&levels.len()),
                    "ordinal requires 2..={max} levels"
                );
                let mut seen = BTreeSet::new();
                for level in levels {
                    ensure!(
                        valid_id(&level.label),
                        "ordinal label must be a short single-line id"
                    );
                    ensure!(
                        seen.insert(level.label.clone()),
                        "duplicate ordinal label {}",
                        level.label
                    );
                    ensure!(
                        level.value.is_finite(),
                        "ordinal level value must be finite"
                    );
                    ensure!(
                        level.description.len() <= 8_192,
                        "ordinal description exceeds 8192 bytes"
                    );
                }
            }
            ModelQuestion::Predicate { statement, .. } => {
                ensure!(!statement.is_empty(), "predicate statement is empty");
                ensure!(
                    statement.len() <= 32_768,
                    "predicate statement exceeds 32768 bytes"
                );
                ensure!(
                    !statement.chars().any(|ch| ch == '\0'),
                    "predicate statement contains NUL"
                );
            }
        }
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.chars().any(|ch| ch == '\n' || ch == '\r' || ch == '\0')
}

fn question_chars(question: &ModelQuestion) -> usize {
    match question {
        ModelQuestion::Predicate { id, statement } => {
            id.chars().count() + statement.chars().count()
        }
        ModelQuestion::Choice { id, labels } => {
            id.chars().count()
                + labels
                    .iter()
                    .map(|label| label.label.chars().count() + label.description.chars().count())
                    .sum::<usize>()
        }
        ModelQuestion::Ordinal { id, levels } => {
            id.chars().count()
                + levels
                    .iter()
                    .map(|level| level.label.chars().count() + level.description.chars().count())
                    .sum::<usize>()
        }
    }
}

fn jev_char_budget(state: &ModelState, questions: &[ModelQuestion]) -> Result<()> {
    let state_chars = state.text.chars().count();
    ensure!(
        state_chars <= JEV_STATE_CHAR_BUDGET,
        "state {} exceeds {JEV_STATE_CHAR_BUDGET} characters",
        state.id
    );
    let mut total = state_chars;
    for question in questions {
        let n = question_chars(question);
        ensure!(
            state_chars + n <= JEV_STATE_CHAR_BUDGET,
            "state {} plus question {} exceeds {JEV_STATE_CHAR_BUDGET} characters",
            state.id,
            question.id()
        );
        total = total.saturating_add(n);
    }
    ensure!(
        total <= JEV_TOTAL_CHAR_BUDGET,
        "state {} plus its questions exceed {JEV_TOTAL_CHAR_BUDGET} characters",
        state.id
    );
    Ok(())
}

pub fn redact_sensitive(text: &str, secret: &str) -> String {
    let mut out = if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "[redacted]")
    };
    if let Some(start) = out.find("Bearer ") {
        let tail = &out[start + "Bearer ".len()..];
        let len = tail
            .find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'')
            .unwrap_or(tail.len());
        if len > 0 {
            out.replace_range(
                start + "Bearer ".len()..start + "Bearer ".len() + len,
                "[redacted]",
            );
        }
    }
    out
}

struct RunningWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    stderr_tail: Arc<Mutex<String>>,
    stderr_task: JoinHandle<()>,
    last_used: Instant,
    ready: ModelProvenance,
}

pub struct ModelProvider {
    config: ModelConfig,
    source_fingerprint: String,
    settings_fingerprint: String,
    worker: Arc<Mutex<Option<RunningWorker>>>,
    pid: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
}

impl Drop for ModelProvider {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 {
            kill_group(pid);
        }
        if let Some(handle) = self.watchdog.take() {
            handle.abort();
        }
    }
}

impl ModelProvider {
    pub async fn new(root: &Path) -> Result<Self> {
        let loaded = load_model_config(root)?;
        Self::from_loaded(loaded)
    }

    fn from_loaded(loaded: LoadedModelConfig) -> Result<Self> {
        let idle = Duration::from_millis(loaded.config.idle_timeout_ms);
        let worker = Arc::new(Mutex::new(None));
        let pid = Arc::new(AtomicU32::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let watchdog = if loaded.config.provider == ModelProviderKind::Local
            && loaded.config.idle_timeout_ms > 0
        {
            Some(spawn_watchdog(
                worker.clone(),
                pid.clone(),
                shutdown.clone(),
                idle,
            ))
        } else {
            None
        };
        Ok(Self {
            config: loaded.config,
            source_fingerprint: loaded.source_fingerprint,
            settings_fingerprint: loaded.settings_fingerprint,
            worker,
            pid,
            shutdown,
            watchdog,
        })
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }
    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
    pub fn settings_fingerprint(&self) -> &str {
        &self.settings_fingerprint
    }
    pub fn settings_identity(&self) -> ModelSettingsIdentity {
        settings_identity(&self.config)
    }
    pub fn managed_worker_pid(&self) -> Option<u32> {
        let pid = self.pid.load(Ordering::Acquire);
        (pid > 0).then_some(pid)
    }

    pub async fn evaluate(
        &mut self,
        states: Vec<ModelState>,
        questions: Vec<ModelQuestion>,
        timeout_ms: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<ModelResponse> {
        ensure!(
            (1..=3_600_000).contains(&timeout_ms),
            "timeout_ms must be 1..=3600000"
        );
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled before start");
        }
        validate_batch(&states, &questions, self.config.provider)?;
        match self.config.provider {
            ModelProviderKind::Local => {
                self.evaluate_local(states, questions, timeout_ms, &cancel)
                    .await
            }
            ModelProviderKind::Jev => {
                evaluate_jev(&self.config, states, questions, timeout_ms, &cancel).await
            }
        }
    }

    async fn evaluate_local(
        &mut self,
        states: Vec<ModelState>,
        questions: Vec<ModelQuestion>,
        timeout_ms: u64,
        cancel: &AtomicBool,
    ) -> Result<ModelResponse> {
        let gliner = self.config.profile == Some(LocalProfile::Lightweight);
        let request_id = uuid::Uuid::new_v4().to_string();
        if gliner
            && questions
                .iter()
                .all(|q| matches!(q, ModelQuestion::Predicate { .. }))
        {
            return Ok(ModelResponse {
                id: request_id,
                results: unsupported_predicates(&states, &questions),
                provenance: local_not_invoked_provenance(),
            });
        }
        let sent: Vec<ModelQuestion> = if gliner {
            questions
                .iter()
                .filter(|q| !matches!(q, ModelQuestion::Predicate { .. }))
                .cloned()
                .collect()
        } else {
            questions.clone()
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut crashed = false;
        loop {
            match self
                .local_round(&states, &sent, &request_id, deadline, cancel)
                .await
            {
                Ok(mut response) => {
                    if gliner
                        && questions
                            .iter()
                            .any(|q| matches!(q, ModelQuestion::Predicate { .. }))
                    {
                        response.results =
                            merge_predicate_results(&states, &questions, response.results);
                    }
                    return Ok(response);
                }
                Err(AttemptError::Crash(message)) if !crashed => {
                    crashed = true;
                    self.stop_worker().await;
                    let _ = message;
                    continue;
                }
                Err(AttemptError::Crash(message)) => {
                    self.stop_worker().await;
                    bail!("local worker crashed again: {message}");
                }
                Err(AttemptError::Fatal(error)) => {
                    self.stop_worker().await;
                    return Err(error);
                }
            }
        }
    }

    async fn local_round(
        &self,
        states: &[ModelState],
        questions: &[ModelQuestion],
        request_id: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> std::result::Result<ModelResponse, AttemptError> {
        let payload = serde_json::json!({
            "version": PROTOCOL_VERSION,
            "id": request_id,
            "op": "evaluate",
            "states": states,
            "questions": questions,
            "max_input_tokens": self.config.max_input_tokens,
        });
        let mut bytes =
            serde_json::to_vec(&payload).map_err(|err| AttemptError::Fatal(err.into()))?;
        if bytes.len() + 1 > MAX_FRAME_BYTES {
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "evaluation request exceeds 8MiB frame"
            )));
        }
        bytes.push(b'\n');
        self.ensure_worker(deadline, cancel).await?;
        let mut guard = self.worker.lock().await;
        let running = guard
            .as_mut()
            .ok_or_else(|| AttemptError::Crash("worker missing after start".into()))?;
        if let Err(error) = running.stdin.write_all(&bytes).await {
            return Err(AttemptError::Crash(format!("worker stdin closed: {error}")));
        }
        if let Err(error) = running.stdin.flush().await {
            return Err(AttemptError::Crash(format!(
                "worker stdin flush failed: {error}"
            )));
        }
        let frame = read_frame(&mut running.stdout, deadline, cancel).await?;
        running.last_used = Instant::now();
        let message: WorkerMessage = serde_json::from_slice(&frame).map_err(|err| {
            AttemptError::Fatal(anyhow::anyhow!(
                "worker response is not protocol JSON: {err}"
            ))
        })?;
        if message.version != PROTOCOL_VERSION {
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "worker protocol version {} is not {PROTOCOL_VERSION}",
                message.version
            )));
        }
        if message.id.as_deref() != Some(request_id) {
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "worker response id does not match the request"
            )));
        }
        if let Some(error) = message.error {
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "local worker error: {error}"
            )));
        }
        let results = message.results.unwrap_or_default();
        let mut provenance = message.provenance.ok_or_else(|| {
            AttemptError::Fatal(anyhow::anyhow!("worker response omitted provenance"))
        })?;
        promote_identity_fields(&mut provenance);
        check_local_provenance(&self.config, &provenance).map_err(AttemptError::Fatal)?;
        ensure_worker_coverage(states, questions, &results).map_err(AttemptError::Fatal)?;
        Ok(ModelResponse {
            id: request_id.to_string(),
            results,
            provenance,
        })
    }

    async fn ensure_worker(
        &self,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> std::result::Result<(), AttemptError> {
        if cancel.load(Ordering::Acquire) {
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "evaluation cancelled before start"
            )));
        }
        {
            let guard = self.worker.lock().await;
            if let Some(running) = guard.as_ref() {
                let idle = Duration::from_millis(self.config.idle_timeout_ms);
                let expired = self.config.idle_timeout_ms > 0 && running.last_used.elapsed() > idle;
                if !expired && running.child.id().is_some() {
                    return Ok(());
                }
            }
            if guard.is_some() {
                drop(guard);
                self.stop_worker().await;
            }
        }
        let choice = if std::env::var_os("CHECKWEAVE_PYTHON").is_none() {
            let choice = EnvChoice::resolve(&self.config).map_err(AttemptError::Fatal)?;
            let offline = self.config.offline;
            if let Err(error) =
                install_local_environment(&self.config, offline, cancel, &choice).await
            {
                return Err(AttemptError::Fatal(error));
            }
            choice
        } else {
            materialize_worker(&cache_root().map_err(AttemptError::Fatal)?)
                .map_err(AttemptError::Fatal)?;
            EnvChoice { wheel: None }
        };
        self.spawn_worker(deadline, cancel, &choice).await
    }

    async fn spawn_worker(
        &self,
        deadline: Instant,
        cancel: &AtomicBool,
        choice: &EnvChoice,
    ) -> std::result::Result<(), AttemptError> {
        let python = python_executable(&self.config, choice).map_err(AttemptError::Fatal)?;
        let worker_path = materialize_worker(&cache_root().map_err(AttemptError::Fatal)?)
            .map_err(AttemptError::Fatal)?;
        let mut command = Command::new(&python);
        command
            .arg("-m")
            .arg("checkweave_worker")
            .arg("--backend")
            .arg(backend_flag(
                self.config.profile.unwrap_or(LocalProfile::Default),
            ))
            .arg("--device")
            .arg(&self.config.device)
            .arg("--threads")
            .arg(self.config.threads.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env("PYTHONPATH", &worker_path)
            .env("PYTHONUNBUFFERED", "1")
            .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env(
                "CHECKWEAVE_PROFILE",
                profile_name(self.config.profile.unwrap_or(LocalProfile::Default)),
            )
            .env(
                "CHECKWEAVE_MODEL_ID",
                self.config.checkpoint.clone().unwrap_or_default(),
            )
            .env(
                "CHECKWEAVE_MODEL_REVISION",
                self.config.revision.clone().unwrap_or_default(),
            )
            .env("CHECKWEAVE_CODE_REVISION", SEMIF_CODE_REVISION)
            .env("CHECKWEAVE_GGUF_REPO", SEMIF_GGUF_REPO)
            .env("CHECKWEAVE_GGUF_REVISION", SEMIF_GGUF_REVISION)
            .env("CHECKWEAVE_GGUF_FILE", SEMIF_GGUF_FILE)
            .env(
                "CHECKWEAVE_ADAPTER_VERSION",
                settings_identity(&self.config).adapter_version,
            )
            .env(
                "HF_HUB_CACHE",
                cache_root()
                    .map_err(AttemptError::Fatal)?
                    .join("huggingface"),
            );
        if self.config.offline {
            command.arg("--offline");
            command.env("HF_HUB_OFFLINE", "1");
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|err| AttemptError::Fatal(anyhow::anyhow!("start local worker: {err}")))?;
        let pid = child.id().unwrap_or(0);
        self.pid.store(pid, Ordering::Release);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AttemptError::Fatal(anyhow::anyhow!("worker stdin missing")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AttemptError::Fatal(anyhow::anyhow!("worker stdout missing")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AttemptError::Fatal(anyhow::anyhow!("worker stderr missing")))?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let stderr_task = spawn_stderr_forwarder(stderr, stderr_tail.clone());
        let mut running = RunningWorker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_tail,
            stderr_task,
            last_used: Instant::now(),
            ready: blank_provenance(),
        };
        let startup = Duration::from_millis(self.config.startup_timeout_ms);
        let startup_deadline = Instant::now() + startup;
        let ready_deadline = if startup_deadline < deadline {
            startup_deadline
        } else {
            deadline
        };
        let frame = match read_frame(&mut running.stdout, ready_deadline, cancel).await {
            Ok(frame) => frame,
            Err(error) => {
                let tail = running.stderr_tail.lock().await.clone();
                stop_running(&mut running).await;
                self.pid.store(0, Ordering::Release);
                return Err(annotate_stderr(error, &tail));
            }
        };
        let message: WorkerMessage = match serde_json::from_slice(&frame) {
            Ok(message) => message,
            Err(err) => {
                stop_running(&mut running).await;
                self.pid.store(0, Ordering::Release);
                return Err(AttemptError::Fatal(anyhow::anyhow!(
                    "worker ready frame is not JSON: {err}"
                )));
            }
        };
        if message.version != PROTOCOL_VERSION {
            stop_running(&mut running).await;
            self.pid.store(0, Ordering::Release);
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "worker protocol version {} is not {PROTOCOL_VERSION}",
                message.version
            )));
        }
        match message.ready {
            Some(true) => {}
            Some(false) => {
                let error = message.error.unwrap_or_else(|| "worker not ready".into());
                stop_running(&mut running).await;
                self.pid.store(0, Ordering::Release);
                return Err(AttemptError::Fatal(anyhow::anyhow!(
                    "local worker failed to start: {error}"
                )));
            }
            None => {
                stop_running(&mut running).await;
                self.pid.store(0, Ordering::Release);
                return Err(AttemptError::Fatal(anyhow::anyhow!(
                    "worker did not send a ready message"
                )));
            }
        }
        let mut provenance = match message.provenance {
            Some(provenance) => provenance,
            None => {
                stop_running(&mut running).await;
                self.pid.store(0, Ordering::Release);
                return Err(AttemptError::Fatal(anyhow::anyhow!(
                    "worker ready message omitted provenance"
                )));
            }
        };
        promote_identity_fields(&mut provenance);
        if let Err(error) = check_local_provenance(&self.config, &provenance) {
            stop_running(&mut running).await;
            self.pid.store(0, Ordering::Release);
            return Err(AttemptError::Fatal(error));
        }
        running.ready = provenance;
        let model_dir = cache_root()
            .map_err(AttemptError::Fatal)?
            .join("huggingface");
        if directory_bytes(&model_dir, MAX_MODEL_BYTES).unwrap_or(0) > MAX_MODEL_BYTES {
            stop_running(&mut running).await;
            self.pid.store(0, Ordering::Release);
            return Err(AttemptError::Fatal(anyhow::anyhow!(
                "model cache exceeds 12 GiB"
            )));
        }
        running.last_used = Instant::now();
        *self.worker.lock().await = Some(running);
        Ok(())
    }

    /// Live ready provenance for cache identity. Starts the worker when needed.
    /// Does not send an evaluate request.
    pub async fn readiness(
        &mut self,
        timeout_ms: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<ModelProvenance> {
        ensure!(
            (1..=3_600_000).contains(&timeout_ms),
            "timeout_ms must be 1..=3600000"
        );
        ensure!(
            self.config.provider == ModelProviderKind::Local,
            "readiness is the local worker's ready provenance"
        );
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled before start");
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        self.ensure_worker(deadline, cancel.as_ref())
            .await
            .map_err(|error| match error {
                AttemptError::Crash(message) => anyhow::anyhow!(message),
                AttemptError::Fatal(error) => error,
            })?;
        Ok(self
            .worker
            .lock()
            .await
            .as_ref()
            .context("worker missing after ready")?
            .ready
            .clone())
    }

    async fn stop_worker(&self) {
        self.pid.store(0, Ordering::Release);
        if let Some(mut running) = self.worker.lock().await.take() {
            stop_running(&mut running).await;
        }
    }
}

fn annotate_stderr(error: AttemptError, tail: &str) -> AttemptError {
    if tail.is_empty() {
        return error;
    }
    let clipped = if tail.len() > 2_000 {
        &tail[tail.len() - 2_000..]
    } else {
        tail
    };
    match error {
        AttemptError::Crash(message) => {
            AttemptError::Crash(format!("{message}; stderr: {clipped}"))
        }
        AttemptError::Fatal(error) => {
            AttemptError::Fatal(error.context(format!("worker stderr: {clipped}")))
        }
    }
}

enum AttemptError {
    Crash(String),
    Fatal(anyhow::Error),
}

fn spawn_watchdog(
    worker: Arc<Mutex<Option<RunningWorker>>>,
    pid: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    idle: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let tick = Duration::from_millis(200)
            .max(idle / 4)
            .min(Duration::from_secs(5));
        loop {
            tokio::time::sleep(tick).await;
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let mut guard = worker.lock().await;
            let expired = guard
                .as_ref()
                .is_some_and(|running| running.last_used.elapsed() > idle);
            if expired {
                let current = pid.swap(0, Ordering::AcqRel);
                if current > 0 {
                    kill_group(current);
                }
                if let Some(mut running) = guard.take() {
                    stop_running(&mut running).await;
                }
            }
        }
    })
}

async fn stop_running(running: &mut RunningWorker) {
    if let Some(pid) = running.child.id() {
        kill_group(pid);
    }
    let _ = running.child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), running.child.wait()).await;
    running.stderr_task.abort();
}

fn kill_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn spawn_stderr_forwarder(
    stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<String>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    eprint!("{text}");
                    let mut guard = tail.lock().await;
                    guard.push_str(&text);
                    if guard.len() > MAX_STDERR_BYTES {
                        let extra = guard.len() - MAX_STDERR_BYTES;
                        guard.drain(..extra);
                    }
                }
            }
        }
    })
}

async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    deadline: Instant,
    cancel: &AtomicBool,
) -> std::result::Result<Vec<u8>, AttemptError> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(AttemptError::Fatal(anyhow::anyhow!("evaluation cancelled")));
        }
        if Instant::now() >= deadline {
            return Err(AttemptError::Fatal(anyhow::anyhow!("evaluation timed out")));
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        if slice.is_zero() {
            return Err(AttemptError::Fatal(anyhow::anyhow!("evaluation timed out")));
        }
        match tokio::time::timeout(slice, reader.read(&mut tmp)).await {
            Err(_) => continue,
            Ok(Ok(0)) => return Err(AttemptError::Crash("worker closed stdout".into())),
            Ok(Err(error)) => return Err(AttemptError::Crash(format!("worker stdout: {error}"))),
            Ok(Ok(n)) => {
                for &byte in &tmp[..n] {
                    if byte == b'\n' {
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                        return Ok(buf);
                    }
                    if buf.len() >= MAX_FRAME_BYTES {
                        return Err(AttemptError::Fatal(anyhow::anyhow!(
                            "worker frame exceeds 8MiB"
                        )));
                    }
                    buf.push(byte);
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkerMessage {
    version: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ready: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Option<Vec<ModelResult>>,
    #[serde(default)]
    provenance: Option<ModelProvenance>,
}

fn check_local_provenance(config: &ModelConfig, provenance: &ModelProvenance) -> Result<()> {
    ensure!(
        provenance.provider == "local",
        "local worker reported provider {}",
        provenance.provider
    );
    let profile = config.profile.unwrap_or(LocalProfile::Default);
    let (model, revision) = match profile {
        LocalProfile::Default => (SEMIF_MODEL_ID, SEMIF_MODEL_REVISION),
        LocalProfile::Lightweight => (LOCAL_MODEL_ID, LOCAL_MODEL_REVISION),
    };
    ensure!(
        provenance.model == model,
        "local worker reported model {}",
        provenance.model
    );
    ensure!(
        provenance.revision == revision,
        "local worker reported revision {}",
        provenance.revision
    );
    ensure!(!provenance.device.is_empty(), "local worker omitted device");
    ensure!(
        !provenance.precision.is_empty(),
        "local worker omitted precision"
    );
    ensure!(
        semantics_present(&provenance.score_semantics),
        "local worker omitted score semantics"
    );
    ensure!(
        !provenance.input_policy.is_empty(),
        "local worker omitted input policy"
    );
    match profile {
        LocalProfile::Lightweight => ensure!(
            provenance.adapter_version == GLINER_ADAPTER_VERSION,
            "lightweight worker adapter {} is not {GLINER_ADAPTER_VERSION}",
            provenance.adapter_version
        ),
        LocalProfile::Default => ensure!(
            provenance.adapter_version == SEMIF_ADAPTER_VERSION,
            "default worker adapter {} is not {SEMIF_ADAPTER_VERSION}",
            provenance.adapter_version
        ),
    }
    Ok(())
}

fn promote_identity_fields(provenance: &mut ModelProvenance) {
    if provenance.gguf_sha256.is_none()
        && let Some(sha) = provenance
            .extra
            .get("gguf")
            .and_then(|value| value.get("sha256"))
            .and_then(|value| value.as_str())
    {
        provenance.gguf_sha256 = Some(sha.to_string());
    }
    if provenance.code_revision.is_none()
        && let Some(pin) = provenance
            .extra
            .get("code_pin")
            .and_then(|value| value.as_str())
    {
        provenance.code_revision = Some(pin.to_string());
    }
}

fn semantics_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn blank_provenance() -> ModelProvenance {
    ModelProvenance {
        provider: String::new(),
        model: String::new(),
        revision: String::new(),
        adapter_version: String::new(),
        device: String::new(),
        precision: String::new(),
        score_semantics: Value::Null,
        input_policy: String::new(),
        runtime_versions: BTreeMap::new(),
        usage: None,
        device_requested: None,
        device_fallback: None,
        accelerator: None,
        parameter_device: None,
        input_device: None,
        threads: None,
        offline: None,
        language: None,
        gguf_sha256: None,
        code_revision: None,
        backend: None,
        profile: None,
        extra: BTreeMap::new(),
    }
}

fn ensure_worker_coverage(
    states: &[ModelState],
    questions: &[ModelQuestion],
    results: &[ModelResult],
) -> Result<()> {
    for state in states {
        for question in questions {
            ensure!(
                results.iter().any(
                    |result| result.state_id == state.id && result.question_id == question.id()
                ),
                "worker omitted result for state {} question {}",
                state.id,
                question.id()
            );
        }
    }
    Ok(())
}

fn unsupported_predicates(states: &[ModelState], questions: &[ModelQuestion]) -> Vec<ModelResult> {
    let mut results = Vec::new();
    for state in states {
        for question in questions {
            results.push(ModelResult {
                state_id: state.id.clone(),
                question_id: question.id().to_string(),
                status: ModelStatus::Unsupported,
                label: None,
                value: None,
                scores: None,
                reason: Some("Local GLiNER2.5 does not support evidence predicates. A low classification score is not missing evidence.".into()),
                confidence: None,
                provider_value: None,
            });
        }
    }
    results
}

fn merge_predicate_results(
    states: &[ModelState],
    questions: &[ModelQuestion],
    worker_results: Vec<ModelResult>,
) -> Vec<ModelResult> {
    let mut merged = Vec::new();
    for state in states {
        for question in questions {
            if matches!(question, ModelQuestion::Predicate { .. }) {
                merged.push(ModelResult {
                    state_id: state.id.clone(),
                    question_id: question.id().to_string(),
                    status: ModelStatus::Unsupported,
                    label: None,
                    value: None,
                    scores: None,
                    reason: Some("Local GLiNER2.5 does not support evidence predicates. A low classification score is not missing evidence.".into()),
                    confidence: None,
                    provider_value: None,
                });
            } else if let Some(result) = worker_results
                .iter()
                .find(|result| result.state_id == state.id && result.question_id == question.id())
            {
                merged.push(result.clone());
            }
        }
    }
    merged
}

fn local_not_invoked_provenance() -> ModelProvenance {
    let mut provenance = blank_provenance();
    provenance.provider = "local".into();
    provenance.model = LOCAL_MODEL_ID.into();
    provenance.revision = LOCAL_MODEL_REVISION.into();
    provenance.adapter_version = GLINER_ADAPTER_VERSION.into();
    provenance.device = "not_invoked".into();
    provenance.precision = "not_applicable".into();
    provenance.score_semantics = Value::String("predicate_unsupported".into());
    provenance.input_policy = GLINER_INPUT_POLICY.into();
    provenance.profile = Some("lightweight".into());
    provenance
}

pub fn cache_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CHECKWEAVE_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("checkweave"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME is not set; cannot locate the shared model cache")?;
    Ok(PathBuf::from(home).join(".cache").join("checkweave"))
}

fn python_executable(config: &ModelConfig, choice: &EnvChoice) -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("CHECKWEAVE_PYTHON") {
        let path = PathBuf::from(explicit);
        ensure!(
            path.is_file(),
            "CHECKWEAVE_PYTHON is not a file: {}",
            path.display()
        );
        return Ok(path);
    }
    let root = cache_root()?;
    let manifest = read_manifest(&env_dir_for_requirements(
        &root,
        config,
        choice.wheel.as_ref(),
    )?)?;
    let python = PathBuf::from(manifest.python);
    ensure!(
        python.is_file(),
        "managed python no longer exists at {}",
        python.display()
    );
    Ok(python)
}

#[derive(Debug, Serialize, Deserialize)]
struct InstallManifest {
    version: u32,
    requirements_sha256: String,
    installer: String,
    python: String,
    #[serde(default)]
    torch_wheel: String,
}

pub async fn setup(config: &ModelConfig, offline: bool) -> Result<Value> {
    setup_cancellable(config, offline, &AtomicBool::new(false)).await
}

/// Same as [`setup`], but installation waits can observe `cancel` without blocking the runtime.
pub async fn setup_cancellable(
    config: &ModelConfig,
    offline: bool,
    cancel: &AtomicBool,
) -> Result<Value> {
    let config = config.clone().normalized()?;
    match config.provider {
        ModelProviderKind::Local => setup_local(&config, offline, cancel).await,
        ModelProviderKind::Jev => {
            if offline {
                return Ok(serde_json::json!({
                    "provider": "jev",
                    "model": config.model,
                    "endpoint_origin": config.endpoint.as_deref().map(endpoint_origin),
                    "api_key_env": config.api_key_env,
                    "installed": false,
                    "offline": true,
                }));
            }
            let name = config
                .api_key_env
                .clone()
                .context("jev provider requires api_key_env")?;
            let present = std::env::var_os(&name).is_some_and(|value| !value.is_empty());
            ensure!(present, "environment variable {name} is not set");
            Ok(serde_json::json!({
                "provider": "jev",
                "model": config.model,
                "endpoint_origin": config.endpoint.as_deref().map(endpoint_origin),
                "api_key_env": name,
                "api_key_present": true,
                "installed": false,
                "offline": false,
            }))
        }
    }
}

async fn setup_local(config: &ModelConfig, offline: bool, cancel: &AtomicBool) -> Result<Value> {
    if std::env::var_os("CHECKWEAVE_PYTHON").is_some() {
        let python = python_executable(config, &EnvChoice { wheel: None })?;
        let worker = materialize_worker(&cache_root()?)?;
        return Ok(serde_json::json!({
            "provider": "local",
            "profile": profile_name(config.profile.unwrap_or(LocalProfile::Default)),
            "managed_env": false,
            "python": python,
            "worker_path": worker,
            "model": config.checkpoint,
            "revision": config.revision,
            "offline": offline,
        }));
    }
    let choice = EnvChoice::resolve(config)?;
    install_local_environment(config, offline, cancel, &choice).await?;
    let root = cache_root()?;
    let env_dir = env_dir_for_requirements(&root, config, choice.wheel.as_ref())?;
    let manifest = read_manifest(&env_dir)?;
    Ok(serde_json::json!({
        "provider": "local",
        "profile": profile_name(config.profile.unwrap_or(LocalProfile::Default)),
        "managed_env": true,
        "environment": env_dir,
        "python": manifest.python,
        "installer": manifest.installer,
        "model": config.checkpoint,
        "revision": config.revision,
        "adapter_version": settings_identity(config).adapter_version,
        "offline": offline,
        "requirements_sha256": manifest.requirements_sha256,
        "torch_wheel": manifest.torch_wheel,
    }))
}

async fn install_local_environment(
    config: &ModelConfig,
    offline: bool,
    cancel: &AtomicBool,
    choice: &EnvChoice,
) -> Result<()> {
    let root = cache_root()?;
    fs::create_dir_all(&root)?;
    let requirements = requirements_text(config)?;
    let wheel = choice.wheel.as_ref();
    let env_dir = env_dir_for_requirements(&root, config, wheel)?;
    fs::create_dir_all(&env_dir)?;
    let lock_path = env_dir.join("install.lock");
    let lock_file = acquire_install_lock(&lock_path, cancel).await?;
    let requirements_sha = requirements_identity(config, wheel)?;
    let outcome = install_locked(
        &env_dir,
        &requirements,
        &requirements_sha,
        wheel,
        offline,
        cancel,
    )
    .await;
    let _ = fs2::FileExt::unlock(&lock_file);
    outcome
}

async fn acquire_install_lock(path: &Path, cancel: &AtomicBool) -> Result<std::fs::File> {
    let path = path.to_path_buf();
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("installation cancelled");
        }
        let attempt = path.clone();
        let locked = tokio::task::spawn_blocking(move || {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&attempt)?;
            match file.try_lock_exclusive() {
                Ok(()) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await
        .context("install lock task")?
        .context("open install lock")?;
        if let Some(file) = locked {
            return Ok(file);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn install_locked(
    env_dir: &Path,
    requirements: &str,
    requirements_sha: &str,
    wheel: Option<&TorchWheel>,
    offline: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    materialize_worker(&cache_root()?)?;
    if let Ok(manifest) = read_manifest(env_dir)
        && manifest.version == 1
        && manifest.requirements_sha256 == requirements_sha
        && Path::new(&manifest.python).is_file()
    {
        return Ok(());
    }
    if offline {
        bail!("offline preinstall is incomplete for {}", env_dir.display());
    }
    if cancel.load(Ordering::Acquire) {
        bail!("installation cancelled");
    }
    let _ = fs::remove_file(env_dir.join("complete.json"));
    let staging = env_dir.join("venv");
    if staging.exists() {
        fs::remove_dir_all(&staging).context("remove incomplete virtualenv")?;
    }
    let req_path = env_dir.join("requirements.txt");
    fs::write(&req_path, requirements)?;
    let timeout = Duration::from_millis(install_timeout_ms());
    let installer = if let Some(uv) = command_on_path("uv") {
        run_command(
            &uv,
            &[
                "venv",
                "--python",
                "3.12",
                staging.to_str().context("venv path")?,
            ],
            timeout,
            cancel,
        )
        .await?;
        let python = venv_python(&staging);
        install_requirements(
            Some(&uv),
            &python,
            &req_path,
            requirements,
            wheel,
            timeout,
            cancel,
        )
        .await?;
        "uv"
    } else {
        let python3 = command_on_path("python3.12")
            .context("Python 3.12 is required to create the inference environment")?;
        run_command(
            &python3,
            &["-m", "venv", staging.to_str().context("venv path")?],
            timeout,
            cancel,
        )
        .await?;
        let python = venv_python(&staging);
        install_requirements(
            None,
            &python,
            &req_path,
            requirements,
            wheel,
            timeout,
            cancel,
        )
        .await?;
        "venv"
    };
    let python = venv_python(&staging);
    ensure!(
        python.is_file(),
        "installer did not create {}",
        python.display()
    );
    assert_python312(&python)?;
    let size = directory_bytes(&staging, MAX_ENV_BYTES)?;
    ensure!(size <= MAX_ENV_BYTES, "inference environment exceeds 8 GiB");
    let manifest = InstallManifest {
        version: 1,
        requirements_sha256: requirements_sha.to_string(),
        installer: installer.into(),
        python: python.to_string_lossy().into_owned(),
        torch_wheel: wheel
            .map(TorchWheel::identity)
            .unwrap_or("none")
            .to_string(),
    };
    write_atomic(
        &env_dir.join("complete.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn install_timeout_ms() -> u64 {
    std::env::var("CHECKWEAVE_INSTALL_TIMEOUT_MS")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|ms| (1..=3_600_000).contains(ms))
        .unwrap_or(DEFAULT_INSTALL_TIMEOUT_MS)
}

async fn install_requirements(
    uv: Option<&Path>,
    python: &Path,
    requirements_path: &Path,
    requirements: &str,
    wheel: Option<&TorchWheel>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    let _ = requirements;
    let python_arg = python.to_str().context("python path")?;
    let req_arg = requirements_path.to_str().context("requirements path")?;
    if let Some(wheel) = wheel {
        let mut pip = Vec::new();
        if uv.is_some() {
            pip.extend(["pip", "install", "--python", python_arg]);
        } else {
            pip.extend(["-m", "pip", "install"]);
        }
        pip.push(wheel.requirement());
        if let Some(index) = wheel.index_url() {
            pip.extend(["--index-url", index]);
        }
        let program = uv.unwrap_or(python);
        run_command(program, &pip, timeout, cancel).await?;
    }
    let mut extra = Vec::new();
    if wheel.is_some() {
        extra.push(("CMAKE_BUILD_PARALLEL_LEVEL", "6"));
    }
    if let Some(uv) = uv {
        run_command_env(
            uv,
            &["pip", "install", "--python", python_arg, "-r", req_arg],
            timeout,
            cancel,
            &extra,
        )
        .await
    } else {
        run_command_env(
            python,
            &["-m", "pip", "install", "-r", req_arg],
            timeout,
            cancel,
            &extra,
        )
        .await
    }
}

async fn run_command(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    run_command_env(program, args, timeout, cancel, &[]).await
}

async fn run_command_env(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    cancel: &AtomicBool,
    env: &[(&str, &str)],
) -> Result<()> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", program.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Acquire) {
            kill_child(&mut child).await;
            bail!("installation cancelled");
        }
        if Instant::now() >= deadline {
            kill_child(&mut child).await;
            bail!("installation exceeded {} ms", timeout.as_millis());
        }
        match tokio::time::timeout(Duration::from_millis(200), child.wait()).await {
            Err(_) => continue,
            Ok(Ok(status)) if status.success() => return Ok(()),
            Ok(Ok(status)) => bail!("{} exited {status}", program.display()),
            Ok(Err(error)) => bail!("wait {}: {error}", program.display()),
        }
    }
}

async fn kill_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_group(pid);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn venv_python(env_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        env_dir.join("Scripts").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        env_dir.join("bin").join("python")
    }
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn env_dir_for_requirements(
    root: &Path,
    config: &ModelConfig,
    wheel: Option<&TorchWheel>,
) -> Result<PathBuf> {
    Ok(root
        .join("envs")
        .join(requirements_identity(config, wheel)?))
}

fn requirements_identity(config: &ModelConfig, wheel: Option<&TorchWheel>) -> Result<String> {
    let requirements = requirements_text(config)?;
    let profile = profile_name(config.profile.unwrap_or(LocalProfile::Default));
    let wheel_id = wheel.map(TorchWheel::identity).unwrap_or("none");
    Ok(fingerprint_bytes(
        format!("{profile}\n{wheel_id}\n{requirements}").as_bytes(),
    ))
}

const BF16_PARAMETER_COUNT: u64 = 4_659_861_248;
const BF16_FIT_BYTES: u64 = BF16_PARAMETER_COUNT * 2 + 1536 * 1024 * 1024;
const TORCH_CPU_INDEX: &str = "https://download.pytorch.org/whl/cpu";
const TORCH_CUDA_INDEX: &str = "https://download.pytorch.org/whl/cu128";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorchWheel {
    /// Linux CPU wheel. This is the selection for an unfit or absent GPU, including the Tesla M10s.
    Cpu,
    /// Linux CUDA 12.8 wheel. Installed only when one visible GPU passes the BF16 fit gate.
    CudaCu128,
    /// Official default wheel, used for macOS CPU/MPS. Not the Linux CPU index, and not tested here.
    DefaultHost,
}

impl TorchWheel {
    pub fn identity(&self) -> &'static str {
        match self {
            TorchWheel::Cpu => "torch==2.10.0+cpu",
            TorchWheel::CudaCu128 => "torch==2.10.0+cu128",
            TorchWheel::DefaultHost => "torch==2.10.0+default-untested",
        }
    }

    fn requirement(&self) -> &'static str {
        "torch==2.10.0"
    }

    fn index_url(&self) -> Option<&'static str> {
        match self {
            TorchWheel::Cpu => Some(TORCH_CPU_INDEX),
            TorchWheel::CudaCu128 => Some(TORCH_CUDA_INDEX),
            TorchWheel::DefaultHost => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleGpu {
    pub index: u32,
    pub free_bytes: u64,
    pub compute_major: u32,
}

pub fn select_semif_torch_wheel(
    device: &str,
    visible: &[VisibleGpu],
    host_os: &str,
) -> Result<TorchWheel> {
    if host_os == "macos" {
        if device == "cpu" || device == "mps" || device == "auto" {
            return Ok(TorchWheel::DefaultHost);
        }
        bail!("macOS SemIf CUDA is untested; refusing to install a Linux torch wheel for {device}");
    }
    if host_os != "linux" {
        bail!("SemIf managed torch on {host_os} is untested");
    }
    if device == "cpu" {
        return Ok(TorchWheel::Cpu);
    }
    if device == "mps" {
        bail!("mps is not supported on linux");
    }
    let addressed = addressed_gpu(device, visible)?;
    if device == "auto" {
        return Ok(if visible.iter().any(gpu_can_hold_bf16) {
            TorchWheel::CudaCu128
        } else {
            TorchWheel::Cpu
        });
    }
    let Some(gpu) = addressed else {
        bail!(
            "explicit {device} has no matching visible GPU; the CPU torch wheel is not a substitute"
        );
    };
    if !gpu_can_hold_bf16(gpu) {
        bail!(
            "explicit {device} cannot run SemIf BF16 (compute capability {} , {} free bytes, need capability >= 8 and {BF16_FIT_BYTES} free bytes); Q4 is not a silent substitute",
            gpu.compute_major,
            gpu.free_bytes
        );
    }
    Ok(TorchWheel::CudaCu128)
}

fn addressed_gpu<'a>(device: &str, visible: &'a [VisibleGpu]) -> Result<Option<&'a VisibleGpu>> {
    if device == "auto" {
        return Ok(None);
    }
    let ordinal = if device == "cuda" {
        0
    } else if let Some(index) = device.strip_prefix("cuda:") {
        index.parse::<usize>().context("cuda device index")?
    } else {
        bail!("unsupported SemIf device {device}");
    };
    Ok(visible.get(ordinal))
}

fn gpu_can_hold_bf16(gpu: &VisibleGpu) -> bool {
    gpu.compute_major >= 8 && gpu.free_bytes >= BF16_FIT_BYTES
}

/// Torch wheel chosen once for a setup or worker start. Later starts may resolve again.
#[derive(Clone, Debug)]
pub struct EnvChoice {
    wheel: Option<TorchWheel>,
}

impl EnvChoice {
    fn resolve(config: &ModelConfig) -> Result<Self> {
        if std::env::var_os("CHECKWEAVE_REQUIREMENTS_FILE").is_some()
            || config.profile.unwrap_or(LocalProfile::Default) != LocalProfile::Default
        {
            return Ok(Self { wheel: None });
        }
        Self::from_visible(config, &probe_visible_gpus()?, host_os())
    }

    /// Build the choice from an already captured GPU list. Does not probe again.
    pub fn from_visible(
        config: &ModelConfig,
        visible: &[VisibleGpu],
        host_os: &str,
    ) -> Result<Self> {
        if config.profile.unwrap_or(LocalProfile::Default) != LocalProfile::Default {
            return Ok(Self { wheel: None });
        }
        Ok(Self {
            wheel: Some(select_semif_torch_wheel(&config.device, visible, host_os)?),
        })
    }

    pub fn environment_key(&self, config: &ModelConfig) -> Result<String> {
        requirements_identity(config, self.wheel.as_ref())
    }

    pub fn wheel_identity(&self) -> &'static str {
        self.wheel
            .as_ref()
            .map(TorchWheel::identity)
            .unwrap_or("none")
    }
}

fn host_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "other"
    }
}

pub fn parse_nvidia_smi_csv(text: &str) -> Vec<VisibleGpu> {
    let mut gpus = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            continue;
        }
        let Ok(index) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(free_mib) = parts[1].parse::<u64>() else {
            continue;
        };
        let Some(major) = parts[2]
            .split('.')
            .next()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        gpus.push(VisibleGpu {
            index,
            free_bytes: free_mib.saturating_mul(1024 * 1024),
            compute_major: major,
        });
    }
    gpus
}

pub fn filter_cuda_visible_devices(
    gpus: &[VisibleGpu],
    spec: Option<&str>,
) -> Result<Vec<VisibleGpu>> {
    let Some(spec) = spec else {
        return Ok(gpus.to_vec());
    };
    let text = spec.trim();
    if text.is_empty() || text == "-1" {
        return Ok(Vec::new());
    }
    let mut ordered = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let index: u32 = part.parse().with_context(|| {
            format!("CUDA_VISIBLE_DEVICES={spec} is not a comma-separated GPU index list")
        })?;
        if let Some(gpu) = gpus.iter().find(|gpu| gpu.index == index) {
            ordered.push(gpu.clone());
        }
    }
    Ok(ordered)
}

pub fn probe_visible_gpus() -> Result<Vec<VisibleGpu>> {
    let mut command = std::process::Command::new("nvidia-smi");
    command
        .args([
            "--query-gpu=index,memory.free,compute_cap",
            "--format=csv,noheader,nounits",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(Vec::new()),
    };
    let started = Instant::now();
    let status = loop {
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Vec::new());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return Ok(Vec::new()),
        }
    };
    if !status.success() {
        return Ok(Vec::new());
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stdout);
    }
    let parsed = parse_nvidia_smi_csv(&stdout);
    filter_cuda_visible_devices(
        &parsed,
        std::env::var("CUDA_VISIBLE_DEVICES").ok().as_deref(),
    )
}

fn read_manifest(env_dir: &Path) -> Result<InstallManifest> {
    let bytes = fs::read(env_dir.join("complete.json"))
        .context("inference environment is not installed")?;
    let manifest: InstallManifest =
        serde_json::from_slice(&bytes).context("inference install manifest is corrupt")?;
    ensure!(
        manifest.version == 1,
        "unsupported install manifest version {}",
        manifest.version
    );
    Ok(manifest)
}

fn write_atomic(path: &Path, bytes: Vec<u8>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn directory_bytes(root: &Path, cap: u64) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > cap {
                    return Ok(total);
                }
            }
        }
    }
    Ok(total)
}

fn requirements_text(config: &ModelConfig) -> Result<String> {
    if let Some(path) = std::env::var_os("CHECKWEAVE_REQUIREMENTS_FILE") {
        return fs::read_to_string(PathBuf::from(path))
            .context("read CHECKWEAVE_REQUIREMENTS_FILE");
    }
    match config.profile.unwrap_or(LocalProfile::Default) {
        LocalProfile::Default => Ok(EMBEDDED_SEMIF_REQUIREMENTS.to_string()),
        LocalProfile::Lightweight => Ok(EMBEDDED_GLINER_REQUIREMENTS.to_string()),
    }
}

fn assert_python312(python: &Path) -> Result<()> {
    let output = std::process::Command::new(python)
        .arg("-c")
        .arg("import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')")
        .output()
        .with_context(|| format!("run {}", python.display()))?;
    let version = String::from_utf8_lossy(&output.stdout);
    ensure!(
        version.trim() == "3.12",
        "managed inference requires Python 3.12, found {}",
        version.trim()
    );
    Ok(())
}

const WORKER_ASSETS: &[(&str, &str)] = &[
    (
        "checkweave_worker/__init__.py",
        include_str!("../python/checkweave_worker/__init__.py"),
    ),
    (
        "checkweave_worker/__main__.py",
        include_str!("../python/checkweave_worker/__main__.py"),
    ),
    (
        "checkweave_worker/constants.py",
        include_str!("../python/checkweave_worker/constants.py"),
    ),
    (
        "checkweave_worker/devices.py",
        include_str!("../python/checkweave_worker/devices.py"),
    ),
    (
        "checkweave_worker/engine.py",
        include_str!("../python/checkweave_worker/engine.py"),
    ),
    (
        "checkweave_worker/frames.py",
        include_str!("../python/checkweave_worker/frames.py"),
    ),
    (
        "checkweave_worker/gliner_backend.py",
        include_str!("../python/checkweave_worker/gliner_backend.py"),
    ),
    (
        "checkweave_worker/questions.py",
        include_str!("../python/checkweave_worker/questions.py"),
    ),
    (
        "checkweave_worker/quiet.py",
        include_str!("../python/checkweave_worker/quiet.py"),
    ),
    (
        "checkweave_worker/semif_backend.py",
        include_str!("../python/checkweave_worker/semif_backend.py"),
    ),
    (
        "checkweave_worker/stdio.py",
        include_str!("../python/checkweave_worker/stdio.py"),
    ),
];

const EMBEDDED_SEMIF_REQUIREMENTS: &str = include_str!("../python/requirements-semif.txt");
const EMBEDDED_GLINER_REQUIREMENTS: &str = include_str!("../python/requirements-gliner2.txt");

pub fn materialize_worker(cache: &Path) -> Result<PathBuf> {
    let root = cache.join("worker").join(ADAPTER_VERSION);
    for (relative, contents) in WORKER_ASSETS {
        write_if_changed(&root.join(relative), contents.as_bytes())?;
    }
    Ok(root)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.is_file() && fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("py.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

async fn evaluate_jev(
    config: &ModelConfig,
    states: Vec<ModelState>,
    questions: Vec<ModelQuestion>,
    timeout_ms: u64,
    cancel: &AtomicBool,
) -> Result<ModelResponse> {
    ensure!(!config.offline, "jev provider cannot evaluate offline");
    let endpoint = config.endpoint.clone().context("jev endpoint missing")?;
    validate_endpoint(&endpoint)?;
    let env_name = config
        .api_key_env
        .clone()
        .context("jev provider requires api_key_env")?;
    let api_key = std::env::var(&env_name)
        .with_context(|| format!("environment variable {env_name} is not set"))?;
    ensure!(
        !api_key.is_empty(),
        "environment variable {env_name} is empty"
    );
    ensure!(
        !api_key.chars().any(|ch| ch.is_control()),
        "api key contains a control character"
    );
    let model = config.model.clone().unwrap_or_else(|| JEV_MODEL_ID.into());
    ensure!(
        is_concrete_jev_model(&model),
        "refusing unpinned jev model {model}"
    );
    for state in &states {
        jev_char_budget(state, &questions)?;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let connect = Duration::from_millis(timeout_ms.min(10_000));
    let client = reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(Duration::from_millis(timeout_ms))
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
        .user_agent(concat!("checkweave/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build jev client")?;
    let mut combined = Vec::new();
    let mut usage = ModelUsage::default();
    let mut saw_usage = false;
    let mut response_model: Option<String> = None;
    let request_id = uuid::Uuid::new_v4().to_string();
    for state in &states {
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled");
        }
        let body = jev_body(&state.text, &model, &questions)?;
        let raw = post_jev(&client, &endpoint, &api_key, body, deadline, cancel).await?;
        let parsed: JevHttpResponse =
            serde_json::from_slice(&raw).context("jev response is not JSON")?;
        ensure!(
            is_concrete_jev_model(&parsed.model),
            "jev response model {} is not a pinned id",
            parsed.model
        );
        if let Some(previous) = &response_model {
            ensure!(
                previous == &parsed.model,
                "jev response model changed within one evaluate"
            );
        } else {
            response_model = Some(parsed.model.clone());
        }
        if let Some(item) = parsed.usage {
            saw_usage = true;
            usage.input_tokens = usage.input_tokens.saturating_add(item.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(item.output_tokens);
        }
        combined.extend(map_jev_answers(state, &questions, parsed.answers)?);
    }
    let model_id = response_model.context("jev response omitted model")?;
    let mut provenance = blank_provenance();
    provenance.provider = "jev".into();
    provenance.model = model_id.clone();
    provenance.revision = model_id;
    provenance.adapter_version = ADAPTER_VERSION.into();
    provenance.device = "remote".into();
    provenance.precision = "provider-managed".into();
    provenance.score_semantics = Value::String(jev_score_semantics(&questions));
    provenance.input_policy = JEV_INPUT_POLICY.into();
    provenance
        .runtime_versions
        .insert("http".into(), Value::String("typesafe-systemone-v1".into()));
    provenance.runtime_versions.insert(
        "endpoint_origin".into(),
        Value::String(endpoint_origin(&endpoint)),
    );
    provenance
        .runtime_versions
        .insert("requested_model".into(), Value::String(model));
    provenance.usage = saw_usage.then_some(usage);
    provenance.profile = Some("jev".into());
    Ok(ModelResponse {
        id: request_id,
        results: combined,
        provenance,
    })
}

fn jev_score_semantics(questions: &[ModelQuestion]) -> String {
    let mut parts = Vec::new();
    if questions
        .iter()
        .any(|q| matches!(q, ModelQuestion::Predicate { .. }))
    {
        parts.push("noul_yes_probability");
    }
    if questions
        .iter()
        .any(|q| matches!(q, ModelQuestion::Choice { .. }))
    {
        parts.push("choice_exclusive_probabilities");
    }
    if questions
        .iter()
        .any(|q| matches!(q, ModelQuestion::Ordinal { .. }))
    {
        parts.push("score_expected_index;value_is_adapter_expected_level");
    }
    parts.join(";")
}

fn jev_body(state: &str, model: &str, questions: &[ModelQuestion]) -> Result<Vec<u8>> {
    let mut mapped = Map::new();
    for question in questions {
        mapped.insert(question.id().to_string(), jev_question(question)?);
    }
    let body = serde_json::json!({
        "state": state,
        "model": model,
        "questions": Value::Object(mapped),
    });
    Ok(serde_json::to_vec(&body)?)
}

fn jev_question(question: &ModelQuestion) -> Result<Value> {
    Ok(match question {
        ModelQuestion::Predicate { statement, .. } => serde_json::json!({
            "type": "noul",
            "instructions": statement,
            "criteria": { "true": "The statement holds for the state.", "false": "The statement does not hold for the state." }
        }),
        ModelQuestion::Choice { labels, .. } => {
            let mut criteria = Map::new();
            for label in labels {
                let value = if label.description.is_empty() {
                    Value::Null
                } else {
                    Value::String(label.description.clone())
                };
                criteria.insert(label.label.clone(), value);
            }
            serde_json::json!({
                "type": "choice",
                "instructions": "Select the single best label.",
                "criteria": Value::Object(criteria),
            })
        }
        ModelQuestion::Ordinal { levels, .. } => {
            let criteria: Vec<String> = levels
                .iter()
                .map(|level| {
                    if level.description.is_empty() {
                        level.label.clone()
                    } else {
                        level.description.clone()
                    }
                })
                .collect();
            serde_json::json!({
                "type": "score",
                "instructions": "Rate the state on this ordered rubric from lowest to highest.",
                "criteria": criteria,
            })
        }
    })
}

async fn post_jev(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    body: Vec<u8>,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut retries = 0u32;
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("evaluation timed out");
        }
        let request = client
            .post(endpoint)
            .timeout(remaining)
            .bearer_auth(api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone());
        let response = tokio::select! {
            biased;
            _ = cancel_flag(cancel) => bail!("evaluation cancelled"),
            result = request.send() => result,
        };
        match response {
            Ok(response) => {
                let status = response.status();
                let retry_delay = retry_after(&response);
                let bytes = read_jev_body(response, deadline, cancel, api_key).await?;
                let text = redact_sensitive(&String::from_utf8_lossy(&bytes), api_key);
                if status.is_success() {
                    return Ok(text.into_bytes());
                }
                let code = status.as_u16();
                if RETRY_STATUSES.contains(&code) && retries < 2 {
                    retries += 1;
                    let delay = retry_delay
                        .unwrap_or_else(|| Duration::from_millis(200 * u64::from(retries)));
                    let left = deadline.saturating_duration_since(Instant::now());
                    if delay >= left {
                        sleep_cancel(left, cancel).await?;
                        bail!(
                            "jev HTTP {code} retry delay exceeds the deadline: {}",
                            clip(&text)
                        );
                    }
                    sleep_cancel(delay, cancel).await?;
                    continue;
                }
                bail!("jev HTTP {code}: {}", clip(&text));
            }
            Err(error) => {
                let rendered = redact_sensitive(&error.to_string(), api_key);
                bail!("jev request failed: {rendered}");
            }
        }
    }
}

async fn read_jev_body(
    mut response: reqwest::Response,
    deadline: Instant,
    cancel: &AtomicBool,
    api_key: &str,
) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length()
        && length > u64::try_from(MAX_FRAME_BYTES).unwrap_or(u64::MAX)
    {
        bail!("jev response declared {length} bytes, over the 8MiB limit");
    }
    let mut body = Vec::new();
    loop {
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("evaluation timed out");
        }
        let chunk = tokio::select! {
            biased;
            _ = cancel_flag(cancel) => bail!("evaluation cancelled"),
            chunk = tokio::time::timeout(remaining, response.chunk()) => chunk,
        };
        match chunk {
            Ok(Ok(Some(chunk))) => {
                let next = body.len().saturating_add(chunk.len());
                if next > MAX_FRAME_BYTES {
                    bail!("jev response exceeded 8MiB");
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => return Ok(body),
            Ok(Err(error)) => {
                if error.is_timeout() {
                    bail!("evaluation timed out");
                }
                let rendered = redact_sensitive(&error.to_string(), api_key);
                bail!("jev response body failed: {rendered}");
            }
            Err(_) => bail!("evaluation timed out"),
        }
    }
}

fn clip(text: &str) -> String {
    let mut one_line = text.replace(['\n', '\r'], " ");
    if one_line.len() <= 500 {
        return one_line;
    }
    let mut end = 500;
    while end > 0 && !one_line.is_char_boundary(end) {
        end -= 1;
    }
    one_line.truncate(end);
    one_line
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    if let Some(value) = response.headers().get("retry-after-ms") {
        let text = value.to_str().ok()?;
        let millis: u64 = text.parse().ok()?;
        return Some(Duration::from_millis(millis));
    }
    if let Some(value) = response.headers().get(reqwest::header::RETRY_AFTER) {
        let text = value.to_str().ok()?;
        if let Ok(seconds) = text.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
    }
    None
}

async fn cancel_flag(cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn sleep_cancel(delay: Duration, cancel: &AtomicBool) -> Result<()> {
    let end = Instant::now() + delay;
    while Instant::now() < end {
        if cancel.load(Ordering::Acquire) {
            bail!("evaluation cancelled");
        }
        let slice = end
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        if slice.is_zero() {
            break;
        }
        tokio::time::sleep(slice).await;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct JevHttpResponse {
    model: String,
    answers: BTreeMap<String, JevAnswer>,
    #[serde(default)]
    usage: Option<JevUsage>,
}

#[derive(Debug, Deserialize)]
struct JevUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct JevAnswer {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    noul: Option<f64>,
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    probabilities: Option<BTreeMap<String, f64>>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    score: Option<f64>,
}

fn map_jev_answers(
    state: &ModelState,
    questions: &[ModelQuestion],
    mut answers: BTreeMap<String, JevAnswer>,
) -> Result<Vec<ModelResult>> {
    let mut results = Vec::new();
    for question in questions {
        let Some(answer) = answers.remove(question.id()) else {
            bail!("jev omitted answer {}", question.id());
        };
        results.push(map_one_answer(&state.id, question, answer)?);
    }
    Ok(results)
}

fn map_one_answer(
    state_id: &str,
    question: &ModelQuestion,
    answer: JevAnswer,
) -> Result<ModelResult> {
    let base = |status, label, value, scores, reason, confidence, provider_value| ModelResult {
        state_id: state_id.to_string(),
        question_id: question.id().to_string(),
        status,
        label,
        value,
        scores,
        reason,
        confidence,
        provider_value,
    };
    match question {
        ModelQuestion::Predicate { .. } => {
            ensure!(
                answer.kind == "noul",
                "jev answer type {} for predicate {}",
                answer.kind,
                question.id()
            );
            let Some(noul) = answer.noul.filter(|n| n.is_finite()) else {
                return Ok(base(
                    ModelStatus::Unresolved,
                    None,
                    None,
                    None,
                    Some("jev noul was missing".into()),
                    None,
                    None,
                ));
            };
            if !(0.0..=1.0).contains(&noul) {
                return Ok(base(
                    ModelStatus::Unresolved,
                    None,
                    Some(Value::from(noul)),
                    None,
                    Some("jev noul outside 0..=1".into()),
                    None,
                    None,
                ));
            }
            let mut scores = BTreeMap::new();
            scores.insert("true".into(), noul);
            scores.insert("false".into(), 1.0 - noul);
            let label = if noul >= 0.5 { "true" } else { "false" };
            Ok(base(
                ModelStatus::Resolved,
                Some(label.into()),
                Some(Value::from(noul)),
                Some(scores),
                None,
                None,
                None,
            ))
        }
        ModelQuestion::Choice { labels, .. } => {
            ensure!(
                answer.kind == "choice",
                "jev answer type {} for choice {}",
                answer.kind,
                question.id()
            );
            let Some(choice) = answer.choice else {
                return Ok(base(
                    ModelStatus::Unresolved,
                    None,
                    None,
                    answer.probabilities,
                    Some("jev choice was missing".into()),
                    answer.confidence,
                    None,
                ));
            };
            if !labels.iter().any(|label| label.label == choice) {
                return Ok(base(
                    ModelStatus::Unresolved,
                    Some(choice),
                    None,
                    answer.probabilities,
                    Some("jev choice is not one of the requested labels".into()),
                    answer.confidence,
                    None,
                ));
            }
            Ok(base(
                ModelStatus::Resolved,
                Some(choice),
                None,
                answer.probabilities,
                None,
                answer.confidence,
                None,
            ))
        }
        ModelQuestion::Ordinal { levels, .. } => {
            ensure!(
                answer.kind == "score",
                "jev answer type {} for ordinal {}",
                answer.kind,
                question.id()
            );
            let Some(probabilities) = answer.probabilities else {
                return Ok(base(
                    ModelStatus::Unresolved,
                    None,
                    None,
                    None,
                    Some("jev score omitted probabilities".into()),
                    answer.confidence,
                    answer.score,
                ));
            };
            let mut scores = BTreeMap::new();
            let mut expected = 0.0;
            let mut best_index: Option<usize> = None;
            let mut best_prob = f64::NEG_INFINITY;
            for (index, level) in levels.iter().enumerate() {
                let Some(probability) = probabilities.get(&index.to_string()).copied() else {
                    return Ok(base(
                        ModelStatus::Unresolved,
                        None,
                        None,
                        Some(probabilities),
                        Some(format!("jev score omitted level {index}")),
                        answer.confidence,
                        answer.score,
                    ));
                };
                if !probability.is_finite() {
                    return Ok(base(
                        ModelStatus::Unresolved,
                        None,
                        None,
                        Some(probabilities),
                        Some("jev score probability was not finite".into()),
                        answer.confidence,
                        answer.score,
                    ));
                }
                scores.insert(level.label.clone(), probability);
                expected += probability * level.value;
                if probability > best_prob {
                    best_prob = probability;
                    best_index = Some(index);
                }
            }
            let Some(index) = best_index else {
                return Ok(base(
                    ModelStatus::Unresolved,
                    None,
                    None,
                    Some(scores),
                    Some("jev score had no levels".into()),
                    answer.confidence,
                    answer.score,
                ));
            };
            Ok(base(
                ModelStatus::Resolved,
                Some(levels[index].label.clone()),
                Some(Value::from(expected)),
                Some(scores),
                None,
                answer.confidence,
                answer.score,
            ))
        }
    }
}
