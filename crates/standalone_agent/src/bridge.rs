//! Bridge between Warp's request model and the supervised Pi helper.
//!
//! The bridge owns one helper process and a small state machine per Warp
//! conversation:
//!
//! ```text
//! Warp request (user query)      -> exchange E1 -> helper turn.start   -> Pi prompt P
//! Pi pauses on brokered tools    -> tool.calls + turn.awaiting_tools   -> E1 settles (paused)
//! Warp request (tool results)    -> exchange E2 -> helper turn.resume  -> P continues
//! Pi finishes                    -> turn.completed                     -> E2 settles (run done)
//! ```
//!
//! This is what keeps "exchange settled" and "run settled" distinct: an
//! exchange is one Warp request/response stream; a run is one Pi prompt. A run
//! may span many exchanges.
//!
//! Every helper event is validated against the exact session/turn/exchange it
//! claims to belong to. Foreign, stale, duplicate, and out-of-order results are
//! reported as protocol errors and never delivered as tool results.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::helper::{HelperLaunchConfig, HelperOutput, HelperProcess};
use crate::protocol::{
    AgentUsage, Envelope, HelloOk, HelperCompaction, HelperEvent, HelperProviderAuth,
    HelperRetry, HelperSessionOpen, HelperToolResult, HelperToolResultStatus, ProtocolError,
    SessionOpened, ToolCallSpec, TurnIdentity,
};
use crate::secrets::SecretString;

/// Options for the Pi-side retry loop. Retries are configured once, in the
/// helper: the backend never multiplies them.
#[derive(Debug, Clone)]
pub struct RetryOptions {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl Default for RetryOptions {
    fn default() -> Self {
        Self { enabled: true, max_retries: 4, base_delay_ms: 500 }
    }
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub launch: HelperLaunchConfig,
    pub retry: RetryOptions,
    pub compaction_enabled: bool,
}

/// Everything the backend needs to open one Pi session.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Warp conversation id; also the Pi session identity in the protocol.
    pub conversation_id: String,
    pub working_dir: PathBuf,
    pub provider: crate::provider::ProviderProfile,
    pub api_key: Option<SecretString>,
    pub session_file: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub load_context_files: bool,
    pub max_context_file_bytes: u64,
    /// Fork-private data directory (the helper's HOME). Never `~/.pi`.
    pub data_dir: PathBuf,
    /// Warp task id to advertise in the first `CreateTask` event. The app layer
    /// generates one for brand-new conversations (the real Warp server does the
    /// same) and reuses it for every exchange in the conversation.
    pub task_id: Option<String>,
    /// Whether this conversation still needs a `CreateTask` upgrade. False when
    /// the client already has a server-backed task (for example after a restart
    /// or a previous exchange), in which case sending `CreateTask` again would
    /// fail with `UnexpectedUpgrade`.
    pub create_task: bool,
}

/// Adapter-neutral events for one exchange.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// Opens the exchange. `run_id` is stable for the whole Pi turn.
    Init { conversation_id: String, request_id: String, run_id: String },
    /// The client should upgrade the optimistic task to a server-backed task.
    CreateTask { task_id: String },
    TextDelta { message_id: String, delta: String },
    /// Full assistant message text (sent once per message, after the deltas).
    TextMessage { message_id: String, text: String },
    ToolCalls { calls: Vec<ToolCallSpec> },
    /// The exchange is complete but the run is still suspended on these tools.
    ExchangePaused { pending: Vec<String> },
    /// The run finished successfully.
    RunSettled { stop_reason: String, usage: Option<AgentUsage> },
    RunFailed { code: String, message: String, retryable: bool },
    RunCancelled { reason: String },
    /// Protocol violation or helper failure. The exchange settles as an error;
    /// the caller must not automatically retry the affected side effect.
    ProtocolError { code: String, message: String },
    /// Non-fatal helper diagnostics that are safe to surface.
    Diagnostic { level: String, message: String },
}

