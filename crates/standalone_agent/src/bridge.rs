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
//!
//! Liveness guarantees:
//! - Every tool call forwarded to the app receives exactly one result. Warp
//!   drops a cancelled command's result during request conversion, so the
//!   bridge answers every call the app did not answer at the next
//!   `turn.resume`; calls the executor cannot represent are answered in place;
//!   and a turn that stays suspended past the pending-tool deadline is
//!   cancelled instead of parking forever.
//! - Every turn the bridge cancels settles. After `turn.cancel` the helper has
//!   a bounded window to acknowledge; the bridge synthesizes the terminal event
//!   afterwards so the session slot is always released.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use instant::Instant;
use tokio::sync::{mpsc, oneshot};
use warp_multi_agent_api as api;

use crate::helper::{HelperLaunchConfig, HelperOutput, HelperProcess};
use crate::protocol::{
    AgentUsage, Envelope, HelloOk, HelperCompaction, HelperEvent, HelperProviderAuth, HelperRetry,
    HelperSessionOpen, HelperSubagents, HelperToolResult, HelperToolResultStatus, MAX_FRAME_BYTES,
    ProtocolError, SessionOpened, TaskProgressTokens, ToolCallSpec, TurnIdentity,
};
use crate::provider::ProviderProfile;
use crate::secrets::SecretString;
use crate::warp_events::{ToolTranslationError, translate_tool_call};

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
        Self {
            enabled: true,
            max_retries: 4,
            base_delay_ms: 500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub launch: HelperLaunchConfig,
    pub retry: RetryOptions,
    pub compaction_enabled: bool,
    pub timeouts: BridgeTimeouts,
}

/// Watchdog deadlines, one set per driver. All are per turn; the defaults come
/// from the environment so a long-running approval can be matched to the
/// deployment without a rebuild.
#[derive(Debug, Clone)]
pub struct BridgeTimeouts {
    /// A turn whose provider produces no events for this long is cancelled.
    pub stall: Duration,
    /// A turn suspended on tool calls for this long is cancelled with
    /// `tool_timeout` and a retryable failure. `None` keeps the native
    /// unbounded wait for an approval that is still on screen.
    pub pending_tools: Option<Duration>,
    /// How long the helper has to acknowledge `turn.cancel` before the bridge
    /// synthesizes the terminal event itself.
    pub cancel: Duration,
    /// How long one compaction may run before the turn is treated as stalled.
    /// Compaction is a long provider call inside the turn that emits only its
    /// start and finish, so it gets its own, longer deadline.
    pub compaction: Duration,
    /// How often the watchdog checks the deadlines above.
    pub watchdog_tick: Duration,
}

impl Default for BridgeTimeouts {
    fn default() -> Self {
        Self {
            stall: duration_from_env("WARPI_TURN_STALL_TIMEOUT_SECS")
                .unwrap_or(Duration::from_secs(120)),
            pending_tools: match duration_from_env("WARPI_PENDING_TOOL_TIMEOUT_SECS") {
                Some(value) if value.is_zero() => None,
                Some(value) => Some(value),
                None => Some(Duration::from_secs(1800)),
            },
            cancel: duration_from_env("WARPI_CANCEL_DEADLINE_SECS")
                .unwrap_or(Duration::from_secs(5)),
            compaction: duration_from_env("WARPI_COMPACTION_TIMEOUT_SECS")
                .unwrap_or(Duration::from_secs(600)),
            watchdog_tick: Duration::from_secs(5),
        }
    }
}

fn duration_from_env(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
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
    /// Opt-in helper-internal subagents. Only forwarded to `session.open` when
    /// `enabled` and the helper advertised the `subagents` capability.
    pub subagents: Option<HelperSubagents>,
}

/// Adapter-neutral events for one exchange.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// Opens the exchange. `run_id` is stable for the whole Pi turn.
    Init {
        conversation_id: String,
        request_id: String,
        run_id: String,
    },
    /// The client should upgrade the optimistic task to a server-backed task.
    CreateTask {
        task_id: String,
    },
    TextDelta {
        message_id: String,
        delta: String,
    },
    /// Full assistant message text (sent once per message, after the deltas).
    TextMessage {
        message_id: String,
        text: String,
    },
    /// Tool calls the Warp executor can represent. Untranslatable calls never
    /// reach this variant: the bridge answers them with synthetic results.
    ToolCalls {
        calls: Vec<api::message::ToolCall>,
    },
    /// The exchange is complete but the run is still suspended on these tools.
    ExchangePaused {
        pending: Vec<String>,
    },
    /// The run finished successfully.
    RunSettled {
        stop_reason: String,
        usage: Option<AgentUsage>,
    },
    /// One `assistant.usage` fact per assistant message. The app prices it from
    /// the active profile and appends a local ledger record.
    MessageUsage {
        message_id: String,
        model_id: String,
        usage: AgentUsage,
        duration_ms: u64,
        first_token_ms: Option<u64>,
        output_tokens_per_second: Option<f64>,
        stop_reason: String,
    },
    /// A child `task` run started. Diagnostic-level: no native UI mapping yet.
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
    /// Liveness heartbeat from a running child. Forwarded so the app can observe
    /// progress; a native surface lands later.
    TaskProgress {
        task_id: String,
        child_session_id: String,
        elapsed_ms: u64,
        turns: u64,
        tool_calls: u64,
        tokens: TaskProgressTokens,
        pending_tools: u64,
    },
    /// Terminal child outcome. `usage` is the child's own usage, attributed
    /// separately from the parent so the ledger never double-bills it.
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
    /// Context-window reading after a model call or a compaction.
    ContextUpdated {
        tokens: Option<u64>,
        context_window: Option<u64>,
        percent: Option<f64>,
        source: String,
    },
    /// A compaction completed. Forwarded while a turn is attached; a compaction
    /// that fires with no live exchange is dropped (the resulting
    /// `context.updated` reaches the app on the next exchange).
    CompactionFinished {
        reason: String,
        summarized: bool,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        summary_usage: Option<AgentUsage>,
        duration_ms: Option<u64>,
    },
    RunFailed {
        code: String,
        message: String,
        retryable: bool,
    },
    RunCancelled {
        reason: String,
    },
    /// Protocol violation or helper failure. The exchange settles as an error;
    /// the caller must not automatically retry the affected side effect.
    ProtocolError {
        code: String,
        message: String,
    },
    /// Non-fatal helper diagnostics that are safe to surface.
    Diagnostic {
        level: String,
        message: String,
    },
}

/// A terminal event for a conversation whose exchange stream was already gone
/// (for example a turn that expired while the user was looking at an approval
/// card). The app layer uses it to withdraw work that is still waiting on the
/// dead turn.
#[derive(Debug, Clone)]
pub struct OrphanEvent {
    pub conversation_id: String,
    pub event: BridgeEvent,
}

