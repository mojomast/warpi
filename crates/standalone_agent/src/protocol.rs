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
pub const REQUIRED_HELPER_TOOLS: [&str; 6] = ["bash", "read", "write", "edit", "glob", "grep"];

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
            return Err(ProtocolError::Invalid("kind must be 1..=64 characters".into()));
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
            return Err(ProtocolError::OutOfOrder { received: envelope.seq, last });
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

#[derive(Debug, Clone, Deserialize)]
pub struct AgentUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallSpec {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Events the helper can emit. Unknown kinds are ignored with a diagnostic so
/// the helper can add fields/kinds without breaking older backends.
#[derive(Debug, Clone)]
pub enum HelperEvent {
    HelloOk(HelloOk),
    SessionOpened(SessionOpened),
    TurnStarted,
    AssistantDelta { message_id: String, text: String },
    AssistantMessage { message_id: String, text: String },
    AssistantReasoning { message_id: String, text: String },
    ToolCalls { calls: Vec<ToolCallSpec> },
    TurnAwaitingTools { pending: Vec<String> },
    TurnCompleted { stop_reason: String, usage: Option<AgentUsage> },
    TurnCancelling { reason: String },
    TurnCancelled { reason: String },
    TurnFailed { code: String, message: String, retryable: bool },
    CompactionStarted { reason: String },
    CompactionFinished { reason: String, summarized: bool },
    Diagnostic { level: String, message: String },
    ShutdownAck,
    /// The helper rejected our frame (non-terminal for the running turn).
    Rejected { code: String, message: String, retryable: bool },
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
    data.get(field).and_then(serde_json::Value::as_str).map(str::to_string)
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
                usage: data
                    .get("usage")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| ProtocolError::Invalid(format!("turn.completed usage: {e}")))?,
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
        let huge = format!(r#"{{"protocol":1,"seq":1,"kind":"x","data":"{}"}}"#, "a".repeat(MAX_FRAME_BYTES));
        assert!(matches!(Envelope::parse(&huge, None), Err(ProtocolError::TooLarge)));
        assert!(matches!(Envelope::parse("[]", None), Err(ProtocolError::NotAnObject)));
    }

    #[test]
    fn rejects_duplicate_tool_call_ids_in_a_batch() {
        let line = r#"{"protocol":1,"seq":1,"kind":"tool.calls","data":{"calls":[{"tool_call_id":"a","name":"workspace.read_file","arguments":{}},{"tool_call_id":"a","name":"workspace.read_file","arguments":{}}]}}"#;
        let envelope = Envelope::parse(line, None).expect("valid envelope");
        assert!(matches!(HelperEvent::decode(&envelope), Err(ProtocolError::Invalid(_))));
    }

    #[test]
    fn unknown_kinds_are_reported_for_forward_compatibility() {
        let line = r#"{"protocol":1,"seq":9,"kind":"future.event","data":{}}"#;
        let envelope = Envelope::parse(line, None).expect("valid envelope");
        assert!(matches!(HelperEvent::decode(&envelope), Err(ProtocolError::UnknownKind(_))));
    }
}