pub type TurnStream = mpsc::UnboundedReceiver<BridgeEvent>;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("helper process failed to start: {0}")]
    Spawn(String),
    #[error("helper protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("helper exited before completing the request (exit code {0:?}); stderr tail: {1}")]
    HelperExited(Option<i32>, String),
    #[error("no session open for conversation {0}")]
    SessionNotFound(String),
    #[error("session is already busy with another turn")]
    SessionBusy,
    #[error("helper rejected the request: {0}")]
    HelperRejected(String),
    #[error("provider profile is invalid: {0}")]
    Profile(#[from] crate::provider::ProfileError),
    #[error("secret error: {0}")]
    Secret(#[from] crate::secrets::SecretError),
    #[error("bridge shut down")]
    ShutDown,
}

struct TurnState {
    turn_id: String,
    /// Exchange currently attached to `stream`.
    exchange_id: String,
    /// Tool calls the model requested and Warp has not resolved yet.
    pending: BTreeMap<String, ToolCallSpec>,
    /// Tool call ids already delivered in this turn (duplicate detection).
    delivered: BTreeMap<String, ()>,
    stream: Option<mpsc::UnboundedSender<BridgeEvent>>,
}

struct SessionState {
    /// Kept for diagnostics and for the durable conversation/session mapping.
    #[allow(dead_code)]
    conversation_id: String,
    generation: u64,
    task_id: String,
    /// Provider profile id used to open the session (diagnostics only).
    #[allow(dead_code)]
    provider_id: String,
    /// Workspace root the session was opened in (diagnostics only).
    #[allow(dead_code)]
    working_dir: PathBuf,
    /// Existing Pi session file to restore, if any.
    #[allow(dead_code)]
    session_file: Option<PathBuf>,
    create_task: bool,
    create_task_sent: bool,
    turn: Option<TurnState>,
    open_ack: Option<oneshot::Sender<Result<SessionOpened, BridgeError>>>,
}

enum Command {
    Hello(oneshot::Sender<Result<HelloOk, BridgeError>>),
    OpenSession(Box<SessionSpec>, oneshot::Sender<Result<SessionOpened, BridgeError>>),
    StartTurn {
        conversation_id: String,
        prompt: String,
        reply: oneshot::Sender<Result<TurnStream, BridgeError>>,
    },
    ResumeTurn {
        conversation_id: String,
        results: Vec<HelperToolResult>,
        reply: oneshot::Sender<Result<TurnStream, BridgeError>>,
    },
    CancelTurn {
        conversation_id: String,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// Handle used by the Warp-side adapter.
pub struct StandaloneBridge {
    commands: mpsc::UnboundedSender<Command>,
    driver: Option<tokio::task::JoinHandle<()>>,
    config: Arc<BridgeConfig>,
}

impl StandaloneBridge {
    /// Spawn the helper and start the driver.
    pub async fn spawn(config: BridgeConfig) -> Result<Self, BridgeError> {
        let helper = HelperProcess::spawn(config.launch.clone())
            .await
            .map_err(|e| BridgeError::Spawn(e.to_string()))?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(driver_loop(helper, command_rx));
        Ok(Self { commands, driver: Some(driver), config: Arc::new(config) })
    }

    pub fn retry_options(&self) -> &RetryOptions {
        &self.config.retry
    }

    /// Protocol handshake. Fails on version/capability mismatch.
    pub async fn hello(&mut self) -> Result<HelloOk, BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands.send(Command::Hello(tx)).map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    pub async fn open_session(&mut self, spec: SessionSpec) -> Result<SessionOpened, BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::OpenSession(Box::new(spec), tx))
            .map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    /// Start a fresh Pi turn (a user query) and return this exchange's stream.
    pub async fn start_turn(
        &mut self,
        conversation_id: &str,
        prompt: String,
    ) -> Result<TurnStream, BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::StartTurn {
                conversation_id: conversation_id.to_string(),
                prompt,
                reply: tx,
            })
            .map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    /// Deliver tool results and return the continuation exchange's stream.
    pub async fn resume_turn(
        &mut self,
        conversation_id: &str,
        results: Vec<(String, HelperToolResultStatus, String)>,
    ) -> Result<TurnStream, BridgeError> {
        let results = results
            .into_iter()
            .map(|(tool_call_id, status, content)| HelperToolResult { tool_call_id, status, content })
            .collect();
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::ResumeTurn {
                conversation_id: conversation_id.to_string(),
                results,
                reply: tx,
            })
            .map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    pub async fn cancel_turn(&mut self, conversation_id: &str) -> Result<(), BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::CancelTurn { conversation_id: conversation_id.to_string(), reply: tx })
            .map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    pub async fn shutdown(&mut self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        if let Some(driver) = self.driver.take() {
            let _ = driver.await;
        }
    }
}

impl Drop for StandaloneBridge {
    fn drop(&mut self) {
        // Dropping the command channel ends the driver loop, which reaps the
        // helper process. `shutdown()` is preferred so the helper can exit
        // cooperatively.
    }
}