pub type TurnStream = mpsc::UnboundedReceiver<BridgeEvent>;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("helper process failed to start: {0}")]
    Spawn(String),
    #[error("standalone helper runtime is not usable: {0}")]
    Runtime(#[from] crate::helper::RuntimeCheckError),
    #[error("helper protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("helper exited before completing the request (exit code {0:?}); stderr tail: {1}")]
    HelperExited(Option<i32>, String),
    #[error("no session open for conversation {0}")]
    SessionNotFound(String),
    #[error("session {0} still has a running or queued turn")]
    SessionBusy(String),
    #[error("helper rejected the request: {0}")]
    HelperRejected(String),
    #[error("provider profile is invalid: {0}")]
    Profile(#[from] crate::provider::ProfileError),
    #[error("secret error: {0}")]
    Secret(#[from] crate::secrets::SecretError),
    #[error("bridge shut down")]
    ShutDown,
}

/// One exchange's event stream plus the identity `cancel_turn` matches against.
/// The exchange id is stable for as long as its exchange is attached to a turn.
pub struct TurnExchange {
    pub stream: TurnStream,
    pub exchange_id: String,
    /// True when the prompt was accepted into the session queue instead of
    /// starting: the stream stays idle until the running turn settles.
    pub queued: bool,
}

struct TurnState {
    turn_id: String,
    /// Exchange currently attached to `stream`.
    exchange_id: String,
    /// Tool calls the model requested and Warp has not resolved yet.
    pending: BTreeMap<String, ToolCallSpec>,
    /// When the oldest currently-pending call was emitted; the pending-tool
    /// deadline is measured from here.
    pending_since: Option<Instant>,
    /// Error results ready to deliver on the next resume: calls the executor
    /// cannot represent, and calls whose real result the app can never deliver.
    synthetic: BTreeMap<String, HelperToolResult>,
    /// Tool call ids already delivered in this turn (duplicate detection).
    delivered: BTreeMap<String, ()>,
    /// Tool calls the user explicitly rejected on an approval card. Their
    /// synthetic result is a `Rejected` status instead of a generic error.
    denied: BTreeSet<String>,
    /// Armed when `turn.cancel` is sent for this turn. If the helper does not
    /// terminalize the turn by then, the bridge synthesizes the terminal event.
    cancel_deadline: Option<Instant>,
    /// Set while the helper reports a compaction running inside this turn; the
    /// stall watchdog uses the longer compaction deadline instead.
    compacting_since: Option<Instant>,
    stream: Option<mpsc::UnboundedSender<BridgeEvent>>,
}

/// A prompt that arrived while another turn was running. It starts, in arrival
/// order, as soon as the running turn settles; its exchange id lets a caller
/// cancel it before it ever reaches the helper.
struct QueuedTurn {
    exchange_id: String,
    prompt: String,
    stream: mpsc::UnboundedSender<BridgeEvent>,
}

/// What a cancel request resolved to in the session state machine.
enum CancelTarget {
    Queued(QueuedTurn),
    Active(TurnIdentity),
    Nothing,
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
    /// Prompts waiting for the running turn to settle, oldest first.
    queued: VecDeque<QueuedTurn>,
    open_ack: Option<oneshot::Sender<Result<SessionOpened, BridgeError>>>,
    /// Last time the helper produced anything for this session. The stall
    /// watchdog uses it to cancel turns whose provider went quiet.
    last_activity: Instant,
}

enum Command {
    Hello(oneshot::Sender<Result<HelloOk, BridgeError>>),
    OpenSession(
        Box<SessionSpec>,
        oneshot::Sender<Result<SessionOpened, BridgeError>>,
    ),
    StartTurn {
        conversation_id: String,
        prompt: String,
        reply: oneshot::Sender<Result<TurnExchange, BridgeError>>,
    },
    ResumeTurn {
        conversation_id: String,
        results: Vec<HelperToolResult>,
        reply: oneshot::Sender<Result<TurnExchange, BridgeError>>,
    },
    CancelTurn {
        conversation_id: String,
        /// When set, only this exchange is cancelled: it may be the running turn
        /// or a prompt still waiting in the queue. A cancel for an exchange that
        /// already settled is a no-op rather than a cancel of the current turn.
        exchange_id: Option<String>,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    /// Record that the user explicitly rejected one tool call on an approval
    /// card. The next resume answers it with a `Rejected` status.
    DenyToolCall {
        conversation_id: String,
        tool_call_id: String,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// Handle used by the Warp-side adapter.
pub struct StandaloneBridge {
    commands: mpsc::UnboundedSender<Command>,
    driver: Option<tokio::task::JoinHandle<()>>,
    config: Arc<BridgeConfig>,
    orphan_events: Option<mpsc::UnboundedReceiver<OrphanEvent>>,
}

impl StandaloneBridge {
    /// Spawn the helper and start the driver.
    pub async fn spawn(config: BridgeConfig) -> Result<Self, BridgeError> {
        // Fail with an actionable message before starting a runtime that would
        // abort in its own startup (for example a Node build whose CSPRNG
        // self-check fails), instead of surfacing a raw Node crash tail.
        crate::helper::check_helper_runtime(&config.launch).await?;
        let helper = HelperProcess::spawn(config.launch.clone())
            .await
            .map_err(|e| BridgeError::Spawn(e.to_string()))?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (orphan_tx, orphan_rx) = mpsc::unbounded_channel();
        let timeouts = config.timeouts.clone();
        let driver = tokio::spawn(driver_loop(helper, command_rx, timeouts, orphan_tx));
        Ok(Self {
            commands,
            driver: Some(driver),
            config: Arc::new(config),
            orphan_events: Some(orphan_rx),
        })
    }

    /// Take the conversation-scoped terminal events the bridge could not
    /// deliver on an exchange stream. Call once per bridge.
    pub fn take_orphan_events(&mut self) -> Option<mpsc::UnboundedReceiver<OrphanEvent>> {
        self.orphan_events.take()
    }

    pub fn retry_options(&self) -> &RetryOptions {
        &self.config.retry
    }

    /// Protocol handshake. Fails on version/capability mismatch.
    pub async fn hello(&mut self) -> Result<HelloOk, BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Hello(tx))
            .map_err(|_| BridgeError::ShutDown)?;
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
    ///
    /// When another turn is already running the prompt is accepted and queued
    /// (FIFO) instead of rejected; its stream stays idle until it starts.
    pub async fn start_turn(
        &mut self,
        conversation_id: &str,
        prompt: String,
    ) -> Result<TurnExchange, BridgeError> {
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
    ) -> Result<TurnExchange, BridgeError> {
        let results = results
            .into_iter()
            .map(|(tool_call_id, status, content)| HelperToolResult {
                tool_call_id,
                status,
                content,
            })
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

    /// Cancel the turn `exchange_id` belongs to. `None` cancels whichever turn
    /// is running (used where the caller cannot know the exchange id).
    pub async fn cancel_turn(
        &mut self,
        conversation_id: &str,
        exchange_id: Option<&str>,
    ) -> Result<(), BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::CancelTurn {
                conversation_id: conversation_id.to_string(),
                exchange_id: exchange_id.map(str::to_string),
                reply: tx,
            })
            .map_err(|_| BridgeError::ShutDown)?;
        rx.await.map_err(|_| BridgeError::ShutDown)?
    }

    /// Record a user rejection for one tool call of the running turn. The next
    /// `resume_turn` answers the call with a `Rejected` result.
    pub async fn deny_tool_call(
        &mut self,
        conversation_id: &str,
        tool_call_id: &str,
    ) -> Result<(), BridgeError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::DenyToolCall {
                conversation_id: conversation_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                reply: tx,
            })
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

    /// Best-effort synchronous shutdown for process teardown: ask the driver to
    /// shut the helper down without waiting for the acknowledgement. Dropping
    /// the bridge afterwards ends the driver loop if the command never lands.
    pub fn shutdown_now(&mut self) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.commands.send(Command::Shutdown(tx));
    }
}

impl Drop for StandaloneBridge {
    fn drop(&mut self) {
        // Dropping the command channel ends the driver loop, which reaps the
        // helper process. Ask for a cooperative shutdown first so a helper that
        // is still idle exits promptly; `shutdown()` is the waiting version.
        let (tx, _rx) = oneshot::channel();
        let _ = self.commands.send(Command::Shutdown(tx));
    }
}

/// Why the watchdog expired a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryReason {
    /// The provider produced no events for the stall timeout.
    ProviderStalled,
    /// The turn stayed suspended on tool calls past the pending deadline.
    PendingToolTimeout,
    /// A compaction inside the turn ran past its own deadline.
    CompactionTimeout,
    /// `turn.cancel` was sent but the helper never terminalized the turn.
    CancelDeadline,
}

/// Identity of a turn the watchdog expired, captured before the turn was
/// cleared so the helper can still be told to cancel it.
struct ExpiredTurn {
    conversation_id: String,
    turn_id: String,
    exchange_id: String,
    generation: u64,
    reason: ExpiryReason,
}

