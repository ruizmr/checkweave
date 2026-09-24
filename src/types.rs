use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
/// Crate version spoken on IPC, distinct from [`PROTOCOL_VERSION`].
/// A matching protocol with a different implementation must not be reused.
pub const IMPLEMENTATION_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const OPERATOR_VERSION: &str = "jsonl-check-v1";

/// JSON Pointer for an entire value. `/` is the empty-name member, not the root.
pub const JSON_POINTER_ROOT: &str = "";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    Exists { path: String },
    Eq { path: String, value: Value },
    Ne { path: String, value: Value },
    Contains { path: String, value: String },
    Regex { path: String, pattern: String },
    Gt { path: String, value: f64 },
    Ge { path: String, value: f64 },
    Lt { path: String, value: f64 },
    Le { path: String, value: f64 },
    Kind { path: String, kind: JsonKind },
    All { predicates: Vec<Predicate> },
    Any { predicates: Vec<Predicate> },
    Not { predicate: Box<Predicate> },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JsonKind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_records: usize,
    pub max_results: usize,
    pub timeout_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 1000,
            max_bytes: 32 * 1024 * 1024,
            max_records: 100_000,
            max_results: 50,
            timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    /// Workspace-relative globs. JSON Lines records are selected from matching files.
    pub include: Vec<String>,
    pub predicate: Predicate,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Coverage {
    pub files: usize,
    pub records: usize,
    pub evaluated: usize,
    pub matched: usize,
    pub unmatched: usize,
    pub unresolved: usize,
    pub skipped: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceRef {
    pub path: String,
    pub line: usize,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ItemResult {
    pub source: SourceRef,
    /// null means unresolved (including invalid JSON or incompatible value types).
    pub matched: Option<bool>,
    pub reason: Option<String>,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceFingerprint {
    pub path: String,
    pub fingerprint: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckReport {
    pub id: String,
    /// complete, partial, cancelled, or failed
    pub execution: String,
    pub basis: String,
    /// validated, stale, or unknown; validation refers to the recorded snapshot.
    pub freshness: String,
    pub operator_version: String,
    pub workspace: String,
    pub generation: String,
    pub coverage: Coverage,
    pub items: Vec<ItemResult>,
    pub sources: Vec<SourceFingerprint>,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

/// Work that can run synchronously or under a run handle.
/// Control operations (`status`, `shutdown`, run queries) are not included.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkRequest {
    Check(CheckRequest),
    Evidence {
        id: String,
    },
    Compare {
        request: Value,
    },
    Replay {
        kind: ReplayKind,
        id: String,
    },
    Trace {
        request: Value,
    },
    ModelSetup {
        offline: bool,
    },
    ModelEvaluate {
        request: Value,
    },
    /// Body is a `semantic::SemanticCheckRequest`.
    Semantic {
        request: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Check(CheckRequest),
    Evidence {
        id: String,
    },
    /// A later page of a retained trace. Collection evidence stays on `Evidence`.
    TracePage {
        id: String,
        offset: usize,
        limit: usize,
    },
    Status,
    Shutdown,
    Compare {
        request: Value,
    },
    Replay {
        kind: ReplayKind,
        id: String,
    },
    Trace {
        request: Value,
    },
    ModelSetup {
        offline: bool,
    },
    ModelEvaluate {
        request: Value,
    },
    Semantic {
        request: Value,
    },
    RunStart {
        request: WorkRequest,
    },
    RunStatus {
        id: String,
    },
    RunCancel {
        id: String,
    },
}

impl Request {
    /// In-flight reuse is valid only for a deterministic collection check.
    /// Execution, replay, and model calls are not deduplicated.
    pub fn dedup_key(&self) -> Option<String> {
        match self {
            Self::Check(check) => serde_json::to_string(check).ok(),
            Self::RunStart {
                request: WorkRequest::Check(check),
            } => serde_json::to_string(check).ok(),
            _ => None,
        }
    }

    pub fn is_effectful(&self) -> bool {
        matches!(
            self,
            Self::Compare { .. }
                | Self::Replay { .. }
                | Self::Trace { .. }
                | Self::ModelSetup { .. }
                | Self::ModelEvaluate { .. }
                | Self::Semantic { .. }
                | Self::RunStart {
                    request: WorkRequest::Compare { .. }
                        | WorkRequest::Replay { .. }
                        | WorkRequest::Trace { .. }
                        | WorkRequest::ModelSetup { .. }
                        | WorkRequest::ModelEvaluate { .. }
                        | WorkRequest::Semantic { .. },
                }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayKind {
    Compare,
    Trace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Complete,
    Cancelled,
    Failed,
}

/// Handle returned by an asynchronous run. Sync CLI commands may await this internally.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSnapshot {
    pub id: String,
    pub state: RunState,
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn default_implementation() -> String {
    String::new()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRequest {
    pub version: u32,
    /// Sender crate version. Empty only for a peer that predates this field.
    #[serde(default = "default_implementation")]
    pub implementation: String,
    pub request: Request,
}

impl WireRequest {
    pub fn current(request: Request) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            implementation: IMPLEMENTATION_VERSION.to_string(),
            request,
        }
    }

    pub fn same_implementation(&self) -> bool {
        self.version == PROTOCOL_VERSION && self.implementation == IMPLEMENTATION_VERSION
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    pub version: u32,
    #[serde(default = "default_implementation")]
    pub implementation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WireResponse {
    pub fn success(value: impl Serialize) -> anyhow::Result<Self> {
        Ok(Self {
            version: PROTOCOL_VERSION,
            implementation: IMPLEMENTATION_VERSION.to_string(),
            result: Some(serde_json::to_value(value)?),
            error: None,
        })
    }
    pub fn failure(error: impl ToString) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            implementation: IMPLEMENTATION_VERSION.to_string(),
            result: None,
            error: Some(error.to_string()),
        }
    }
}
