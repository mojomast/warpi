//! Wire protocol (v1) between the standalone Rust backend and the supervised
//! Pi helper.
//!
//! The TypeScript mirror is `standalone/pi-helper/src/protocol.ts`; both files
//! must change together. Frames are newline-delimited JSON on the helper's
//! stdin/stdout. Only protocol frames travel on stdout; diagnostics are on
//! stderr.
//!
//! Invariants enforced here (and again in the helper):
//! - `protocol` must equal [`PROTOCOL_VERSION`].
//! - `seq` is monotonically increasing per direction.
//! - Session-scoped frames carry `session_id` and `generation`; turn-scoped
//!   frames additionally carry `turn_id` and `exchange_id`.
//! - Frames larger than [`MAX_FRAME_BYTES`] are rejected before parsing.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Protocol version. Bumped on incompatible frame changes.
pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum serialized frame size accepted from the helper.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Maximum identifier length.
pub const MAX_ID_LENGTH: usize = 256;

/// Helpers implemented by the pinned Pi SDK version we ship.
pub const REQUIRED_HELPER_TOOLS: [&str; 7] = [
    "bash",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "bash_output",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("frame is not a JSON object")]
    NotAnObject,
    #[error("protocol mismatch: expected {expected}, received {received}")]
    VersionMismatch { expected: u32, received: u32 },
    #[error("invalid frame: {0}")]
    Invalid(String),
    #[error("frame exceeds the {MAX_FRAME_BYTES} byte limit")]
    TooLarge,
    #[error("out-of-order or duplicate helper sequence {received} (last {last})")]
    OutOfOrder { received: u64, last: u64 },
    #[error("unknown frame kind: {0}")]
    UnknownKind(String),
}

/// Raw envelope shared by both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: u32,
    pub seq: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Envelope {
    pub fn new(kind: &str, seq: u64) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            seq,
            kind: kind.to_string(),
            session_id: None,
            turn_id: None,
            exchange_id: None,
            generation: None,
            data: None,
        }
    }

    pub fn with_identity(mut self, identity: &TurnIdentity) -> Self {
        self.session_id = Some(identity.session_id.clone());
        self.turn_id = Some(identity.turn_id.clone());
        self.exchange_id = Some(identity.exchange_id.clone());
        self.generation = Some(identity.generation);
        self
    }

    pub fn with_session(mut self, session_id: &str, generation: u64, exchange_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self.generation = Some(generation);
        self.exchange_id = Some(exchange_id.to_string());
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Parse and validate one inbound helper frame.
    ///
    /// `last_seq` is the previous sequence number seen from the helper; frames
    /// must strictly increase it.
    pub fn parse(frame: &str, last_seq: Option<u64>) -> Result<Self, ProtocolError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        let value: serde_json::Value =
            serde_json::from_str(frame).map_err(|e| ProtocolError::InvalidJson(e.to_string()))?;
        let serde_json::Value::Object(_) = value else {
            return Err(ProtocolError::NotAnObject);
        };
        let envelope: Envelope =
            serde_json::from_value(value).map_err(|e| ProtocolError::Invalid(e.to_string()))?;
        if envelope.protocol != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                received: envelope.protocol,
            });
        }
        if envelope.kind.is_empty() || envelope.kind.len() > 64 {
            return Err(ProtocolError::Invalid(
                "kind must be 1..=64 characters".into(),
            ));
        }
        for (name, value) in [
            ("session_id", &envelope.session_id),
            ("turn_id", &envelope.turn_id),
            ("exchange_id", &envelope.exchange_id),
        ] {
            if let Some(value) = value
                && (value.is_empty() || value.len() > MAX_ID_LENGTH)
            {
                return Err(ProtocolError::Invalid(format!(
                    "{name} must be a non-empty string of at most {MAX_ID_LENGTH} characters"
                )));
            }
        }
        if let Some(last) = last_seq
            && envelope.seq <= last
        {
            return Err(ProtocolError::OutOfOrder {
                received: envelope.seq,
                last,
            });
        }
        Ok(envelope)
    }
}