struct Driver {
    sessions: HashMap<String, SessionState>,
    seq: u64,
    hello: Option<Result<HelloOk, BridgeError>>,
    hello_waiters: Vec<oneshot::Sender<Result<HelloOk, BridgeError>>>,
    timeouts: BridgeTimeouts,
    /// Terminal events that no exchange stream could receive.
    orphan_events: Option<mpsc::UnboundedSender<OrphanEvent>>,
    /// Whether the connected helper advertised the `subagents` capability. A
    /// session-open `subagents` config is only forwarded when this is true.
    helper_subagents: bool,
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
        // A reopen must never orphan a live turn or a queued prompt: its frames
        // would arrive with the old generation and be dropped, leaving the app
        // with a stream that never terminates.
        if let Some(existing) = self.sessions.get(&spec.conversation_id)
            && (existing.turn.is_some() || !existing.queued.is_empty())
        {
            return Err(BridgeError::SessionBusy(spec.conversation_id.clone()));
        }
        let payload = session_open_payload(spec, self.helper_subagents)?;
        let generation = self
            .sessions
            .get(&spec.conversation_id)
            .map_or(0, |session| session.generation + 1);
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
                queued: VecDeque::new(),
                open_ack: None,
                last_activity: Instant::now(),
            },
        );
        Ok(())
    }

    /// Accept a fresh turn. Prompts that arrive while another turn is running
    /// are queued instead of rejected with a busy error; the queue drains in
    /// arrival order as each turn settles.
    async fn start_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        prompt: String,
        reply: oneshot::Sender<Result<TurnExchange, BridgeError>>,
    ) {
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            let _ = reply.send(Err(BridgeError::SessionNotFound(
                conversation_id.to_string(),
            )));
            return;
        };
        let exchange_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        if session.turn.is_some() {
            let queued_len = session.queued.len() + 1;
            session.queued.push_back(QueuedTurn {
                exchange_id: exchange_id.clone(),
                prompt,
                stream: tx,
            });
            tracing::info!(
                conversation_id,
                queued_len,
                "accepted prompt into the session queue while a turn is running"
            );
            let _ = reply.send(Ok(TurnExchange {
                stream: rx,
                exchange_id,
                queued: true,
            }));
            return;
        }
        match self
            .begin_turn(helper, conversation_id, prompt, exchange_id.clone(), tx)
            .await
        {
            Ok(()) => {
                let _ = reply.send(Ok(TurnExchange {
                    stream: rx,
                    exchange_id,
                    queued: false,
                }));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Send `turn.start` to the helper and attach `stream` to the new turn.
    /// The session must exist and have no running turn.
    async fn begin_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        prompt: String,
        exchange_id: String,
        stream: mpsc::UnboundedSender<BridgeEvent>,
    ) -> Result<(), BridgeError> {
        let Some(session) = self.sessions.get(conversation_id) else {
            return Err(BridgeError::SessionNotFound(conversation_id.to_string()));
        };
        let turn_id = uuid::Uuid::new_v4().to_string();
        let generation = session.generation;
        let create_task = session.create_task && !session.create_task_sent;
        let task_id = if session.task_id.is_empty() {
            conversation_id.to_string()
        } else {
            session.task_id.clone()
        };
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id: turn_id.clone(),
            exchange_id: exchange_id.clone(),
            generation,
        };
        let _ = stream.send(BridgeEvent::Init {
            conversation_id: conversation_id.to_string(),
            request_id: exchange_id.clone(),
            run_id: turn_id.clone(),
        });
        if create_task {
            let _ = stream.send(BridgeEvent::CreateTask { task_id });
        }
        let frame = Envelope::new("turn.start", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "prompt": prompt }));
        helper.send(&frame).await?;
        if let Some(session) = self.sessions.get_mut(conversation_id) {
            session.create_task_sent = true;
            session.last_activity = Instant::now();
            session.turn = Some(TurnState {
                turn_id,
                exchange_id,
                pending: BTreeMap::new(),
                pending_since: None,
                synthetic: BTreeMap::new(),
                delivered: BTreeMap::new(),
                denied: BTreeSet::new(),
                cancel_deadline: None,
                compacting_since: None,
                stream: Some(stream),
            });
        }
        Ok(())
    }

    /// Removes and returns the oldest queued prompt for `conversation_id` when
    /// no turn is running.
    fn dequeue_next(&mut self, conversation_id: &str) -> Option<QueuedTurn> {
        let session = self.sessions.get_mut(conversation_id)?;
        if session.turn.is_some() {
            return None;
        }
        session.queued.pop_front()
    }

    /// Start the oldest queued prompt for `conversation_id`, if any, once no
    /// turn is running. A queued prompt whose start fails settles with a failure
    /// event and the next one is tried.
    async fn start_next_queued(&mut self, helper: &mut HelperProcess, conversation_id: &str) {
        loop {
            let Some(queued) = self.dequeue_next(conversation_id) else {
                return;
            };
            tracing::info!(conversation_id, "starting the next queued prompt");
            let stream = queued.stream.clone();
            if let Err(error) = self
                .begin_turn(
                    helper,
                    conversation_id,
                    queued.prompt,
                    queued.exchange_id,
                    stream.clone(),
                )
                .await
            {
                let _ = stream.send(BridgeEvent::RunFailed {
                    code: "helper_error".to_string(),
                    message: error.to_string(),
                    retryable: false,
                });
            }
        }
    }

    async fn resume_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        results: Vec<HelperToolResult>,
        reply: oneshot::Sender<Result<TurnExchange, BridgeError>>,
    ) {
        let seq = self.next_seq();
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            let _ = reply.send(Err(BridgeError::SessionNotFound(
                conversation_id.to_string(),
            )));
            return;
        };
        session.last_activity = Instant::now();
        let generation = session.generation;
        let Some(turn) = session.turn.as_mut() else {
            let _ = reply.send(Err(BridgeError::HelperRejected(
                "no active Pi turn to resume".to_string(),
            )));
            return;
        };
        let plan = plan_resume(turn, results);
        if !plan.duplicates.is_empty() {
            tracing::info!(
                conversation_id,
                ids = ?plan.duplicates,
                "ignoring tool results that were already delivered in this turn"
            );
        }
        // A request that delivered nothing for this turn cannot be trusted to
        // say which pending calls Warp dropped: fail it without consuming state
        // so the caller can retry with the right batch.
        if plan.accepted.is_empty() && !plan.unknown.is_empty() {
            let exchange_id = turn.exchange_id.clone();
            let turn_id = turn.turn_id.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(BridgeEvent::Init {
                conversation_id: conversation_id.to_string(),
                request_id: exchange_id.clone(),
                run_id: turn_id,
            });
            let _ = tx.send(BridgeEvent::ProtocolError {
                code: "unknown_tool_result".to_string(),
                message: format!(
                    "tool results do not match pending tool calls in this session/turn (unknown: {})",
                    plan.unknown.join(", ")
                ),
            });
            turn.stream = Some(tx);
            let _ = reply.send(Ok(TurnExchange {
                stream: rx,
                exchange_id,
                queued: false,
            }));
            return;
        }
        // Everything was a duplicate: the helper already has these results and
        // must not receive them twice. If the Pi turn is still running, the
        // caller is re-attaching to its continuation (for example after a
        // response-stream retry that re-sent results the bridge had already
        // delivered); keep the turn's exchange identity and let the helper's
        // continuation arrive on the new stream. Synthesizing a terminal event
        // here would strand the live run behind a receiver nobody reads.
        if plan.outgoing.is_empty() {
            let exchange_id = turn.exchange_id.clone();
            let turn_id = turn.turn_id.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(BridgeEvent::Init {
                conversation_id: conversation_id.to_string(),
                request_id: exchange_id.clone(),
                run_id: turn_id,
            });
            turn.stream = Some(tx);
            let _ = reply.send(Ok(TurnExchange {
                stream: rx,
                exchange_id,
                queued: false,
            }));
            return;
        }
        if !plan.unknown.is_empty() {
            tracing::warn!(
                conversation_id,
                ids = ?plan.unknown,
                "ignoring tool results that match no emitted call"
            );
        }
        let exchange_id = uuid::Uuid::new_v4().to_string();
        let turn_id = turn.turn_id.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(BridgeEvent::Init {
            conversation_id: conversation_id.to_string(),
            request_id: exchange_id.clone(),
            run_id: turn_id.clone(),
        });
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id,
            exchange_id: exchange_id.clone(),
            generation,
        };
        let outgoing_ids: Vec<String> = plan
            .outgoing
            .iter()
            .map(|result| result.tool_call_id.clone())
            .collect();
        let outgoing = bounded_resume_results(plan.outgoing);
        let frame = Envelope::new("turn.resume", seq)
            .with_identity(&identity)
            .with_data(serde_json::json!({
                "results": serialized_tool_results(&outgoing),
            }));
        // State is committed only after the helper accepted the frame: a failed
        // send leaves every pending call pending, so the caller can retry.
        if let Err(error) = helper.send(&frame).await {
            let _ = reply.send(Err(error.into()));
            return;
        }
        for id in outgoing_ids {
            turn.pending.remove(&id);
            turn.synthetic.remove(&id);
            turn.delivered.insert(id, ());
        }
        if turn.pending.is_empty() {
            turn.pending_since = None;
        }
        turn.exchange_id = exchange_id.clone();
        turn.stream = Some(tx);
        let _ = reply.send(Ok(TurnExchange {
            stream: rx,
            exchange_id,
            queued: false,
        }));
    }

    /// Send `turn.resume` without opening a new exchange. Used when the bridge
    /// answers tool calls itself: the continuation (text, further calls, or the
    /// terminal event) must reach the exchange those calls arrived in.
    async fn resume_turn_in_place(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        results: Vec<HelperToolResult>,
    ) -> Result<(), BridgeError> {
        let seq = self.next_seq();
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            return Err(BridgeError::SessionNotFound(conversation_id.to_string()));
        };
        let generation = session.generation;
        let Some(turn) = session.turn.as_mut() else {
            return Err(BridgeError::HelperRejected(
                "no active Pi turn to resume".to_string(),
            ));
        };
        let identity = TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id: turn.turn_id.clone(),
            exchange_id: turn.exchange_id.clone(),
            generation,
        };
        let result_ids: Vec<String> = results
            .iter()
            .map(|result| result.tool_call_id.clone())
            .collect();
        let frame = Envelope::new("turn.resume", seq)
            .with_identity(&identity)
            .with_data(serde_json::json!({
                "results": serialized_tool_results(&bounded_resume_results(results)),
            }));
        helper.send(&frame).await?;
        for id in result_ids {
            turn.pending.remove(&id);
            turn.synthetic.remove(&id);
            turn.delivered.insert(id, ());
        }
        if turn.pending.is_empty() {
            turn.pending_since = None;
        }
        Ok(())
    }

    async fn cancel_turn(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        exchange_id: Option<String>,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    ) {
        match self.cancel_target(conversation_id, exchange_id.as_deref()) {
            Err(error) => {
                let _ = reply.send(Err(error));
            }
            Ok(CancelTarget::Queued(queued)) => {
                let _ = queued.stream.send(BridgeEvent::RunCancelled {
                    reason: "cancelled before it started".to_string(),
                });
                let _ = reply.send(Ok(()));
            }
            Ok(CancelTarget::Active(identity)) => {
                let frame = Envelope::new("turn.cancel", self.next_seq())
                    .with_identity(&identity)
                    .with_data(serde_json::json!({ "reason": "cancelled by user" }));
                let result = helper.send(&frame).await.map_err(BridgeError::from);
                if result.is_ok() {
                    self.arm_cancel_deadline(conversation_id, &identity);
                }
                let _ = reply.send(result);
            }
            Ok(CancelTarget::Nothing) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    async fn deny_tool_call(
        &mut self,
        conversation_id: &str,
        tool_call_id: &str,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    ) {
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            let _ = reply.send(Err(BridgeError::SessionNotFound(
                conversation_id.to_string(),
            )));
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            // The turn already settled; nothing left to reject.
            let _ = reply.send(Ok(()));
            return;
        };
        if turn.pending.contains_key(tool_call_id) || turn.synthetic.contains_key(tool_call_id) {
            turn.denied.insert(tool_call_id.to_string());
        }
        let _ = reply.send(Ok(()));
    }

    /// Send `turn.cancel` for the live turn of one session and arm its cancel
    /// deadline. Used when a malformed helper frame makes the turn unusable.
    async fn cancel_turn_for_session(
        &mut self,
        helper: &mut HelperProcess,
        conversation_id: &str,
        reason: &str,
    ) {
        let identity = match self.sessions.get(conversation_id) {
            Some(session) => match session.turn.as_ref() {
                Some(turn) => TurnIdentity {
                    session_id: conversation_id.to_string(),
                    turn_id: turn.turn_id.clone(),
                    exchange_id: turn.exchange_id.clone(),
                    generation: session.generation,
                },
                None => return,
            },
            None => return,
        };
        let frame = Envelope::new("turn.cancel", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "reason": reason }));
        let _ = helper.send(&frame).await;
        self.arm_cancel_deadline(conversation_id, &identity);
    }

    /// Start the clock on a turn.cancel that was just written to the helper.
    fn arm_cancel_deadline(&mut self, conversation_id: &str, identity: &TurnIdentity) {
        let deadline = Instant::now() + self.timeouts.cancel;
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            return;
        };
        if turn.turn_id != identity.turn_id {
            return;
        }
        turn.cancel_deadline = Some(deadline);
    }

    /// Resolves which exchange `exchange_id` identifies. A queued prompt is
    /// matched first so a stale cancel can never fall through to the running
    /// turn; a cancel for an already-settled exchange does nothing.
    fn cancel_target(
        &mut self,
        conversation_id: &str,
        exchange_id: Option<&str>,
    ) -> Result<CancelTarget, BridgeError> {
        let Some(session) = self.sessions.get_mut(conversation_id) else {
            return Err(BridgeError::SessionNotFound(conversation_id.to_string()));
        };
        if let Some(exchange_id) = exchange_id
            && let Some(index) = session
                .queued
                .iter()
                .position(|queued| queued.exchange_id == exchange_id)
            && let Some(queued) = session.queued.remove(index)
        {
            return Ok(CancelTarget::Queued(queued));
        }
        let Some(turn) = session.turn.as_ref() else {
            return Ok(CancelTarget::Nothing);
        };
        if let Some(exchange_id) = exchange_id
            && turn.exchange_id != exchange_id
        {
            return Ok(CancelTarget::Nothing);
        }
        // Cancel targets the exchange currently attached to the turn so the
        // helper's terminal events correlate with an exchange we are reading.
        Ok(CancelTarget::Active(TurnIdentity {
            session_id: conversation_id.to_string(),
            turn_id: turn.turn_id.clone(),
            exchange_id: turn.exchange_id.clone(),
            generation: session.generation,
        }))
    }

    async fn handle_frame(&mut self, helper: &mut HelperProcess, frame: Envelope) {
        // Only frames that belong to the live turn count as activity. A helper
        // that keeps streaming stale-turn events must not keep the session's
        // stall watchdog from firing.
        if self.frame_refreshes_activity(&frame)
            && let Some(session_id) = frame.session_id.as_deref()
            && let Some(session) = self.sessions.get_mut(session_id)
        {
            session.last_activity = Instant::now();
        }
        let event = match HelperEvent::decode(&frame) {
            Ok(event) => event,
            Err(ProtocolError::UnknownKind(kind)) => {
                tracing::debug!(kind, "ignoring unknown helper event kind");
                return;
            }
            Err(error) => {
                let message = error.to_string();
                // A malformed frame is scoped to its session: fail that
                // exchange and cancel its turn rather than taking down every
                // live session in the bridge.
                if let Some(session_id) = frame.session_id.clone() {
                    self.fail_stream_for(&session_id, "helper_protocol_error", &message);
                    self.cancel_turn_for_session(helper, &session_id, "malformed helper event")
                        .await;
                } else {
                    tracing::warn!(kind = %frame.kind, error = %message, "ignoring malformed helper event");
                }
                return;
            }
        };
        match event {
            HelperEvent::HelloOk(ok) => {
                self.helper_subagents = ok.capabilities.subagents;
                self.resolve_hello(Ok(ok));
            }
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
                BridgeEvent::TextDelta {
                    message_id,
                    delta: text,
                },
            ),
            HelperEvent::AssistantMessage { message_id, text } => {
                self.forward(&frame, BridgeEvent::TextMessage { message_id, text })
            }
            HelperEvent::AssistantReasoning { .. } => {
                // Reasoning display is deferred: the v1 UI path does not render
                // thinking tokens (see PROVIDER_COMPATIBILITY.md).
            }
            HelperEvent::ToolCalls { calls } => {
                let session_id = frame.session_id.clone().unwrap_or_default();
                let mut forwarded = Vec::new();
                let mut answer_in_place = Vec::new();
                let mut duplicates = Vec::new();
                if let Some(turn) = self.active_turn_mut(&frame) {
                    // A duplicate id from the helper is answered in place with a
                    // rejection instead of failing the exchange: failing leaves
                    // the helper parked on a call nobody will ever resolve, and
                    // the model never learns why. The helper treats a rejection
                    // for an already-settled call as a duplicate and drops it.
                    let mut fresh_calls = Vec::new();
                    for call in &calls {
                        let was_pending = turn.pending.remove(&call.tool_call_id).is_some();
                        let was_synthetic = turn.synthetic.remove(&call.tool_call_id).is_some();
                        if was_pending
                            || was_synthetic
                            || turn.delivered.contains_key(&call.tool_call_id)
                        {
                            duplicates.push(synthetic_duplicate_result(&call.tool_call_id));
                        } else {
                            fresh_calls.push(call.clone());
                        }
                    }
                    if turn.pending.is_empty() {
                        turn.pending_since = None;
                    }
                    let partition = partition_tool_calls(&fresh_calls);
                    let translatable_ids: BTreeSet<String> = partition
                        .translatable
                        .iter()
                        .map(|call| call.tool_call_id.clone())
                        .collect();
                    for call in &fresh_calls {
                        if translatable_ids.contains(&call.tool_call_id) {
                            turn.pending.insert(call.tool_call_id.clone(), call.clone());
                        }
                    }
                    if !translatable_ids.is_empty() && turn.pending_since.is_none() {
                        turn.pending_since = Some(Instant::now());
                    }
                    let new_synthetic: Vec<HelperToolResult> =
                        partition.synthetic.values().cloned().collect();
                    turn.synthetic.extend(partition.synthetic);
                    if translatable_ids.is_empty() {
                        // Nothing the client can execute: answer every call
                        // ourselves so the Pi run continues in this exchange
                        // instead of waiting for a result that cannot arrive.
                        answer_in_place = new_synthetic;
                    } else {
                        forwarded = partition.translatable;
                    }
                }
                if !forwarded.is_empty() {
                    self.forward(&frame, BridgeEvent::ToolCalls { calls: forwarded });
                }
                answer_in_place.extend(duplicates);
                if !answer_in_place.is_empty()
                    && let Err(error) = self
                        .resume_turn_in_place(helper, &session_id, answer_in_place)
                        .await
                {
                    let message = format!("could not answer unresolvable tool calls: {error}");
                    self.fail_stream_for(&session_id, "helper_error", &message);
                    self.finish_turn(helper, &frame).await;
                }
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
                // A pause is only meaningful to the client when it can see at
                // least one call to answer. Otherwise the bridge already
                // resumed the turn in place and must not settle the exchange.
                let visible = self
                    .active_turn_mut(&frame)
                    .is_some_and(|turn| !turn.pending.is_empty());
                if !visible {
                    tracing::debug!(
                        turn_id = ?frame.turn_id,
                        "suppressing a tool pause with no client-visible calls"
                    );
                    return;
                }
                self.forward(&frame, BridgeEvent::ExchangePaused { pending });
            }
            HelperEvent::TurnCompleted { stop_reason, usage } => {
                self.forward(&frame, BridgeEvent::RunSettled { stop_reason, usage });
                self.finish_turn(helper, &frame).await;
            }
            HelperEvent::AssistantUsage {
                message_id,
                model_id,
                usage,
                duration_ms,
                first_token_ms,
                output_tokens_per_second,
                stop_reason,
            } => self.forward(
                &frame,
                BridgeEvent::MessageUsage {
                    message_id,
                    model_id,
                    usage,
                    duration_ms,
                    first_token_ms,
                    output_tokens_per_second,
                    stop_reason,
                },
            ),
            HelperEvent::TaskStarted {
                task_id,
                child_session_id,
                description,
                subagent_type,
                prompt_bytes,
                max_turns,
                deadline_ms,
                token_cap,
            } => self.forward(
                &frame,
                BridgeEvent::TaskStarted {
                    task_id,
                    child_session_id,
                    description,
                    subagent_type,
                    prompt_bytes,
                    max_turns,
                    deadline_ms,
                    token_cap,
                },
            ),
            HelperEvent::TaskProgress {
                task_id,
                child_session_id,
                elapsed_ms,
                turns,
                tool_calls,
                tokens,
                pending_tools,
            } => self.forward(
                &frame,
                BridgeEvent::TaskProgress {
                    task_id,
                    child_session_id,
                    elapsed_ms,
                    turns,
                    tool_calls,
                    tokens,
                    pending_tools,
                },
            ),
            HelperEvent::TaskCompleted {
                task_id,
                child_session_id,
                status,
                reason,
                subagent_type,
                turns,
                tool_calls,
                usage,
                wall_ms,
                summary_bytes,
            } => self.forward(
                &frame,
                BridgeEvent::TaskCompleted {
                    task_id,
                    child_session_id,
                    status,
                    reason,
                    subagent_type,
                    turns,
                    tool_calls,
                    usage,
                    wall_ms,
                    summary_bytes,
                },
            ),
            HelperEvent::ContextUpdated {
                tokens,
                context_window,
                percent,
                source,
            } => self.forward(
                &frame,
                BridgeEvent::ContextUpdated {
                    tokens,
                    context_window,
                    percent,
                    source,
                },
            ),
            HelperEvent::TurnCancelling { .. } => {
                // The helper is working on the cancel; make sure a helper that
                // never finishes still releases the turn eventually.
                let deadline = Instant::now() + self.timeouts.cancel;
                if let Some(session) = frame
                    .session_id
                    .as_deref()
                    .and_then(|session_id| self.sessions.get_mut(session_id))
                    && let Some(turn) = session.turn.as_mut()
                    && turn.cancel_deadline.is_none()
                {
                    turn.cancel_deadline = Some(deadline);
                }
            }
            HelperEvent::TurnCancelled { reason } => {
                self.forward(&frame, BridgeEvent::RunCancelled { reason });
                self.finish_turn(helper, &frame).await;
            }
            HelperEvent::TurnFailed {
                code,
                message,
                retryable,
            } => {
                self.forward(
                    &frame,
                    BridgeEvent::RunFailed {
                        code,
                        message,
                        retryable,
                    },
                );
                self.finish_turn(helper, &frame).await;
            }
            HelperEvent::Rejected { code, message, .. } => {
                self.forward(&frame, BridgeEvent::ProtocolError { code, message });
            }
            HelperEvent::Diagnostic { level, message } => {
                self.forward(&frame, BridgeEvent::Diagnostic { level, message });
            }
            HelperEvent::CompactionStarted { .. } => {
                if let Some(turn) = self.active_turn_mut(&frame) {
                    turn.compacting_since = Some(Instant::now());
                }
            }
            HelperEvent::CompactionFinished {
                reason,
                summarized,
                tokens_before,
                tokens_after,
                summary_usage,
                duration_ms,
            } => {
                if let Some(turn) = self.active_turn_mut(&frame) {
                    turn.compacting_since = None;
                }
                self.forward(
                    &frame,
                    BridgeEvent::CompactionFinished {
                        reason,
                        summarized,
                        tokens_before,
                        tokens_after,
                        summary_usage,
                        duration_ms,
                    },
                );
            }
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

    /// Whether a frame counts as activity for the session's live turn. Frames
    /// for a previous turn (or a different exchange of the same turn) must not
    /// keep the stall watchdog from firing.
    fn frame_refreshes_activity(&self, frame: &Envelope) -> bool {
        let Some(session_id) = frame.session_id.as_deref() else {
            return false;
        };
        let Some(session) = self.sessions.get(session_id) else {
            return false;
        };
        match session.turn.as_ref() {
            None => true,
            Some(turn) => {
                frame.turn_id.as_deref() == Some(turn.turn_id.as_str())
                    && frame
                        .exchange_id
                        .as_deref()
                        .is_none_or(|exchange_id| exchange_id == turn.exchange_id)
            }
        }
    }

    fn forward(&mut self, frame: &Envelope, event: BridgeEvent) {
        let Some(conversation_id) = frame.session_id.clone() else {
            tracing::debug!(kind = %frame.kind, "dropping turn event without a session");
            return;
        };
        let is_terminal = matches!(
            event,
            BridgeEvent::RunSettled { .. }
                | BridgeEvent::RunFailed { .. }
                | BridgeEvent::RunCancelled { .. }
                | BridgeEvent::ProtocolError { .. }
        );
        let stream = match self.active_turn_mut(frame) {
            Some(turn) => {
                if is_terminal {
                    turn.stream.take()
                } else {
                    turn.stream.clone()
                }
            }
            None => {
                tracing::debug!(kind = %frame.kind, turn_id = ?frame.turn_id, "dropping stale turn event");
                return;
            }
        };
        match stream {
            Some(stream) => match stream.send(event) {
                Ok(()) => {}
                Err(error) => {
                    if is_terminal {
                        self.publish_orphan(&conversation_id, error.0);
                    }
                }
            },
            None => {
                if is_terminal {
                    self.publish_orphan(&conversation_id, event);
                }
            }
        }
    }

    /// Send a terminal event that no exchange stream could receive to the
    /// conversation-scoped orphan channel.
    fn publish_orphan(&mut self, conversation_id: &str, event: BridgeEvent) {
        if let Some(orphan_events) = self.orphan_events.as_ref() {
            let _ = orphan_events.send(OrphanEvent {
                conversation_id: conversation_id.to_string(),
                event,
            });
        }
    }

    fn fail_stream_for(&mut self, session_id: &str, code: &str, message: &str) {
        let event = BridgeEvent::ProtocolError {
            code: code.to_string(),
            message: message.to_string(),
        };
        let has_turn = self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.turn.is_some());
        let stream = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.turn.as_mut())
            .and_then(|turn| turn.stream.take());
        match stream {
            Some(stream) => match stream.send(event) {
                Ok(()) => {}
                Err(error) => {
                    if has_turn {
                        self.publish_orphan(session_id, error.0);
                    }
                }
            },
            None if has_turn => self.publish_orphan(session_id, event),
            None => {}
        }
    }

    fn fail_current_stream(&mut self, code: &str, message: &str) {
        let mut orphans = Vec::new();
        for (conversation_id, session) in self.sessions.iter_mut() {
            let Some(turn) = session.turn.as_mut() else {
                continue;
            };
            let event = BridgeEvent::ProtocolError {
                code: code.to_string(),
                message: message.to_string(),
            };
            match turn.stream.take() {
                Some(stream) => {
                    let _ = stream.send(event);
                }
                None => orphans.push((conversation_id.clone(), event)),
            }
        }
        for (conversation_id, event) in orphans {
            self.publish_orphan(&conversation_id, event);
        }
    }

    /// Fails every queued prompt's stream, so an exchange waiting for a turn
    /// slot does not hang when the session/helper goes away.
    fn fail_queued_streams(&mut self, code: &str, message: &str) {
        for session in self.sessions.values_mut() {
            for queued in session.queued.drain(..) {
                let _ = queued.stream.send(BridgeEvent::RunFailed {
                    code: code.to_string(),
                    message: message.to_string(),
                    retryable: false,
                });
            }
        }
    }

    /// Removes the turn a terminal event belongs to. The event must match the
    /// live turn's id and its attached exchange: a late terminal event from a
    /// turn the watchdog expired must not clear the turn that replaced it.
    fn take_matching_turn(&mut self, frame: &Envelope) -> Option<TurnState> {
        let session = self.sessions.get_mut(frame.session_id.as_deref()?)?;
        let turn = session.turn.as_ref()?;
        if frame.turn_id.as_deref() != Some(turn.turn_id.as_str())
            || frame.exchange_id.as_deref() != Some(turn.exchange_id.as_str())
        {
            return None;
        }
        session.turn.take()
    }

    /// Clears the finished turn and immediately starts the next queued prompt,
    /// so FIFO acceptance turns into FIFO execution.
    async fn finish_turn(&mut self, helper: &mut HelperProcess, frame: &Envelope) {
        let Some(session_id) = frame.session_id.clone() else {
            return;
        };
        let Some(turn) = self.take_matching_turn(frame) else {
            tracing::warn!(
                session_id,
                turn_id = ?frame.turn_id,
                "ignoring a terminal event for a turn that is no longer live"
            );
            return;
        };
        // The run is over, so its unresolved calls can never be answered. They
        // are dropped with the turn; the warning is the audit trail.
        if !turn.pending.is_empty() || !turn.synthetic.is_empty() {
            let mut unresolved: Vec<&str> = turn.pending.keys().map(String::as_str).collect();
            unresolved.extend(turn.synthetic.keys().map(String::as_str));
            tracing::warn!(
                session_id,
                unresolved = ?unresolved,
                "turn settled with tool calls that never received a result"
            );
        }
        self.start_next_queued(helper, &session_id).await;
    }

    /// Expires turns past any of the watchdog deadlines: the attached stream is
    /// settled with the matching event and the session stops being busy, so the
    /// next prompt is accepted instead of hitting a busy error forever.
    fn expire_stalled_turns(&mut self, timeouts: &BridgeTimeouts) -> Vec<ExpiredTurn> {
        let now = Instant::now();
        let mut expired = Vec::new();
        let mut orphans = Vec::new();
        for (conversation_id, session) in self.sessions.iter_mut() {
            let reason = match session.turn.as_ref() {
                None => continue,
                Some(turn) => {
                    if turn.cancel_deadline.is_some_and(|deadline| now >= deadline) {
                        ExpiryReason::CancelDeadline
                    } else if !turn.pending.is_empty() {
                        match timeouts.pending_tools {
                            Some(timeout)
                                if turn.pending_since.is_some_and(|since| {
                                    now.saturating_duration_since(since) >= timeout
                                }) =>
                            {
                                ExpiryReason::PendingToolTimeout
                            }
                            _ => continue,
                        }
                    } else if let Some(since) = turn.compacting_since {
                        // Compaction runs as one long provider call inside the
                        // turn; give it its own deadline instead of the stall one.
                        if now.saturating_duration_since(since) >= timeouts.compaction {
                            ExpiryReason::CompactionTimeout
                        } else {
                            continue;
                        }
                    } else if now.saturating_duration_since(session.last_activity) >= timeouts.stall
                    {
                        ExpiryReason::ProviderStalled
                    } else {
                        continue;
                    }
                }
            };
            let Some(turn) = session.turn.take() else {
                continue;
            };
            let event = match reason {
                ExpiryReason::ProviderStalled => BridgeEvent::ProtocolError {
                    code: "timeout".to_string(),
                    message: format!(
                        "the provider produced no events for {}s, so the turn was cancelled; send the prompt again",
                        timeouts.stall.as_secs()
                    ),
                },
                ExpiryReason::PendingToolTimeout => BridgeEvent::RunFailed {
                    code: "tool_timeout".to_string(),
                    message: format!(
                        "{} tool call(s) received no result within {}s, so the turn was cancelled and the calls were dropped; send the prompt again",
                        turn.pending.len(),
                        timeouts
                            .pending_tools
                            .map_or(0, |timeout| timeout.as_secs())
                    ),
                    retryable: true,
                },
                ExpiryReason::CompactionTimeout => BridgeEvent::RunFailed {
                    code: "compaction_timeout".to_string(),
                    message: format!(
                        "compaction did not finish within {}s, so the turn was cancelled; send the prompt again",
                        timeouts.compaction.as_secs()
                    ),
                    retryable: true,
                },
                ExpiryReason::CancelDeadline => BridgeEvent::RunCancelled {
                    reason: "the helper did not acknowledge turn.cancel in time".to_string(),
                },
            };
            match turn.stream.as_ref() {
                Some(stream) => {
                    if let Err(error) = stream.send(event) {
                        // The exchange the app was reading is gone (a paused
                        // approval card leaves no live stream). The app layer
                        // still has to learn the turn died so it can withdraw
                        // the card.
                        orphans.push((conversation_id.clone(), error.0));
                    }
                }
                None => orphans.push((conversation_id.clone(), event)),
            }
            expired.push(ExpiredTurn {
                conversation_id: conversation_id.clone(),
                turn_id: turn.turn_id,
                exchange_id: turn.exchange_id,
                generation: session.generation,
                reason,
            });
        }
        for (conversation_id, event) in orphans {
            self.publish_orphan(&conversation_id, event);
        }
        expired
    }

    /// Best-effort `turn.cancel` for a turn the watchdog already expired. Turns
    /// expired by the cancel deadline have already been told to cancel.
    async fn cancel_stalled_turn(&mut self, helper: &mut HelperProcess, expired: &ExpiredTurn) {
        let reason = match expired.reason {
            ExpiryReason::ProviderStalled => "provider went quiet",
            ExpiryReason::PendingToolTimeout => "pending tool calls received no result",
            ExpiryReason::CompactionTimeout => "compaction did not finish",
            ExpiryReason::CancelDeadline => return,
        };
        let identity = TurnIdentity {
            session_id: expired.conversation_id.clone(),
            turn_id: expired.turn_id.clone(),
            exchange_id: expired.exchange_id.clone(),
            generation: expired.generation,
        };
        let frame = Envelope::new("turn.cancel", self.next_seq())
            .with_identity(&identity)
            .with_data(serde_json::json!({ "reason": reason }));
        let _ = helper.send(&frame).await;
    }

    fn fail_pending_opens(&mut self, error: BridgeError) {
        for session in self.sessions.values_mut() {
            if let Some(ack) = session.open_ack.take() {
                let _ = ack.send(Err(error.clone()));
            }
        }
    }
}