struct Driver {
    sessions: HashMap<String, SessionState>,
    seq: u64,
    hello: Option<Result<HelloOk, BridgeError>>,
    hello_waiters: Vec<oneshot::Sender<Result<HelloOk, BridgeError>>>,
}

impl Driver {
    fn next_seq(&mut self) -> u64 {
        let value = self.seq;
        self.seq += 1;
        value
    }

    async fn send_hello(&mut self, helper: &mut HelperProcess) -> Result<(), BridgeError> {
        let frame = Envelope::new("hello", self.next_seq()).with_data(serde_json::json!({}));
        helper.send(&frame).await.map_err(BridgeError::from)
    }

    fn resolve_hello(&mut self, result: Result<HelloOk, BridgeError>) {
        if self.hello.is_none() {
            self.hello = Some(result.clone());
        }
        for waiter in self.hello_waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
    }

    async fn open_session(
        &mut self,
        helper: &mut HelperProcess,
        spec: SessionSpec,
        reply: oneshot::Sender<Result<SessionOpened, BridgeError>>,
    ) {
        match self.open_session_inner(helper, &spec).await {
            Ok(()) => {
                if let Some(session) = self.sessions.get_mut(&spec.conversation_id) {
                    session.open_ack = Some(reply);
                } else {
                    let _ = reply.send(Err(BridgeError::SessionNotFound(spec.conversation_id)));
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn open_session_inner(
        &mut self,
        helper: &mut HelperProcess,
        spec: &SessionSpec,
    ) -> Result<(), BridgeError> {
        let api_key = spec.api_key.as_ref().map(|key| key.expose_secret().to_string());
        let provider = spec.provider.helper_config(api_key.as_deref())?;
        if matches!(provider.auth, HelperProviderAuth::None) && api_key.is_some() {
            return Err(BridgeError::HelperRejected(
                "auth=none profile must not resolve a credential".into(),
            ));
        }
        let generation = self
            .sessions
            .get(&spec.conversation_id)
            .map_or(0, |session| session.generation + 1);
        let pi_dir = spec.data_dir.join("pi");
        let payload = HelperSessionOpen {
            working_dir: spec.working_dir.to_string_lossy().into_owned(),
            agent_dir: pi_dir.join("agent").to_string_lossy().into_owned(),
            session_dir: pi_dir.join("sessions").to_string_lossy().into_owned(),
            session_file: spec.session_file.as_ref().map(|path| path.to_string_lossy().into_owned()),
            system_prompt: spec.system_prompt.clone(),
            load_context_files: spec.load_context_files,
            max_context_file_bytes: spec.max_context_file_bytes,
            provider,
            compaction: HelperCompaction { enabled: true, reserve_tokens: 0, keep_recent_tokens: 0 },
            retry: HelperRetry {
                enabled: false,
                max_retries: 0,
                base_delay_ms: 0,
            },
        };
        let data = serde_json::to_value(&payload)
            .map_err(|e| BridgeError::Protocol(ProtocolError::Invalid(e.to_string())))?;
        let exchange_id = uuid::Uuid::new_v4().to_string();
        let frame = Envelope::new("session.open", self.next_seq())
            .with_session(&spec.conversation_id, generation, &exchange_id)
            .with_data(data);
        helper.send(&frame).await?;
        self.sessions.insert(
            spec.conversation_id.clone(),
            SessionState {
                conversation_id: spec.conversation_id.clone(),
                generation,
                task_id: spec.task_id.clone().unwrap_or_default(),
                create_task: spec.create_task,
                provider_id: spec.provider.id.clone(),
                working_dir: spec.working_dir.clone(),
                session_file: spec.session_file.clone(),
                create_task_sent: false,
                turn: None,
                open_ack: None,
            },
        );
        Ok(())
    }

    async fn start_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        prompt: String,
        reply: oneshot::Sender<Result<TurnStream, BridgeError>>,
    ) {
        let session = match self.sessions.get(conversation_id) {
            Some(session) => session,
            None => {
                let _ = reply.send(Err(BridgeError::SessionNotFound(conversation_id.to_string())));
                return;
            }
        };
        if session.turn.is_some() {
            let _ = reply.send(Err(BridgeError::SessionBusy));
            return;
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        let exchange_id = uuid::Uuid::new_v4().to_string();
        let generation = session.generation;
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id: turn_id.clone(),
            exchange_id: exchange_id.clone(),
            generation,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(BridgeEvent::Init {
            conversation_id: conversation_id.to_string(),
            request_id: exchange_id.clone(),
            run_id: turn_id.clone(),
        });
        if session.create_task && !session.create_task_sent {
            let task_id = if session.task_id.is_empty() {
                conversation_id.to_string()
            } else {
                session.task_id.clone()
            };
            let _ = tx.send(BridgeEvent::CreateTask { task_id: task_id.clone() });
        }
        let frame = Envelope::new("turn.start", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "prompt": prompt }));
        if let Err(error) = helper.send(&frame).await {
            let _ = reply.send(Err(error.into()));
            return;
        }
        if let Some(session) = self.sessions.get_mut(conversation_id) {
            session.create_task_sent = true;
            session.turn = Some(TurnState {
                turn_id,
                exchange_id,
                pending: BTreeMap::new(),
                delivered: BTreeMap::new(),
                stream: Some(tx),
            });
        }
        let _ = reply.send(Ok(rx));
    }

    async fn resume_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        results: Vec<HelperToolResult>,
        reply: oneshot::Sender<Result<TurnStream, BridgeError>>,
    ) {
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            let _ = reply.send(Err(BridgeError::SessionNotFound(conversation_id.to_string())));
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            let _ = reply.send(Err(BridgeError::HelperRejected(
                "no active Pi turn to resume".to_string(),
            )));
            return;
        };
        let exchange_id = uuid::Uuid::new_v4().to_string();
        let turn_id = turn.turn_id.clone();
        let generation = session.generation;
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(BridgeEvent::Init {
            conversation_id: conversation_id.to_string(),
            request_id: exchange_id.clone(),
            run_id: turn_id.clone(),
        });
        let mut accepted = Vec::new();
        let mut unknown = Vec::new();
        let mut duplicates = Vec::new();
        for result in results {
            if turn.pending.contains_key(&result.tool_call_id) {
                turn.pending.remove(&result.tool_call_id);
                turn.delivered.insert(result.tool_call_id.clone(), ());
                accepted.push(result);
            } else if turn.delivered.contains_key(&result.tool_call_id) {
                duplicates.push(result.tool_call_id);
            } else {
                unknown.push(result.tool_call_id);
            }
        }
        if !unknown.is_empty() || !duplicates.is_empty() {
            turn.stream = Some(tx.clone());
            let mut parts = Vec::new();
            if !unknown.is_empty() {
                parts.push(format!("unknown: {}", unknown.join(", ")));
            }
            if !duplicates.is_empty() {
                parts.push(format!("already delivered in this turn: {}", duplicates.join(", ")));
            }
            let _ = tx.send(BridgeEvent::ProtocolError {
                code: "unknown_tool_result".to_string(),
                message: format!(
                    "tool results do not match pending tool calls in this session/turn ({})",
                    parts.join("; ")
                ),
            });
            let _ = reply.send(Ok(rx));
            return;
        }
        turn.exchange_id = exchange_id.clone();
        turn.stream = Some(tx);
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id,
            exchange_id,
            generation,
        };
        let serialized = accepted
            .iter()
            .map(|result| {
                serde_json::json!({
                    "tool_call_id": result.tool_call_id,
                    "status": result.status,
                    "content": result.content,
                })
            })
            .collect::<Vec<_>>();
        let frame = Envelope::new("turn.resume", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "results": serialized }));
        if let Err(error) = helper.send(&frame).await {
            let _ = reply.send(Err(error.into()));
            return;
        }
        let _ = reply.send(Ok(rx));
    }

    async fn cancel_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    ) {
        let Some(session) = self.sessions.get(conversation_id) else {
            let _ = reply.send(Err(BridgeError::SessionNotFound(conversation_id.to_string())));
            return;
        };
        let Some(turn) = session.turn.as_ref() else {
            let _ = reply.send(Ok(()));
            return;
        };
        // Cancel targets the exchange currently attached to the turn so the
        // helper's terminal events correlate with an exchange we are reading.
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id: turn.turn_id.clone(),
            exchange_id: turn.exchange_id.clone(),
            generation: session.generation,
        };
        let frame = Envelope::new("turn.cancel", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "reason": "cancelled by user" }));
        let _ = reply.send(helper.send(&frame).await.map_err(BridgeError::from));
    }

    fn handle_frame(&mut self, frame: Envelope) {
        let event = match HelperEvent::decode(&frame) {
            Ok(event) => event,
            Err(ProtocolError::UnknownKind(kind)) => {
                tracing::debug!(kind, "ignoring unknown helper event kind");
                return;
            }
            Err(error) => {
                let message = error.to_string();
                self.fail_current_stream("helper_protocol_error", &message);
                return;
            }
        };
        match event {
            HelperEvent::HelloOk(ok) => self.resolve_hello(Ok(ok)),
            HelperEvent::SessionOpened(opened) => {
                if let Some(session) = self.sessions.get_mut(&opened.session_id)
                    && let Some(ack) = session.open_ack.take()
                {
                    let _ = ack.send(Ok(opened));
                }
            }
            HelperEvent::TurnStarted => {}
            HelperEvent::AssistantDelta { message_id, text } => self.forward(
                &frame,
                BridgeEvent::TextDelta { message_id, delta: text },
            ),
            HelperEvent::AssistantMessage { message_id, text } => self.forward(
                &frame,
                BridgeEvent::TextMessage { message_id, text },
            ),
            HelperEvent::AssistantReasoning { .. } => {
                // Reasoning display is deferred: the v1 UI path does not render
                // thinking tokens (see PROVIDER_COMPATIBILITY.md).
            }
            HelperEvent::ToolCalls { calls } => {
                let session_id = frame.session_id.clone().unwrap_or_default();
                if let Some(turn) = self.active_turn_mut(&frame) {
                    for call in &calls {
                        if turn.pending.contains_key(&call.tool_call_id)
                            || turn.delivered.contains_key(&call.tool_call_id)
                        {
                            let message =
                                format!("duplicate tool call id from helper: {}", call.tool_call_id);
                            self.fail_stream_for(&session_id, "helper_protocol_error", &message);
                            return;
                        }
                        turn.pending.insert(call.tool_call_id.clone(), call.clone());
                    }
                }
                self.forward(&frame, BridgeEvent::ToolCalls { calls });
            }
            HelperEvent::TurnAwaitingTools { pending } => {
                if pending.is_empty() {
                    let session_id = frame.session_id.clone().unwrap_or_default();
                    self.fail_stream_for(
                        &session_id,
                        "helper_protocol_error",
                        "helper paused an exchange without pending tool calls",
                    );
                    return;
                }
                self.forward(&frame, BridgeEvent::ExchangePaused { pending });
            }
            HelperEvent::TurnCompleted { stop_reason, usage } => {
                self.forward(&frame, BridgeEvent::RunSettled { stop_reason, usage });
                self.clear_turn(&frame);
            }
            HelperEvent::TurnCancelling { .. } => {}
            HelperEvent::TurnCancelled { reason } => {
                self.forward(&frame, BridgeEvent::RunCancelled { reason });
                self.clear_turn(&frame);
            }
            HelperEvent::TurnFailed { code, message, retryable } => {
                self.forward(&frame, BridgeEvent::RunFailed { code, message, retryable });
                self.clear_turn(&frame);
            }
            HelperEvent::Rejected { code, message, .. } => {
                self.forward(&frame, BridgeEvent::ProtocolError { code, message });
            }
            HelperEvent::Diagnostic { level, message } => {
                self.forward(&frame, BridgeEvent::Diagnostic { level, message });
            }
            HelperEvent::CompactionStarted { .. } | HelperEvent::CompactionFinished { .. } => {}
            HelperEvent::ShutdownAck => {}
        }
    }

    fn active_turn_mut(&mut self, frame: &Envelope) -> Option<&mut TurnState> {
        let session_id = frame.session_id.as_deref()?;
        let turn_id = frame.turn_id.as_deref()?;
        let exchange_id = frame.exchange_id.as_deref()?;
        let session = self.sessions.get_mut(session_id)?;
        let turn = session.turn.as_mut()?;
        if turn.turn_id != turn_id || turn.exchange_id != exchange_id {
            return None;
        }
        Some(turn)
    }

    fn forward(&mut self, frame: &Envelope, event: BridgeEvent) {
        let Some(turn) = self.active_turn_mut(frame) else {
            tracing::debug!(kind = %frame.kind, turn_id = ?frame.turn_id, "dropping stale turn event");
            return;
        };
        let is_terminal = matches!(
            event,
            BridgeEvent::RunSettled { .. }
                | BridgeEvent::RunFailed { .. }
                | BridgeEvent::RunCancelled { .. }
                | BridgeEvent::ProtocolError { .. }
        );
        let stream = if is_terminal { turn.stream.take() } else { turn.stream.clone() };
        if let Some(stream) = stream
            && stream.send(event).is_err()
        {
            tracing::debug!("exchange stream dropped; event discarded");
        }
    }

    fn fail_stream_for(&mut self, session_id: &str, code: &str, message: &str) {
        if let Some(session) = self.sessions.get_mut(session_id)
            && let Some(stream) = session.turn.as_mut().and_then(|turn| turn.stream.take())
        {
            let _ = stream.send(BridgeEvent::ProtocolError {
                code: code.to_string(),
                message: message.to_string(),
            });
        }
    }

    fn fail_current_stream(&mut self, code: &str, message: &str) {
        for session in self.sessions.values_mut() {
            if let Some(stream) = session.turn.as_mut().and_then(|turn| turn.stream.take()) {
                let _ = stream.send(BridgeEvent::ProtocolError {
                    code: code.to_string(),
                    message: message.to_string(),
                });
            }
        }
    }

    fn clear_turn(&mut self, frame: &Envelope) {
        let Some(session_id) = frame.session_id.as_deref() else { return };
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.turn = None;
        }
    }

    fn fail_pending_opens(&mut self, error: BridgeError) {
        for session in self.sessions.values_mut() {
            if let Some(ack) = session.open_ack.take() {
                let _ = ack.send(Err(error.clone()));
            }
        }
    }
}