/// Identity attached to every turn-scoped frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnIdentity {
    pub session_id: String,
    pub turn_id: String,
    pub exchange_id: String,
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Helper -> backend events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct HelperCapabilities {
    pub protocol: u32,
    pub brokered_tools: Vec<String>,
    pub compaction: bool,
    pub cancellation: bool,
    pub context_files: bool,
    /// The helper can host read-only child sessions when a session opens with an
    /// enabled `subagents` config. Absent from older helpers, so it defaults off.
    #[serde(default)]
    pub subagents: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloOk {
    pub helper_version: String,
    pub node_version: String,
    pub capabilities: HelperCapabilities,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionOpened {
    #[serde(default)]
    pub resumed: bool,
    #[serde(default)]
    pub session_file: Option<String>,
    pub session_id: String,
    pub active_tools: Vec<String>,
    pub model_id: String,
    #[serde(default)]
    pub working_dir: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AgentUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    /// Reported subset of `output_tokens`, never billed on top of it.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// SDK-reported USD cost. Currently all zeros (standalone profiles register
    /// zero rates); the local pricing table is authoritative.
    #[serde(default)]
    pub cost: Option<AgentUsageCost>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AgentUsageCost {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub total: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallSpec {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Live token counters carried by a `task.progress` heartbeat. Every field is
/// optional on the wire: a missing or malformed counter reads as zero instead of
/// failing the frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskProgressTokens {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Events the helper can emit. Unknown kinds are ignored with a diagnostic so
/// the helper can add fields/kinds without breaking older backends.
#[derive(Debug, Clone)]
pub enum HelperEvent {
    HelloOk(HelloOk),
    SessionOpened(SessionOpened),
    TurnStarted,
    AssistantDelta {
        message_id: String,
        text: String,
    },
    AssistantMessage {
        message_id: String,
        text: String,
    },
    AssistantReasoning {
        message_id: String,
        text: String,
    },
    ToolCalls {
        calls: Vec<ToolCallSpec>,
    },
    TurnAwaitingTools {
        pending: Vec<String>,
    },
    TurnCompleted {
        stop_reason: String,
        usage: Option<AgentUsage>,
    },
    /// One per assistant message, including tool-call-only and errored
    /// messages. The app prices it and appends a ledger record.
    AssistantUsage {
        message_id: String,
        model_id: String,
        usage: AgentUsage,
        duration_ms: u64,
        first_token_ms: Option<u64>,
        output_tokens_per_second: Option<f64>,
        stop_reason: String,
    },
    /// A `task` tool call started a read-only child session. Diagnostic-level:
    /// it carries the child attribution ids but no native UI representation yet.
    TaskStarted {
        task_id: String,
        child_session_id: String,
        description: String,
        subagent_type: String,
        prompt_bytes: u64,
        max_turns: u64,
        deadline_ms: u64,
        token_cap: u64,
    },
    /// Liveness heartbeat while a child runs. The parent exchange emits no
    /// provider events during a child, so this refreshes the stall watchdog.
    TaskProgress {
        task_id: String,
        child_session_id: String,
        elapsed_ms: u64,
        turns: u64,
        tool_calls: u64,
        tokens: TaskProgressTokens,
        pending_tools: u64,
    },
    /// Terminal child outcome. `usage` is the child's own token usage, kept
    /// separate from the parent's so the ledger never double-bills it.
    TaskCompleted {
        task_id: String,
        child_session_id: String,
        status: String,
        reason: Option<String>,
        subagent_type: String,
        turns: u64,
        tool_calls: u64,
        usage: AgentUsage,
        wall_ms: u64,
        summary_bytes: u64,
    },
    /// A context-window reading, after a model call or a compaction.
    ContextUpdated {
        tokens: Option<u64>,
        context_window: Option<u64>,
        percent: Option<f64>,
        source: String,
    },
    TurnCancelling {
        reason: String,
    },
    TurnCancelled {
        reason: String,
    },
    TurnFailed {
        code: String,
        message: String,
        retryable: bool,
    },
    CompactionStarted {
        reason: String,
    },
    CompactionFinished {
        reason: String,
        summarized: bool,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        summary_usage: Option<AgentUsage>,
        duration_ms: Option<u64>,
    },
    Diagnostic {
        level: String,
        message: String,
    },
    ShutdownAck,
    /// The helper rejected our frame (non-terminal for the running turn).
    Rejected {
        code: String,
        message: String,
        retryable: bool,
    },
}

impl HelperEvent {
    /// The helper's own view of the session this event belongs to.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            HelperEvent::SessionOpened(opened) => Some(&opened.session_id),
            _ => None,
        }
    }
}

fn required_str(data: &serde_json::Value, field: &str) -> Result<String, ProtocolError> {
    data.get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_ID_LENGTH)
        .map(str::to_string)
        .ok_or_else(|| ProtocolError::Invalid(format!("{field} must be a non-empty string")))
}

fn optional_str(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn optional_u64(data: &serde_json::Value, field: &str) -> Option<u64> {
    data.get(field).and_then(serde_json::Value::as_u64)
}

/// A finite non-negative float, or `None` for anything else (a malformed
/// reading must not fail the frame or render as a number).
fn optional_f64(data: &serde_json::Value, field: &str) -> Option<f64> {
    data.get(field)
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn optional_usage(
    data: &serde_json::Value,
    field: &str,
) -> Result<Option<AgentUsage>, ProtocolError> {
    data.get(field)
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| ProtocolError::Invalid(format!("{field}: {e}")))
}

/// Nested task-progress tokens. Unlike `usage`, a malformed object is dropped
/// to zeros rather than failing the heartbeat frame.
fn optional_tokens(data: &serde_json::Value, field: &str) -> TaskProgressTokens {
    data.get(field)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

impl HelperEvent {
    /// Decode a validated envelope into a typed event.
    pub fn decode(envelope: &Envelope) -> Result<Self, ProtocolError> {
        let empty = serde_json::Value::Object(serde_json::Map::new());
        let data = envelope.data.as_ref().unwrap_or(&empty);
        let event = match envelope.kind.as_str() {
            "hello.ok" => HelperEvent::HelloOk(
                serde_json::from_value(data.clone())
                    .map_err(|e| ProtocolError::Invalid(format!("hello.ok: {e}")))?,
            ),
            "session.opened" => HelperEvent::SessionOpened(
                serde_json::from_value(data.clone())
                    .map_err(|e| ProtocolError::Invalid(format!("session.opened: {e}")))?,
            ),
            "turn.started" => HelperEvent::TurnStarted,
            "assistant.delta" => HelperEvent::AssistantDelta {
                message_id: required_str(data, "message_id")?,
                text: optional_str(data, "text").unwrap_or_default(),
            },
            "assistant.message" => HelperEvent::AssistantMessage {
                message_id: required_str(data, "message_id")?,
                text: optional_str(data, "text").unwrap_or_default(),
            },
            "assistant.reasoning" => HelperEvent::AssistantReasoning {
                message_id: required_str(data, "message_id")?,
                text: optional_str(data, "text").unwrap_or_default(),
            },
            "tool.calls" => {
                let calls: Vec<ToolCallSpec> = data
                    .get("calls")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| ProtocolError::Invalid(format!("tool.calls: {e}")))?
                    .unwrap_or_default();
                let mut seen = BTreeSet::new();
                for call in &calls {
                    if call.tool_call_id.is_empty() || call.tool_call_id.len() > MAX_ID_LENGTH {
                        return Err(ProtocolError::Invalid(format!(
                            "tool.calls: invalid tool_call_id {:?}",
                            call.tool_call_id
                        )));
                    }
                    if !seen.insert(call.tool_call_id.clone()) {
                        return Err(ProtocolError::Invalid(format!(
                            "tool.calls: duplicate tool_call_id {}",
                            call.tool_call_id
                        )));
                    }
                }
                HelperEvent::ToolCalls { calls }
            }
            "turn.awaiting_tools" => HelperEvent::TurnAwaitingTools {
                pending: data
                    .get("pending")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "turn.completed" => HelperEvent::TurnCompleted {
                stop_reason: optional_str(data, "stop_reason").unwrap_or_else(|| "stop".into()),
                usage: optional_usage(data, "usage")?,
            },
            "assistant.usage" => HelperEvent::AssistantUsage {
                message_id: required_str(data, "message_id")?,
                model_id: optional_str(data, "model_id").unwrap_or_default(),
                usage: optional_usage(data, "usage")?.unwrap_or_default(),
                duration_ms: optional_u64(data, "duration_ms").unwrap_or(0),
                first_token_ms: optional_u64(data, "first_token_ms"),
                output_tokens_per_second: optional_f64(data, "output_tokens_per_second"),
                stop_reason: optional_str(data, "stop_reason").unwrap_or_else(|| "stop".into()),
            },
            // Child-task events are decoded defensively: every field is optional
            // so a future helper can add fields without breaking older backends.
            "task.started" => HelperEvent::TaskStarted {
                task_id: optional_str(data, "task_id").unwrap_or_default(),
                child_session_id: optional_str(data, "child_session_id").unwrap_or_default(),
                description: optional_str(data, "description").unwrap_or_default(),
                subagent_type: optional_str(data, "subagent_type").unwrap_or_default(),
                prompt_bytes: optional_u64(data, "prompt_bytes").unwrap_or(0),
                max_turns: optional_u64(data, "max_turns").unwrap_or(0),
                deadline_ms: optional_u64(data, "deadline_ms").unwrap_or(0),
                token_cap: optional_u64(data, "token_cap").unwrap_or(0),
            },
            "task.progress" => HelperEvent::TaskProgress {
                task_id: optional_str(data, "task_id").unwrap_or_default(),
                child_session_id: optional_str(data, "child_session_id").unwrap_or_default(),
                elapsed_ms: optional_u64(data, "elapsed_ms").unwrap_or(0),
                turns: optional_u64(data, "turns").unwrap_or(0),
                tool_calls: optional_u64(data, "tool_calls").unwrap_or(0),
                tokens: optional_tokens(data, "tokens"),
                pending_tools: optional_u64(data, "pending_tools").unwrap_or(0),
            },
            "task.completed" => HelperEvent::TaskCompleted {
                task_id: optional_str(data, "task_id").unwrap_or_default(),
                child_session_id: optional_str(data, "child_session_id").unwrap_or_default(),
                status: optional_str(data, "status").unwrap_or_else(|| "error".into()),
                reason: optional_str(data, "reason"),
                subagent_type: optional_str(data, "subagent_type").unwrap_or_default(),
                turns: optional_u64(data, "turns").unwrap_or(0),
                tool_calls: optional_u64(data, "tool_calls").unwrap_or(0),
                usage: optional_usage(data, "usage")?.unwrap_or_default(),
                wall_ms: optional_u64(data, "wall_ms").unwrap_or(0),
                summary_bytes: optional_u64(data, "summary_bytes").unwrap_or(0),
            },
            "context.updated" => HelperEvent::ContextUpdated {
                tokens: optional_u64(data, "tokens"),
                context_window: optional_u64(data, "context_window"),
                percent: optional_f64(data, "percent"),
                source: optional_str(data, "source").unwrap_or_default(),
            },
            "turn.cancelling" => HelperEvent::TurnCancelling {
                reason: optional_str(data, "reason").unwrap_or_default(),
            },
            "turn.cancelled" => HelperEvent::TurnCancelled {
                reason: optional_str(data, "reason").unwrap_or_default(),
            },
            "turn.failed" => HelperEvent::TurnFailed {
                code: optional_str(data, "code").unwrap_or_else(|| "unknown".into()),
                message: optional_str(data, "message").unwrap_or_default(),
                retryable: data
                    .get("retryable")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            },
            "compaction.started" => HelperEvent::CompactionStarted {
                reason: optional_str(data, "reason").unwrap_or_default(),
            },
            "compaction.finished" => HelperEvent::CompactionFinished {
                reason: optional_str(data, "reason").unwrap_or_default(),
                summarized: data
                    .get("summarized")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                tokens_before: optional_u64(data, "tokens_before"),
                tokens_after: optional_u64(data, "tokens_after"),
                summary_usage: optional_usage(data, "summary_usage")?,
                duration_ms: optional_u64(data, "duration_ms"),
            },
            "diagnostic" => HelperEvent::Diagnostic {
                level: optional_str(data, "level").unwrap_or_else(|| "info".into()),
                message: optional_str(data, "message").unwrap_or_default(),
            },
            "shutdown.ack" => HelperEvent::ShutdownAck,
            "error" => HelperEvent::Rejected {
                code: optional_str(data, "code").unwrap_or_else(|| "unknown".into()),
                message: optional_str(data, "message").unwrap_or_default(),
                retryable: data
                    .get("retryable")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            },
            other => return Err(ProtocolError::UnknownKind(other.to_string())),
        };
        Ok(event)
    }
}

// ---------------------------------------------------------------------------
// Backend -> helper payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HelperCredential {
    None,
    ApiKey { api_key: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct HelperProviderConfig {
    pub provider_id: String,
    pub name: String,
    pub base_url: String,
    pub api: String,
    pub auth: HelperProviderAuth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    pub model_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub supports_image_input: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<serde_json::Value>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HelperProviderAuth {
    None,
    ApiKey { api_key: String },
}

impl std::fmt::Debug for HelperProviderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HelperProviderAuth::None => f.write_str("None"),
            HelperProviderAuth::ApiKey { .. } => f.write_str("ApiKey(redacted)"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HelperSessionOpen {
    pub working_dir: String,
    pub agent_dir: String,
    pub session_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub load_context_files: bool,
    pub max_context_file_bytes: u64,
    pub provider: HelperProviderConfig,
    pub compaction: HelperCompaction,
    pub retry: HelperRetry,
    /// Opt-in helper-internal subagents. Absent (the default) keeps the `task`
    /// tool unregistered and every existing conversation byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagents: Option<HelperSubagents>,
}

/// Session-open configuration for the helper's read-only `task` tool. The
/// helper only registers the tool when `enabled` is true and it advertised the
/// `subagents` capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperSubagents {
    #[serde(default)]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<HelperSubagentBudget>,
}

/// Per-child budgets for the `task` tool; omitted fields keep the helper's
/// defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperSubagentBudget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_token_cap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HelperCompaction {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HelperRetry {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HelperToolResultStatus {
    Success,
    Rejected,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct HelperToolResult {
    pub tool_call_id: String,
    pub status: HelperToolResultStatus,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_event_envelope() {
        let line = r#"{"protocol":1,"seq":4,"kind":"assistant.delta","session_id":"s","turn_id":"t","exchange_id":"e","generation":1,"data":{"message_id":"m","text":"hi"}}"#;
        let envelope = Envelope::parse(line, Some(3)).expect("valid frame");
        assert_eq!(envelope.seq, 4);
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::AssistantDelta { message_id, text } => {
                assert_eq!(message_id, "m");
                assert_eq!(text, "hi");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn rejects_version_mismatch_and_out_of_order_frames() {
        let wrong_version = r#"{"protocol":2,"seq":1,"kind":"hello.ok"}"#;
        assert!(matches!(
            Envelope::parse(wrong_version, None),
            Err(ProtocolError::VersionMismatch { .. })
        ));
        let duplicate = r#"{"protocol":1,"seq":3,"kind":"turn.started"}"#;
        assert!(matches!(
            Envelope::parse(duplicate, Some(3)),
            Err(ProtocolError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn rejects_oversized_and_non_object_frames() {
        let huge = format!(
            r#"{{"protocol":1,"seq":1,"kind":"x","data":"{}"}}"#,
            "a".repeat(MAX_FRAME_BYTES)
        );
        assert!(matches!(
            Envelope::parse(&huge, None),
            Err(ProtocolError::TooLarge)
        ));
        assert!(matches!(
            Envelope::parse("[]", None),
            Err(ProtocolError::NotAnObject)
        ));
    }

    #[test]
    fn rejects_duplicate_tool_call_ids_in_a_batch() {
        let line = r#"{"protocol":1,"seq":1,"kind":"tool.calls","data":{"calls":[{"tool_call_id":"a","name":"workspace.read_file","arguments":{}},{"tool_call_id":"a","name":"workspace.read_file","arguments":{}}]}}"#;
        let envelope = Envelope::parse(line, None).expect("valid envelope");
        assert!(matches!(
            HelperEvent::decode(&envelope),
            Err(ProtocolError::Invalid(_))
        ));
    }

    #[test]
    fn unknown_kinds_are_reported_for_forward_compatibility() {
        let line = r#"{"protocol":1,"seq":9,"kind":"future.event","data":{}}"#;
        let envelope = Envelope::parse(line, None).expect("valid envelope");
        assert!(matches!(
            HelperEvent::decode(&envelope),
            Err(ProtocolError::UnknownKind(_))
        ));
    }

    #[test]
    fn decodes_assistant_usage_with_and_without_optionals() {
        let full = r#"{"protocol":1,"seq":1,"kind":"assistant.usage","data":{"message_id":"m1","model_id":"deepseek-flash","usage":{"input_tokens":160,"output_tokens":56,"cache_read_tokens":1280,"cache_write_tokens":0,"reasoning_tokens":13,"total_tokens":1496,"cost":{"input":0.1,"output":0.2,"cache_read":0.3,"cache_write":0.4,"total":0.5}},"duration_ms":812,"first_token_ms":210,"output_tokens_per_second":68.9,"stop_reason":"stop"}}"#;
        let envelope = Envelope::parse(full, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::AssistantUsage {
                message_id,
                model_id,
                usage,
                duration_ms,
                first_token_ms,
                output_tokens_per_second,
                stop_reason,
            } => {
                assert_eq!(message_id, "m1");
                assert_eq!(model_id, "deepseek-flash");
                assert_eq!(usage.input_tokens, 160);
                assert_eq!(usage.output_tokens, 56);
                assert_eq!(usage.cache_read_tokens, Some(1280));
                assert_eq!(usage.reasoning_tokens, Some(13));
                assert_eq!(usage.total_tokens, Some(1496));
                assert_eq!(usage.cost.expect("cost").total, 0.5);
                assert_eq!(duration_ms, 812);
                assert_eq!(first_token_ms, Some(210));
                assert_eq!(output_tokens_per_second, Some(68.9));
                assert_eq!(stop_reason, "stop");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let minimal = r#"{"protocol":1,"seq":2,"kind":"assistant.usage","data":{"message_id":"m2","usage":{"input_tokens":1,"output_tokens":2}}}"#;
        let envelope = Envelope::parse(minimal, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::AssistantUsage {
                usage,
                duration_ms,
                first_token_ms,
                output_tokens_per_second,
                stop_reason,
                ..
            } => {
                assert_eq!(usage.reasoning_tokens, None);
                assert_eq!(usage.cost, None);
                assert_eq!(usage.total_tokens, None);
                assert_eq!(duration_ms, 0);
                assert_eq!(first_token_ms, None);
                assert_eq!(output_tokens_per_second, None);
                assert_eq!(stop_reason, "stop");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decodes_context_updated_and_drops_malformed_readings() {
        let line = r#"{"protocol":1,"seq":3,"kind":"context.updated","data":{"tokens":null,"percent":null,"source":"usage"}}"#;
        let envelope = Envelope::parse(line, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::ContextUpdated {
                tokens,
                context_window,
                percent,
                source,
            } => {
                assert_eq!(tokens, None);
                assert_eq!(context_window, None);
                assert_eq!(percent, None);
                assert_eq!(source, "usage");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let malformed = r#"{"protocol":1,"seq":4,"kind":"context.updated","data":{"tokens":7937,"context_window":131072,"percent":"6.06","source":"estimate"}}"#;
        let envelope = Envelope::parse(malformed, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::ContextUpdated {
                tokens,
                context_window,
                percent,
                ..
            } => {
                assert_eq!(tokens, Some(7937));
                assert_eq!(context_window, Some(131072));
                assert_eq!(percent, None, "a malformed percent must not be rendered");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decodes_extended_compaction_finished_with_missing_optionals() {
        let minimal = r#"{"protocol":1,"seq":5,"kind":"compaction.finished","data":{"reason":"threshold","summarized":true}}"#;
        let envelope = Envelope::parse(minimal, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::CompactionFinished {
                reason,
                summarized,
                tokens_before,
                tokens_after,
                summary_usage,
                duration_ms,
            } => {
                assert_eq!(reason, "threshold");
                assert!(summarized);
                assert_eq!(tokens_before, None);
                assert_eq!(tokens_after, None);
                assert!(summary_usage.is_none());
                assert_eq!(duration_ms, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let full = r#"{"protocol":1,"seq":6,"kind":"compaction.finished","data":{"reason":"threshold","summarized":true,"tokens_before":90000,"tokens_after":12000,"summary_usage":{"input_tokens":90000,"output_tokens":512,"reasoning_tokens":8},"duration_ms":4200}}"#;
        let envelope = Envelope::parse(full, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::CompactionFinished {
                tokens_before,
                tokens_after,
                summary_usage,
                duration_ms,
                ..
            } => {
                assert_eq!(tokens_before, Some(90_000));
                assert_eq!(tokens_after, Some(12_000));
                let usage = summary_usage.expect("summary usage");
                assert_eq!(usage.reasoning_tokens, Some(8));
                assert_eq!(duration_ms, Some(4200));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn decodes_task_events_with_every_optional_field_present() {
        let started = r#"{"protocol":1,"seq":10,"kind":"task.started","data":{"task_id":"task-1","child_session_id":"s:task-1","description":"explore auth","subagent_type":"explore","prompt_bytes":1234,"max_turns":12,"deadline_ms":600000,"token_cap":200000}}"#;
        let envelope = Envelope::parse(started, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::TaskStarted {
                task_id,
                child_session_id,
                description,
                subagent_type,
                prompt_bytes,
                max_turns,
                deadline_ms,
                token_cap,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(child_session_id, "s:task-1");
                assert_eq!(description, "explore auth");
                assert_eq!(subagent_type, "explore");
                assert_eq!(prompt_bytes, 1234);
                assert_eq!(max_turns, 12);
                assert_eq!(deadline_ms, 600_000);
                assert_eq!(token_cap, 200_000);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let progress = r#"{"protocol":1,"seq":11,"kind":"task.progress","data":{"task_id":"task-1","child_session_id":"s:task-1","elapsed_ms":30000,"turns":3,"tool_calls":5,"tokens":{"input_tokens":4000,"output_tokens":300,"total_tokens":4300},"pending_tools":1}}"#;
        let envelope = Envelope::parse(progress, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::TaskProgress {
                task_id,
                elapsed_ms,
                tokens,
                pending_tools,
                ..
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(elapsed_ms, 30_000);
                assert_eq!(tokens.input_tokens, 4_000);
                assert_eq!(tokens.total_tokens, 4_300);
                assert_eq!(pending_tools, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let completed = r#"{"protocol":1,"seq":12,"kind":"task.completed","data":{"task_id":"task-1","child_session_id":"s:task-1","status":"ok","subagent_type":"verify","turns":7,"tool_calls":9,"usage":{"input_tokens":41230,"output_tokens":3800,"total_tokens":45030},"wall_ms":92000,"summary_bytes":2048}}"#;
        let envelope = Envelope::parse(completed, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::TaskCompleted {
                status,
                reason,
                subagent_type,
                usage,
                wall_ms,
                summary_bytes,
                ..
            } => {
                assert_eq!(status, "ok");
                assert_eq!(reason, None);
                assert_eq!(subagent_type, "verify");
                assert_eq!(usage.input_tokens, 41_230);
                assert_eq!(usage.total_tokens, Some(45_030));
                assert_eq!(wall_ms, 92_000);
                assert_eq!(summary_bytes, 2048);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn task_events_tolerate_missing_and_unknown_fields() {
        // A minimal `task.started`: no ids, no budgets, plus a field this
        // backend does not know about.
        let started =
            r#"{"protocol":1,"seq":20,"kind":"task.started","data":{"future_field":true}}"#;
        let envelope = Envelope::parse(started, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("unknown fields never fail a task frame") {
            HelperEvent::TaskStarted {
                task_id,
                subagent_type,
                max_turns,
                ..
            } => {
                assert!(task_id.is_empty());
                assert!(subagent_type.is_empty());
                assert_eq!(max_turns, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // A malformed nested `tokens` object reads as zeros, not an error.
        let progress = r#"{"protocol":1,"seq":21,"kind":"task.progress","data":{"task_id":"t","tokens":{"input_tokens":"lots"}}}"#;
        let envelope = Envelope::parse(progress, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::TaskProgress { tokens, .. } => {
                assert_eq!(tokens, TaskProgressTokens::default());
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // A terminal event with no usage still decodes; status falls back to a
        // conservative error rather than claiming success.
        let completed = r#"{"protocol":1,"seq":22,"kind":"task.completed","data":{}}"#;
        let envelope = Envelope::parse(completed, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::TaskCompleted {
                status,
                usage,
                reason,
                ..
            } => {
                assert_eq!(status, "error");
                assert_eq!(usage, AgentUsage::default());
                assert_eq!(reason, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn helper_capabilities_default_subagents_off_when_absent() {
        let line = r#"{"protocol":1,"seq":1,"kind":"hello.ok","data":{"helper_version":"0.1.0","node_version":"22.19.0","capabilities":{"protocol":1,"brokered_tools":["bash"],"compaction":true,"cancellation":true,"context_files":true}}}"#;
        let envelope = Envelope::parse(line, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::HelloOk(ok) => assert!(!ok.capabilities.subagents),
            other => panic!("unexpected event: {other:?}"),
        }

        let capable = line.replace(
            "\"context_files\":true",
            "\"context_files\":true,\"subagents\":true",
        );
        let envelope = Envelope::parse(&capable, None).expect("valid frame");
        match HelperEvent::decode(&envelope).expect("decodes") {
            HelperEvent::HelloOk(ok) => assert!(ok.capabilities.subagents),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