/// Split a helper tool-call batch into calls the Warp executor can represent
/// and error results for the rest. Untranslatable calls never reach the app as
/// `Tool::Server`, which maps to `NoClientRepresentation` and would park the
/// run waiting for a result that no action produces.
struct ToolCallPartition {
    translatable: Vec<api::message::ToolCall>,
    synthetic: BTreeMap<String, HelperToolResult>,
}

fn partition_tool_calls(calls: &[ToolCallSpec]) -> ToolCallPartition {
    let mut translatable = Vec::new();
    let mut synthetic = BTreeMap::new();
    for call in calls {
        match translate_tool_call(call) {
            Ok(translated) => translatable.push(translated),
            Err(error) => {
                synthetic.insert(
                    call.tool_call_id.clone(),
                    synthetic_untranslatable_result(&call.tool_call_id, &error),
                );
            }
        }
    }
    ToolCallPartition {
        translatable,
        synthetic,
    }
}

/// The model-visible result for a call the executor cannot represent.
fn synthetic_untranslatable_result(
    tool_call_id: &str,
    error: &ToolTranslationError,
) -> HelperToolResult {
    HelperToolResult {
        tool_call_id: tool_call_id.to_string(),
        status: HelperToolResultStatus::Error,
        content: format!(
            "Warp cannot execute this tool call: {error}. Fix the arguments or use a supported tool and continue."
        ),
    }
}