impl Clone for BridgeError {
    fn clone(&self) -> Self {
        match self {
            BridgeError::Spawn(message) => BridgeError::Spawn(message.clone()),
            BridgeError::Protocol(error) => BridgeError::Protocol(error.clone()),
            BridgeError::HelperExited(code, tail) => BridgeError::HelperExited(*code, tail.clone()),
            BridgeError::SessionNotFound(id) => BridgeError::SessionNotFound(id.clone()),
            BridgeError::SessionBusy => BridgeError::SessionBusy,
            BridgeError::HelperRejected(message) => BridgeError::HelperRejected(message.clone()),
            BridgeError::Profile(error) => BridgeError::Profile(error.clone()),
            BridgeError::Secret(error) => BridgeError::Secret(error.clone()),
            BridgeError::ShutDown => BridgeError::ShutDown,
        }
    }
}

async fn driver_loop(mut helper: HelperProcess, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut output_rx = helper.take_output();
    let mut driver = Driver { sessions: HashMap::new(), seq: 0, hello: None, hello_waiters: Vec::new() };
    if let Err(error) = driver.send_hello(&mut helper).await {
        driver.resolve_hello(Err(error));
    }
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Hello(reply) => match &driver.hello {
                        Some(result) => { let _ = reply.send(result.clone()); }
                        None => driver.hello_waiters.push(reply),
                    },
                    Command::OpenSession(spec, reply) => driver.open_session(&mut helper, *spec, reply).await,
                    Command::StartTurn { conversation_id, prompt, reply } => {
                        driver.start_turn(&mut helper, &conversation_id, prompt, reply).await;
                    }
                    Command::ResumeTurn { conversation_id, results, reply } => {
                        driver.resume_turn(&mut helper, &conversation_id, results, reply).await;
                    }
                    Command::CancelTurn { conversation_id, reply } => {
                        driver.cancel_turn(&mut helper, &conversation_id, reply).await;
                    }
                    Command::Shutdown(ack) => {
                        let error = BridgeError::ShutDown;
                        driver.resolve_hello(Err(error.clone()));
                        driver.fail_pending_opens(error);
                        driver.fail_current_stream("shutdown", "standalone bridge is shutting down");
                        helper.shutdown().await;
                        let _ = ack.send(());
                        break;
                    }
                }
            }
            output = output_rx.recv() => {
                let output = helper.observe(output);
                let Some(output) = output else { break };
                match output {
                    HelperOutput::Frame(frame) => driver.handle_frame(*frame),
                    HelperOutput::ProtocolError(error) => {
                        let message = error.to_string();
                        driver.fail_current_stream("helper_protocol_error", &message);
                    }
                    HelperOutput::Stderr(_) => {}
                    HelperOutput::Exited(code) => {
                        let tail = helper.stderr_tail().join("\n");
                        driver.resolve_hello(Err(BridgeError::HelperExited(code, tail.clone())));
                        driver.fail_pending_opens(BridgeError::HelperExited(code, tail.clone()));
                        driver.fail_current_stream(
                            "helper_exited",
                            &format!("helper exited unexpectedly (code {code:?})"),
                        );
                        break;
                    }
                }
            }
        }
    }
}

/// Fork-private application directory used by the helper for one workspace.
pub fn pi_data_dir(app_data_dir: &Path, workspace: &Path) -> PathBuf {
    let name = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    app_data_dir.join("pi-sessions").join(name)
}