/// The model-visible result for a call whose real result Warp can never
/// deliver (for example a command the snapshot cascade cancelled).
fn synthetic_cancelled_result(tool_call_id: &str) -> HelperToolResult {
    HelperToolResult {
        tool_call_id: tool_call_id.to_string(),
        status: HelperToolResultStatus::Error,
        content: format!(
            "{tool_call_id} was cancelled in Warp and has no result. Do not wait for it; continue without its output, or use a different approach."
        ),
    }
}

/// The model-visible result for a call the user explicitly rejected on an
/// approval card. This is a rejection, not a transient failure: the model must
/// not retry the same command on its own.
fn synthetic_rejected_result(tool_call_id: &str) -> HelperToolResult {
    HelperToolResult {
        tool_call_id: tool_call_id.to_string(),
        status: HelperToolResultStatus::Rejected,
        content: format!(
            "The user rejected {tool_call_id}; it was not run. Do not run it again unless the user explicitly asks for it. Continue with a different approach or explain what you need."
        ),
    }
}

/// The model-visible result for a tool-call id the helper emitted twice. The
/// duplicate is rejected so the original call keeps its one result.
fn synthetic_duplicate_result(tool_call_id: &str) -> HelperToolResult {
    HelperToolResult {
        tool_call_id: tool_call_id.to_string(),
        status: HelperToolResultStatus::Rejected,
        content: format!(
            "Duplicate tool call id {tool_call_id} was ignored; the original call already has or will receive its result. Continue."
        ),
    }
}

/// What a `turn.resume` request should deliver.
struct ResumePlan {
    /// Results the helper will receive, in order: accepted results, deferred
    /// synthetic errors, then synthesized errors for every dropped call.
    outgoing: Vec<HelperToolResult>,
    /// Ids the request supplied that matched a pending call.
    accepted: Vec<String>,
    /// Ids the request supplied that were already delivered.
    duplicates: Vec<String>,
    /// Ids the request supplied that match no call in this turn.
    unknown: Vec<String>,
}

/// Classify incoming tool results and fill the gaps Warp leaves.
///
/// The app only sends a follow-up once every action in the exchange finished or
/// was cancelled, so a pending call missing from the batch is one whose result
/// Warp dropped (a cancelled command converts to `Ignore`); it is answered
/// here. A request that delivered nothing recognized is not trusted to say
/// anything about the pending set, so it leaves the turn untouched.
fn plan_resume(turn: &TurnState, results: Vec<HelperToolResult>) -> ResumePlan {
    let mut accepted = Vec::new();
    let mut accepted_ids = Vec::new();
    let mut duplicates = Vec::new();
    let mut unknown = Vec::new();
    for result in results {
        if turn.pending.contains_key(&result.tool_call_id) {
            accepted_ids.push(result.tool_call_id.clone());
            accepted.push(result);
        } else if turn.delivered.contains_key(&result.tool_call_id)
            || turn.synthetic.contains_key(&result.tool_call_id)
        {
            duplicates.push(result.tool_call_id);
        } else {
            unknown.push(result.tool_call_id);
        }
    }
    let has_accepted = !accepted.is_empty();
    let mut delivered: BTreeSet<String> = accepted_ids.iter().cloned().collect();
    let mut outgoing = accepted;
    for (id, result) in &turn.synthetic {
        if delivered.insert(id.clone()) {
            outgoing.push(result.clone());
        }
    }
    if has_accepted {
        for id in turn.pending.keys() {
            if delivered.insert(id.clone()) {
                outgoing.push(if turn.denied.contains(id) {
                    synthetic_rejected_result(id)
                } else {
                    synthetic_cancelled_result(id)
                });
            }
        }
    }
    ResumePlan {
        outgoing,
        accepted: accepted_ids,
        duplicates,
        unknown,
    }
}

/// Headroom kept for the envelope and result metadata when bounding one
/// `turn.resume` frame.
const RESUME_FRAME_HEADROOM_BYTES: usize = 64 * 1024;
/// Approximate JSON cost of one result object besides its content.
const RESULT_JSON_OVERHEAD_BYTES: usize = 512;
/// Floor for a single result's content after the aggregate bound is applied;
/// every pending call still receives a result even in a very large batch.
const MIN_RESULT_CONTENT_BYTES: usize = 64;

/// Bound one `turn.resume` frame: the helper rejects frames over
/// [`crate::protocol::MAX_FRAME_BYTES`], so an oversized batch (for example
/// many large file reads answered in one request) has to be truncated rather
/// than failing permanently. Every result is kept, with its content capped.
fn bounded_resume_results(results: Vec<HelperToolResult>) -> Vec<HelperToolResult> {
    if results.is_empty() {
        return results;
    }
    let budget = MAX_FRAME_BYTES - RESUME_FRAME_HEADROOM_BYTES;
    let mut cap = budget
        .saturating_sub(results.len() * RESULT_JSON_OVERHEAD_BYTES)
        .checked_div(results.len())
        .unwrap_or(0)
        .max(2 * MIN_RESULT_CONTENT_BYTES);
    loop {
        let bounded: Vec<HelperToolResult> = results
            .iter()
            .map(|result| HelperToolResult {
                content: bounded_content(&result.content, cap),
                ..result.clone()
            })
            .collect();
        if resume_payload_size(&bounded) <= budget || cap <= MIN_RESULT_CONTENT_BYTES {
            return bounded;
        }
        cap = (cap / 2).max(MIN_RESULT_CONTENT_BYTES);
    }
}

fn resume_payload_size(results: &[HelperToolResult]) -> usize {
    serde_json::to_string(&serde_json::json!({
        "results": serialized_tool_results(results),
    }))
    .map(|json| json.len())
    .unwrap_or(0)
}

fn bounded_content(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated by standalone backend: resume frame size limit]",
        &value[..end]
    )
}

fn serialized_tool_results(results: &[HelperToolResult]) -> Vec<serde_json::Value> {
    results
        .iter()
        .map(|result| {
            serde_json::json!({
                "tool_call_id": result.tool_call_id,
                "status": result.status,
                "content": result.content,
            })
        })
        .collect()
}

impl Clone for BridgeError {
    fn clone(&self) -> Self {
        match self {
            BridgeError::Spawn(message) => BridgeError::Spawn(message.clone()),
            BridgeError::Runtime(error) => BridgeError::Runtime(error.clone()),
            BridgeError::Protocol(error) => BridgeError::Protocol(error.clone()),
            BridgeError::HelperExited(code, tail) => BridgeError::HelperExited(*code, tail.clone()),
            BridgeError::SessionNotFound(id) => BridgeError::SessionNotFound(id.clone()),
            BridgeError::SessionBusy(id) => BridgeError::SessionBusy(id.clone()),
            BridgeError::HelperRejected(message) => BridgeError::HelperRejected(message.clone()),
            BridgeError::Profile(error) => BridgeError::Profile(error.clone()),
            BridgeError::Secret(error) => BridgeError::Secret(error.clone()),
            BridgeError::ShutDown => BridgeError::ShutDown,
        }
    }
}

async fn driver_loop(
    mut helper: HelperProcess,
    mut commands: mpsc::UnboundedReceiver<Command>,
    timeouts: BridgeTimeouts,
    orphan_events: mpsc::UnboundedSender<OrphanEvent>,
) {
    let watchdog_tick = timeouts.watchdog_tick;
    let mut output_rx = helper.take_output();
    let mut driver = Driver {
        sessions: HashMap::new(),
        seq: 0,
        hello: None,
        hello_waiters: Vec::new(),
        timeouts: timeouts.clone(),
        orphan_events: Some(orphan_events),
        helper_subagents: false,
    };
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
                    Command::CancelTurn { conversation_id, exchange_id, reply } => {
                        driver
                            .cancel_turn(&mut helper, &conversation_id, exchange_id, reply)
                            .await;
                    }
                    Command::DenyToolCall { conversation_id, tool_call_id, reply } => {
                        driver
                            .deny_tool_call(&conversation_id, &tool_call_id, reply)
                            .await;
                    }
                    Command::Shutdown(ack) => {
                        let error = BridgeError::ShutDown;
                        driver.resolve_hello(Err(error.clone()));
                        driver.fail_pending_opens(error);
                        driver.fail_current_stream("shutdown", "standalone bridge is shutting down");
                        driver.fail_queued_streams("shutdown", "standalone bridge is shutting down");
                        helper.shutdown().await;
                        let _ = ack.send(());
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(watchdog_tick) => {
                for expired in driver.expire_stalled_turns(&timeouts) {
                    driver.cancel_stalled_turn(&mut helper, &expired).await;
                    driver
                        .start_next_queued(&mut helper, &expired.conversation_id)
                        .await;
                }
            }
            output = output_rx.recv() => {
                let output = helper.observe(output);
                let Some(output) = output else { break };
                match output {
                    HelperOutput::Frame(frame) => driver.handle_frame(&mut helper, *frame).await,
                    HelperOutput::ProtocolError(error) => {
                        let message = error.to_string();
                        driver.fail_current_stream("helper_protocol_error", &message);
                        driver.fail_queued_streams("helper_protocol_error", &message);
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
                        driver.fail_queued_streams(
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

/// Build the helper's `session.open` payload for one spec. Pure, so the
/// compaction policy is testable without a helper process. `helper_subagents`
/// gates the opt-in `subagents` config on the helper's advertised capability:
/// a helper that cannot host children must keep opening inert sessions.
fn session_open_payload(
    spec: &SessionSpec,
    helper_subagents: bool,
) -> Result<HelperSessionOpen, BridgeError> {
    let api_key = spec
        .api_key
        .as_ref()
        .map(|key| key.expose_secret().to_string());
    let provider = spec.provider.helper_config(api_key.as_deref())?;
    if matches!(provider.auth, HelperProviderAuth::None) && api_key.is_some() {
        return Err(BridgeError::HelperRejected(
            "auth=none profile must not resolve a credential".into(),
        ));
    }
    let subagents = spec
        .subagents
        .clone()
        .filter(|config| config.enabled)
        .filter(|_| {
            if helper_subagents {
                true
            } else {
                tracing::warn!(
                    conversation_id = %spec.conversation_id,
                    "session requested subagents but the helper does not advertise the capability; opening without the task tool"
                );
                false
            }
        });
    let pi_dir = spec.data_dir.join("pi");
    Ok(HelperSessionOpen {
        working_dir: spec.working_dir.to_string_lossy().into_owned(),
        agent_dir: pi_dir.join("agent").to_string_lossy().into_owned(),
        session_dir: pi_dir.join("sessions").to_string_lossy().into_owned(),
        session_file: spec
            .session_file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        system_prompt: spec.system_prompt.clone(),
        load_context_files: spec.load_context_files,
        max_context_file_bytes: spec.max_context_file_bytes,
        provider,
        compaction: derive_compaction_settings(&spec.provider),
        retry: HelperRetry {
            enabled: false,
            max_retries: 0,
            base_delay_ms: 0,
        },
        subagents,
    })
}

/// Mirror of the helper's `deriveCompactionSettings`. Sending explicit derived
/// numbers (never zero) keeps older helpers that respect the payload and newer
/// ones that would otherwise derive identical values on the same policy.
fn derive_compaction_settings(profile: &ProviderProfile) -> HelperCompaction {
    let quarter = (profile.context_limit / 4).max(2_048);
    HelperCompaction {
        enabled: true,
        reserve_tokens: quarter.min(16_384).min(profile.output_limit),
        keep_recent_tokens: quarter.min(20_000),
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

#[cfg(test)]
mod queue_and_watchdog_tests {
    use std::time::Duration;

    use super::*;

    fn session_with_turn(
        tx: mpsc::UnboundedSender<BridgeEvent>,
        idle_for: Duration,
    ) -> SessionState {
        SessionState {
            conversation_id: "c1".to_string(),
            generation: 1,
            task_id: String::new(),
            provider_id: "p".to_string(),
            working_dir: PathBuf::from("/tmp"),
            session_file: None,
            create_task: false,
            create_task_sent: true,
            turn: Some(TurnState {
                turn_id: "t".to_string(),
                exchange_id: "e".to_string(),
                pending: BTreeMap::new(),
                pending_since: None,
                synthetic: BTreeMap::new(),
                delivered: BTreeMap::new(),
                denied: BTreeSet::new(),
                cancel_deadline: None,
                compacting_since: None,
                stream: Some(tx),
            }),
            queued: VecDeque::new(),
            open_ack: None,
            last_activity: Instant::now() - idle_for,
        }
    }

    fn empty_driver() -> Driver {
        Driver {
            sessions: HashMap::new(),
            seq: 0,
            hello: None,
            hello_waiters: Vec::new(),
            timeouts: BridgeTimeouts::default(),
            orphan_events: None,
            helper_subagents: false,
        }
    }

    /// Timeouts with a far-future pending deadline, so only the behavior under
    /// test fires.
    fn timeouts(stall: Duration) -> BridgeTimeouts {
        BridgeTimeouts {
            stall,
            pending_tools: Some(Duration::from_secs(3600)),
            cancel: Duration::from_secs(300),
            compaction: Duration::from_secs(3600),
            watchdog_tick: Duration::from_secs(5),
        }
    }

    fn pending_bash_call(id: &str) -> ToolCallSpec {
        ToolCallSpec {
            tool_call_id: id.to_string(),
            name: "workspace.shell".to_string(),
            arguments: serde_json::json!({ "command": "ls" }),
        }
    }

    fn enqueue(driver: &mut Driver, conversation_id: &str, exchange_id: &str, prompt: &str) {
        let session = driver.sessions.get_mut(conversation_id).expect("session");
        session.queued.push_back(QueuedTurn {
            exchange_id: exchange_id.to_string(),
            prompt: prompt.to_string(),
            stream: mpsc::unbounded_channel().0,
        });
    }

    #[test]
    fn quiet_turns_are_expired_with_a_timeout_error() {
        let mut driver = empty_driver();
        let (tx, mut rx) = mpsc::unbounded_channel();
        driver.sessions.insert(
            "c1".to_string(),
            session_with_turn(tx, Duration::from_secs(300)),
        );

        let stalled = driver.expire_stalled_turns(&timeouts(Duration::from_secs(120)));

        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].conversation_id, "c1");
        assert_eq!(stalled[0].turn_id, "t");
        assert_eq!(stalled[0].exchange_id, "e");
        assert_eq!(stalled[0].generation, 1);
        assert_eq!(stalled[0].reason, ExpiryReason::ProviderStalled);
        assert!(driver.sessions.get("c1").expect("session").turn.is_none());
        match rx.try_recv() {
            Ok(BridgeEvent::ProtocolError { code, message }) => {
                assert_eq!(code, "timeout");
                assert!(
                    message.contains("no events"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn queued_prompts_are_dequeued_fifo_and_only_when_idle() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));
        enqueue(&mut driver, "c1", "e2", "second");
        enqueue(&mut driver, "c1", "e3", "third");

        // The running turn blocks the queue from draining.
        assert!(driver.dequeue_next("c1").is_none());

        driver.sessions.get_mut("c1").expect("session").turn = None;
        assert_eq!(
            driver.dequeue_next("c1").map(|queued| queued.prompt),
            Some("second".to_string())
        );
        assert_eq!(
            driver.dequeue_next("c1").map(|queued| queued.prompt),
            Some("third".to_string())
        );
        assert!(driver.dequeue_next("c1").is_none());
    }

    #[test]
    fn cancelling_a_queued_prompt_does_not_touch_the_running_turn() {
        let mut driver = empty_driver();
        let (tx, mut rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));
        enqueue(&mut driver, "c1", "e2", "second");

        match driver
            .cancel_target("c1", Some("e2"))
            .expect("session exists")
        {
            CancelTarget::Queued(queued) => {
                assert_eq!(queued.prompt, "second");
                let _ = queued.stream.send(BridgeEvent::RunCancelled {
                    reason: "test".into(),
                });
            }
            _ => panic!("expected the queued prompt to be the cancel target"),
        }

        let session = driver.sessions.get("c1").expect("session");
        assert!(session.turn.is_some(), "the running turn must survive");
        assert!(session.queued.is_empty());
        assert!(!matches!(
            rx.try_recv(),
            Ok(BridgeEvent::RunCancelled { .. })
        ));
    }

    #[test]
    fn a_cancel_for_a_settled_exchange_is_a_no_op() {
        let mut driver = empty_driver();
        let (tx, mut rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        let target = driver
            .cancel_target("c1", Some("stale"))
            .expect("session exists");

        assert!(matches!(target, CancelTarget::Nothing));
        let session = driver.sessions.get("c1").expect("session");
        let turn = session.turn.as_ref().expect("turn still running");
        assert_eq!(turn.exchange_id, "e");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_cancel_without_an_exchange_id_targets_the_running_turn() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        match driver.cancel_target("c1", None).expect("session exists") {
            CancelTarget::Active(identity) => {
                assert_eq!(identity.turn_id, "t");
                assert_eq!(identity.exchange_id, "e");
            }
            _ => panic!("expected the running turn to be the cancel target"),
        }
    }

    #[test]
    fn a_cancel_for_the_running_exchange_targets_it() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        match driver
            .cancel_target("c1", Some("e"))
            .expect("session exists")
        {
            CancelTarget::Active(identity) => assert_eq!(identity.turn_id, "t"),
            _ => panic!("expected the running turn to be the cancel target"),
        }
    }

    #[test]
    fn turns_waiting_for_tool_approval_are_not_expired() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = session_with_turn(tx, Duration::from_secs(300));
        let turn = session.turn.as_mut().expect("turn");
        turn.pending
            .insert("call_1".to_string(), pending_bash_call("call_1"));
        turn.pending_since = Some(Instant::now() - Duration::from_secs(300));
        driver.sessions.insert("c1".to_string(), session);

        let stalled = driver.expire_stalled_turns(&timeouts(Duration::from_secs(120)));

        assert!(stalled.is_empty());
        assert!(driver.sessions.get("c1").expect("session").turn.is_some());
    }

    #[test]
    fn a_resume_synthesizes_results_for_ids_warp_never_returned() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = session_with_turn(tx, Duration::ZERO);
        let mut turn = session.turn.expect("turn");
        turn.pending
            .insert("call_a".to_string(), pending_bash_call("call_a"));
        turn.pending
            .insert("call_b".to_string(), pending_bash_call("call_b"));

        let plan = plan_resume(
            &turn,
            vec![HelperToolResult {
                tool_call_id: "call_a".to_string(),
                status: HelperToolResultStatus::Success,
                content: "a output".to_string(),
            }],
        );

        assert_eq!(plan.accepted, vec!["call_a".to_string()]);
        assert!(plan.duplicates.is_empty());
        assert!(plan.unknown.is_empty());
        let ids: Vec<&str> = plan
            .outgoing
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect();
        assert_eq!(ids, vec!["call_a", "call_b"]);
        let synthesized = plan.outgoing.last().expect("synthesized result");
        assert_eq!(synthesized.status, HelperToolResultStatus::Error);
        assert!(
            synthesized.content.contains("cancelled in Warp"),
            "unexpected synthesized content: {}",
            synthesized.content
        );
    }

    #[test]
    fn a_duplicate_or_foreign_result_never_consumes_pending_calls() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = session_with_turn(tx, Duration::ZERO);
        let mut turn = session.turn.expect("turn");
        turn.pending
            .insert("call_a".to_string(), pending_bash_call("call_a"));
        turn.delivered.insert("call_b".to_string(), ());

        let duplicate = plan_resume(
            &turn,
            vec![HelperToolResult {
                tool_call_id: "call_b".to_string(),
                status: HelperToolResultStatus::Success,
                content: "again".to_string(),
            }],
        );
        assert!(duplicate.accepted.is_empty());
        assert_eq!(duplicate.duplicates, vec!["call_b".to_string()]);
        assert!(
            duplicate.outgoing.is_empty(),
            "a duplicate-only batch settles without resuming"
        );

        let foreign = plan_resume(
            &turn,
            vec![HelperToolResult {
                tool_call_id: "someone-else".to_string(),
                status: HelperToolResultStatus::Success,
                content: "x".to_string(),
            }],
        );
        assert!(foreign.accepted.is_empty());
        assert_eq!(foreign.unknown, vec!["someone-else".to_string()]);
        assert!(
            foreign.outgoing.is_empty(),
            "a foreign-only batch must not cancel the pending calls"
        );
    }

    #[test]
    fn deferred_untranslatable_calls_are_delivered_on_the_next_resume() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = session_with_turn(tx, Duration::ZERO);
        let mut turn = session.turn.expect("turn");
        turn.pending
            .insert("call_good".to_string(), pending_bash_call("call_good"));
        turn.synthetic.insert(
            "call_bad".to_string(),
            synthetic_untranslatable_result(
                "call_bad",
                &ToolTranslationError::UnsupportedArgument("glob".to_string()),
            ),
        );

        let plan = plan_resume(
            &turn,
            vec![HelperToolResult {
                tool_call_id: "call_good".to_string(),
                status: HelperToolResultStatus::Success,
                content: "ok".to_string(),
            }],
        );

        let ids: Vec<&str> = plan
            .outgoing
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect();
        assert_eq!(ids, vec!["call_good", "call_bad"]);
        assert!(plan.outgoing[1].content.contains("glob"));
    }

    #[test]
    fn a_turn_that_never_answers_cancel_is_expired() {
        let mut driver = empty_driver();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = session_with_turn(tx, Duration::ZERO);
        session.turn.as_mut().expect("turn").cancel_deadline =
            Some(Instant::now() - Duration::from_secs(1));
        driver.sessions.insert("c1".to_string(), session);

        let expired = driver.expire_stalled_turns(&timeouts(Duration::from_secs(120)));

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].reason, ExpiryReason::CancelDeadline);
        assert!(driver.sessions.get("c1").expect("session").turn.is_none());
        match rx.try_recv() {
            Ok(BridgeEvent::RunCancelled { reason }) => {
                assert!(reason.contains("did not acknowledge"), "reason: {reason}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn pending_tool_calls_are_expired_with_a_retryable_tool_timeout() {
        let mut driver = empty_driver();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = session_with_turn(tx, Duration::from_secs(300));
        let turn = session.turn.as_mut().expect("turn");
        turn.pending
            .insert("call_1".to_string(), pending_bash_call("call_1"));
        turn.pending_since = Some(Instant::now() - Duration::from_secs(300));
        driver.sessions.insert("c1".to_string(), session);

        let expired = driver.expire_stalled_turns(&BridgeTimeouts {
            pending_tools: Some(Duration::from_secs(120)),
            ..timeouts(Duration::from_secs(3600))
        });

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].reason, ExpiryReason::PendingToolTimeout);
        match rx.try_recv() {
            Ok(BridgeEvent::RunFailed {
                code,
                retryable,
                message,
            }) => {
                assert_eq!(code, "tool_timeout");
                assert!(retryable);
                assert!(message.contains("dropped"), "message: {message}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn a_stale_terminal_event_does_not_clear_the_live_turn() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        let stale = Envelope::new("turn.completed", 7).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "old-turn".to_string(),
            exchange_id: "e".to_string(),
            generation: 1,
        });
        assert!(
            driver.take_matching_turn(&stale).is_none(),
            "a terminal event for a different turn must not clear the live one"
        );

        let stale_exchange = Envelope::new("turn.completed", 8).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "t".to_string(),
            exchange_id: "old-exchange".to_string(),
            generation: 1,
        });
        assert!(driver.take_matching_turn(&stale_exchange).is_none());

        let matching = Envelope::new("turn.completed", 9).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "t".to_string(),
            exchange_id: "e".to_string(),
            generation: 1,
        });
        assert!(driver.take_matching_turn(&matching).is_some());
        assert!(driver.sessions.get("c1").expect("session").turn.is_none());
    }

    #[test]
    fn untranslatable_calls_are_partitioned_out_of_the_client_batch() {
        let calls = vec![
            pending_bash_call("call_good"),
            ToolCallSpec {
                tool_call_id: "call_bad".to_string(),
                name: "workspace.grep".to_string(),
                arguments: serde_json::json!({ "pattern": "x", "glob": "*.rs" }),
            },
        ];

        let partition = partition_tool_calls(&calls);

        assert_eq!(partition.translatable.len(), 1);
        assert_eq!(partition.translatable[0].tool_call_id, "call_good");
        let synthetic = partition
            .synthetic
            .get("call_bad")
            .expect("synthetic error");
        assert_eq!(synthetic.status, HelperToolResultStatus::Error);
        assert!(
            synthetic.content.contains("not supported")
                || synthetic.content.contains("Warp cannot execute"),
            "unexpected content: {}",
            synthetic.content
        );
    }

    #[test]
    fn denied_pending_calls_are_rejected_on_resume() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = session_with_turn(tx, Duration::ZERO);
        let mut turn = session.turn.expect("turn");
        turn.pending
            .insert("call_a".to_string(), pending_bash_call("call_a"));
        turn.pending
            .insert("call_b".to_string(), pending_bash_call("call_b"));
        turn.denied.insert("call_b".to_string());

        let plan = plan_resume(
            &turn,
            vec![HelperToolResult {
                tool_call_id: "call_a".to_string(),
                status: HelperToolResultStatus::Success,
                content: "a output".to_string(),
            }],
        );

        let rejected = plan
            .outgoing
            .iter()
            .find(|result| result.tool_call_id == "call_b")
            .expect("call_b is answered");
        assert_eq!(rejected.status, HelperToolResultStatus::Rejected);
        assert!(
            rejected.content.contains("The user rejected"),
            "unexpected content: {}",
            rejected.content
        );
        assert!(
            rejected.content.contains("Do not run it again"),
            "a rejection must tell the model not to retry: {}",
            rejected.content
        );
        assert!(
            !rejected.content.contains("run the command again"),
            "the old retry wording must be gone: {}",
            rejected.content
        );
    }

    #[test]
    fn a_cancelled_call_never_tells_the_model_to_run_it_again() {
        let result = synthetic_cancelled_result("call_a");
        assert_eq!(result.status, HelperToolResultStatus::Error);
        assert!(
            !result.content.contains("run the command again"),
            "the stopgap text must not invite a retry: {}",
            result.content
        );
    }

    #[test]
    fn oversized_resume_batches_are_truncated_to_one_frame() {
        let results: Vec<HelperToolResult> = (0..10)
            .map(|index| HelperToolResult {
                tool_call_id: format!("call_{index}"),
                status: HelperToolResultStatus::Success,
                content: "x".repeat(512 * 1024),
            })
            .collect();

        let bounded = bounded_resume_results(results);

        assert_eq!(bounded.len(), 10, "no pending call may be dropped");
        let ids: BTreeSet<&str> = bounded
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect();
        assert_eq!(ids.len(), 10);
        for result in &bounded {
            assert!(
                result.content.len() <= 512 * 1024,
                "content was already bounded"
            );
            assert!(
                result.content.contains("truncated by standalone backend"),
                "truncation is visible to the model: {}",
                result.content
            );
        }
        assert!(
            resume_payload_size(&bounded) <= MAX_FRAME_BYTES,
            "the bounded resume payload must fit one frame: {} bytes",
            resume_payload_size(&bounded)
        );
    }

    #[test]
    fn a_compacting_turn_uses_the_compaction_deadline() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut session = session_with_turn(tx, Duration::from_secs(600));
        let turn = session.turn.as_mut().expect("turn");
        turn.compacting_since = Some(Instant::now() - Duration::from_secs(300));
        driver.sessions.insert("c1".to_string(), session);

        // Past the 120s stall deadline, but inside the 600s compaction one.
        let stalled = driver.expire_stalled_turns(&BridgeTimeouts {
            stall: Duration::from_secs(120),
            compaction: Duration::from_secs(600),
            ..timeouts(Duration::from_secs(120))
        });
        assert!(
            stalled.is_empty(),
            "a healthy compaction must not trip the stall watchdog"
        );

        let expired = driver.expire_stalled_turns(&BridgeTimeouts {
            stall: Duration::from_secs(120),
            compaction: Duration::from_secs(200),
            ..timeouts(Duration::from_secs(120))
        });
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].reason, ExpiryReason::CompactionTimeout);
    }

    #[test]
    fn stale_frames_do_not_refresh_the_live_turn_activity() {
        let mut driver = empty_driver();
        let (tx, _rx) = mpsc::unbounded_channel();
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        let stale = Envelope::new("assistant.delta", 1).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "an-old-turn".to_string(),
            exchange_id: "e".to_string(),
            generation: 1,
        });
        assert!(
            !driver.frame_refreshes_activity(&stale),
            "a stale turn's frames must not keep the live turn alive"
        );

        let stale_exchange = Envelope::new("assistant.delta", 2).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "t".to_string(),
            exchange_id: "an-old-exchange".to_string(),
            generation: 1,
        });
        assert!(!driver.frame_refreshes_activity(&stale_exchange));

        let live = Envelope::new("assistant.delta", 3).with_identity(&TurnIdentity {
            session_id: "c1".to_string(),
            turn_id: "t".to_string(),
            exchange_id: "e".to_string(),
            generation: 1,
        });
        assert!(driver.frame_refreshes_activity(&live));
    }

    #[test]
    fn an_expiry_with_no_live_stream_is_published_as_an_orphan() {
        let (orphan_tx, mut orphan_rx) = mpsc::unbounded_channel();
        let mut driver = empty_driver();
        driver.orphan_events = Some(orphan_tx);
        // The receiver is dropped, so the expiry event has nowhere to go: this
        // is the paused-approval shape where the app stopped reading the
        // exchange.
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut session = session_with_turn(tx, Duration::from_secs(300));
        let turn = session.turn.as_mut().expect("turn");
        turn.pending
            .insert("call_1".to_string(), pending_bash_call("call_1"));
        turn.pending_since = Some(Instant::now() - Duration::from_secs(300));
        driver.sessions.insert("c1".to_string(), session);

        let expired = driver.expire_stalled_turns(&BridgeTimeouts {
            pending_tools: Some(Duration::from_secs(120)),
            ..timeouts(Duration::from_secs(3600))
        });

        assert_eq!(expired.len(), 1);
        match orphan_rx.try_recv() {
            Ok(OrphanEvent {
                conversation_id,
                event: BridgeEvent::RunFailed { code, .. },
            }) => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(code, "tool_timeout");
            }
            other => panic!("expected an orphan tool-timeout event, got {other:?}"),
        }
    }

    #[test]
    fn failing_a_stream_that_is_gone_publishes_an_orphan() {
        let (orphan_tx, mut orphan_rx) = mpsc::unbounded_channel();
        let mut driver = empty_driver();
        driver.orphan_events = Some(orphan_tx);
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        driver
            .sessions
            .insert("c1".to_string(), session_with_turn(tx, Duration::ZERO));

        driver.fail_stream_for("c1", "helper_exited", "helper exited unexpectedly");

        match orphan_rx.try_recv() {
            Ok(OrphanEvent {
                conversation_id,
                event: BridgeEvent::ProtocolError { code, .. },
            }) => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(code, "helper_exited");
            }
            other => panic!("expected an orphan protocol error, got {other:?}"),
        }
    }

    fn session_spec(context_limit: u64, output_limit: u64) -> SessionSpec {
        SessionSpec {
            conversation_id: "c1".to_string(),
            working_dir: PathBuf::from("/tmp/work"),
            provider: ProviderProfile {
                id: "p1".into(),
                display_name: "Local".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                wire: crate::provider::WireProtocol::OpenAiChatCompletions,
                model_id: "model-x".into(),
                models: Vec::new(),
                disabled_models: Vec::new(),
                credential: crate::provider::CredentialRef::None,
                context_limit,
                output_limit,
                compat: Default::default(),
                reasoning: false,
                supports_image_input: false,
                headers: BTreeMap::new(),
                pricing: BTreeMap::new(),
            },
            api_key: None,
            session_file: None,
            system_prompt: None,
            load_context_files: false,
            max_context_file_bytes: 4096,
            data_dir: PathBuf::from("/tmp/warpi"),
            task_id: Some("t1".to_string()),
            create_task: false,
            subagents: None,
        }
    }

    #[test]
    fn session_open_payload_derives_nonzero_compaction_reserves() {
        let payload = session_open_payload(&session_spec(32_768, 4_096), false).expect("payload");
        assert!(payload.compaction.enabled);
        assert_eq!(payload.compaction.reserve_tokens, 4_096);
        assert_eq!(payload.compaction.keep_recent_tokens, 8_192);

        // The bridge sends the same numbers the helper's own
        // `deriveCompactionSettings` would choose, so neither side has to
        // trust the other's fallback.
        let large = session_open_payload(&session_spec(131_072, 8_192), false).expect("payload");
        assert_eq!(large.compaction.reserve_tokens, 8_192);
        assert_eq!(large.compaction.keep_recent_tokens, 20_000);

        let tiny = session_open_payload(&session_spec(8_192, 1_024), false).expect("payload");
        assert_eq!(tiny.compaction.reserve_tokens, 1_024);
        assert_eq!(tiny.compaction.keep_recent_tokens, 2_048);
    }

    #[test]
    fn session_open_payload_gates_subagents_on_the_helper_capability() {
        // No config: the payload stays byte-identical to the old sessions.
        let plain = session_open_payload(&session_spec(32_768, 4_096), true).expect("payload");
        assert!(plain.subagents.is_none());
        assert_eq!(
            serde_json::to_value(&plain).expect("serializes")["subagents"],
            serde_json::Value::Null,
            "an absent config must not add a subagents key"
        );

        // A disabled config is never forwarded, even when the helper can host
        // children.
        let mut disabled = session_spec(32_768, 4_096);
        disabled.subagents = Some(HelperSubagents {
            enabled: false,
            ..Default::default()
        });
        let payload = session_open_payload(&disabled, true).expect("payload");
        assert!(payload.subagents.is_none());

        // Enabled + capable: the config is forwarded with its budget.
        let mut enabled = session_spec(32_768, 4_096);
        enabled.subagents = Some(HelperSubagents {
            enabled: true,
            max_children: Some(2),
            max_depth: Some(1),
            budget: Some(crate::protocol::HelperSubagentBudget {
                max_turns: Some(8),
                deadline_seconds: Some(120),
                token_cap: Some(50_000),
                aggregate_token_cap: None,
                heartbeat_ms: Some(1_000),
            }),
        });
        let payload = session_open_payload(&enabled, true).expect("payload");
        let subagents = payload.subagents.expect("forwarded");
        assert!(subagents.enabled);
        assert_eq!(subagents.max_children, Some(2));
        assert_eq!(subagents.budget.expect("budget").max_turns, Some(8));

        // Enabled but incapable: the config is dropped so the helper never
        // registers the task tool.
        let payload = session_open_payload(&enabled, false).expect("payload");
        assert!(payload.subagents.is_none());
    }

    #[test]
    fn session_open_frame_serializes_nonzero_reserves() {
        let spec = session_spec(32_768, 4_096);
        let payload = session_open_payload(&spec, false).expect("payload");
        let json = serde_json::to_value(&payload).expect("serializes");
        let compaction = &json["compaction"];
        for field in ["reserve_tokens", "keep_recent_tokens"] {
            let value = compaction[field]
                .as_u64()
                .unwrap_or_else(|| panic!("{field} must serialize as an integer"));
            assert!(value > 0, "{field} must not be the zero override: {json}");
        }
        assert_eq!(compaction["reserve_tokens"], serde_json::json!(4_096));
        assert_eq!(compaction["keep_recent_tokens"], serde_json::json!(8_192));
    }
}
