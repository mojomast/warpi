/**
 * Pi session runtime for the standalone Warp agent backend.
 *
 * Responsibilities:
 * - Own Pi AgentSessions keyed by Warp conversation id, with explicit,
 *   fork-private agent/session directories and no ambient configuration.
 * - Register exactly one provider profile per session (so two sessions with
 *   different endpoints/keys never share a client).
 * - Drive one turn at a time per session, suspend on brokered tool calls,
 *   resume the same Pi prompt when Warp delivers results, and settle the turn
 *   exactly once.
 * - Emit only bounded, identity-stamped frames. Never write to stdout here.
 *
 * Pi owns the model loop and the canonical transcript. Warp owns approvals and
 * execution. This helper only brokers between them.
 */

import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import {
  createAgentSession,
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type AgentSession,
  type AgentSessionEvent,
  type AgentToolResult,
  type ContextUsage,
  type SessionEntry,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import {
  MAX_EVENT_TEXT_BYTES,
  MAX_RESULT_CONTENT_BYTES,
  PROTOCOL_VERSION,
  truncateUtf8,
  type AgentUsage,
  type CompactData,
  type ContextUsageSource,
  type ProviderConfig,
  type RuntimeEvent,
  type SessionOpenData,
  type ToolCallSpec,
  type TurnResumeData,
  type TurnStartData,
} from "./protocol.js";
import {
  createWorkspaceTools,
  STANDALONE_SYSTEM_GUIDANCE,
  WORKSPACE_TOOL_NAMES,
  WorkspaceToolBroker,
  type WorkspaceToolName,
} from "./workspace-tools.js";
import {
  addUsage,
  childContextHeader,
  childSystemPrompt,
  CHILD_TOOL_NAMES,
  countToolCallBlocks,
  emptyUsage,
  frameTaskResult,
  MAX_SUBAGENT_DEPTH,
  normalizeSubagentConfig,
  refusedTaskResult,
  resolveSubagentsEnabled,
  taskParameters,
  TASK_TOOL_DESCRIPTION,
  TASK_TOOL_NAME,
  usageTokens,
  type NormalizedSubagentConfig,
  type SubagentType,
  type TaskParameters,
  type TaskStatus,
} from "./subagents.js";

export const HELPER_VERSION = "0.1.0";

/** Identity attached to every outbound event. */
export interface TurnIdentity {
  session_id: string;
  generation: number;
  turn_id: string;
  exchange_id: string;
}

export interface SessionIdentity {
  session_id: string;
  generation: number;
  exchange_id: string;
}

export interface Emitter {
  emit(event: RuntimeEvent, identity: Partial<TurnIdentity>): void;
  diagnostic(level: "info" | "warn" | "error", message: string): void;
}

interface SessionState {
  ownerKey: string;
  session: AgentSession;
  broker: WorkspaceToolBroker;
  identity: SessionIdentity;
  unsubscribe: () => void;
  active: boolean;
  turnId: string;
  exchangeId: string;
  runToken: number;
  abortRequested: boolean;
  promptSettled: Promise<void>;
  markPromptSettled: () => void;
  assistantMessageCounter: number;
  currentAssistantMessageId: string | undefined;
  contextWindow: number;
  maxOutputTokens: number;
  /** Wall-clock marks used to approximate per-message generation timings. */
  assistantRequestStartedAt: number | undefined;
  firstTokenAt: number | undefined;
  compactionStartedAt: number | undefined;
  /** Session-open configuration reused to build child sessions. */
  openConfig: SessionOpenData;
  workingDir: string;
  subagentConfig: NormalizedSubagentConfig;
  /** Always 0 for parent sessions; children are never SessionStates. */
  subagentDepth: number;
  childCounter: number;
  childrenStarted: number;
  /** Children started in the current parent turn (budget-scoped). */
  tasksStartedThisTurn: number;
  /** Child tokens accumulated in the current parent turn. */
  childTokensThisTurn: number;
  liveChildren: Map<string, ChildRun>;
}

interface TaskOutcome {
  status: TaskStatus;
  reason?: string;
  text: string;
  turns: number;
  toolCalls: number;
  usage: AgentUsage;
  wallMs: number;
}

/** One foreground child session owned by a parent turn. */
interface ChildRun {
  parent: SessionState;
  taskId: string;
  childId: string;
  description: string;
  kind: SubagentType;
  /** Parent turn identity captured at start; task events must reach the ledger even after cancel. */
  identity: TurnIdentity;
  session: AgentSession;
  unsubscribe: () => void;
  cleanupAbort: (() => void) | undefined;
  startedAt: number;
  maxTurns: number;
  tokenCap: number;
  turns: number;
  toolCalls: number;
  usage: AgentUsage;
  abortStatus: TaskStatus | undefined;
  abortReason: string | undefined;
  promptSettled: Promise<void>;
  markPromptSettled: () => void;
  settle: Promise<TaskOutcome>;
  settleResolve: (outcome: TaskOutcome) => void;
  outcome: TaskOutcome | undefined;
  heartbeat: NodeJS.Timeout | undefined;
  deadline: NodeJS.Timeout | undefined;
  settled: boolean;
}

/** Structural subset of the SDK `Usage` type the helper consumes. */
export interface SdkUsageLike {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  reasoning?: number;
  totalTokens: number;
  cost?: { input: number; output: number; cacheRead: number; cacheWrite: number; total: number };
}

interface AssistantMessageLike {
  role?: string;
  content?: Array<{ type?: string; text?: string }>;
  api?: string;
  model?: string;
  stopReason?: string;
  errorMessage?: string;
  usage?: SdkUsageLike;
}

/** Map SDK usage onto the wire shape; missing usage becomes zeros, never null. */
export function toAgentUsage(usage: SdkUsageLike | undefined): AgentUsage {
  if (usage === undefined) {
    return {
      input_tokens: 0,
      output_tokens: 0,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      total_tokens: 0,
    };
  }
  return {
    input_tokens: usage.input,
    output_tokens: usage.output,
    cache_read_tokens: usage.cacheRead,
    cache_write_tokens: usage.cacheWrite,
    ...(usage.reasoning !== undefined ? { reasoning_tokens: usage.reasoning } : {}),
    total_tokens: usage.totalTokens,
    ...(usage.cost !== undefined
      ? {
          cost: {
            input: usage.cost.input,
            output: usage.cost.output,
            cache_read: usage.cost.cacheRead,
            cache_write: usage.cost.cacheWrite,
            total: usage.cost.total,
          },
        }
      : {}),
  };
}

const CODE_NO_SESSION = "session_not_open";
const CODE_STALE_TURN = "stale_turn";
const CODE_UNKNOWN_TOOL_RESULT = "unknown_tool_result";
const CODE_PROVIDER_ERROR = "provider_error";
const CODE_MAX_OUTPUT = "max_output_tokens";
const CODE_INTERNAL = "internal_error";

function lastAssistantMessage(messages: readonly unknown[]): { stopReason?: string; text: string; usage?: AgentUsage } | undefined {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index] as {
      role?: string;
      content?: Array<{ type?: string; text?: string }>;
      stopReason?: string;
      errorMessage?: string;
      usage?: SdkUsageLike;
    };
    if (message?.role !== "assistant") continue;
    const text = (message.content ?? [])
      .filter((block) => block.type === "text")
      .map((block) => block.text ?? "")
      .join("");
    return {
      stopReason: message.stopReason,
      text: message.errorMessage ?? text,
      usage: message.usage === undefined ? undefined : toAgentUsage(message.usage),
    };
  }
  return undefined;
}

export function deriveCompactionSettings(contextWindow: number, maxOutputTokens: number) {
  const reserveTokens = Math.min(16384, maxOutputTokens, Math.max(2048, Math.floor(contextWindow / 4)));
  const keepRecentTokens = Math.min(20000, Math.max(2048, Math.floor(contextWindow / 4)));
  return { enabled: true, reserveTokens, keepRecentTokens };
}

/**
 * Repair a resumed transcript left with assistant tool calls that never got a
 * result (the crash / kill -9 shape: the turn was suspended on an approval and
 * the process died). A provider request built from such a transcript is
 * invalid, so every dangling call gets a synthetic error result before the
 * session opens. Returns the number of calls repaired.
 */
export function repairDanglingToolCalls(sessionManager: SessionManager): number {
  const answered = new Set<string>();
  const dangling: Array<{ id: string; name: string }> = [];
  for (const entry of sessionManager.getEntries() as SessionEntry[]) {
    if (entry.type !== "message") continue;
    const message = entry.message as unknown as {
      role?: string;
      toolCallId?: string;
      content?: Array<{ type?: string; id?: string; name?: string }>;
    };
    if (message.role === "assistant") {
      for (const block of message.content ?? []) {
        if (block.type === "toolCall" && typeof block.id === "string") {
          dangling.push({ id: block.id, name: block.name ?? "unknown" });
        }
      }
    } else if (message.role === "toolResult" && typeof message.toolCallId === "string") {
      answered.add(message.toolCallId);
    }
  }
  let repaired = 0;
  for (const call of dangling) {
    if (answered.has(call.id)) continue;
    sessionManager.appendMessage({
      role: "toolResult",
      toolCallId: call.id,
      toolName: call.name,
      content: [
        {
          type: "text",
          text: "This tool call never ran: the previous session stopped while it was waiting for approval. Do not wait for its output; continue, or make a new tool call if the work is still needed.",
        },
      ],
      isError: true,
      timestamp: Date.now(),
    } as never);
    answered.add(call.id);
    repaired += 1;
  }
  return repaired;
}

export class PiAgentRuntime {
  private readonly sessions = new Map<string, SessionState>();
  private readonly queues = new Map<string, Promise<void>>();
  private shuttingDown = false;

  constructor(private readonly emitter: Emitter) {}

  async handleShutdown(): Promise<void> {
    this.shuttingDown = true;
    const states = [...this.sessions.values()];
    this.sessions.clear();
    for (const state of states) {
      await this.disposeState(state).catch(() => undefined);
    }
  }

  async handleSessionOpen(identity: SessionIdentity, data: SessionOpenData): Promise<RuntimeEvent> {
    const existing = this.sessions.get(identity.session_id);
    if (existing !== undefined) {
      if (existing.identity.generation === identity.generation) {
        return this.openedEvent(existing, true);
      }
      await this.disposeState(existing);
      this.sessions.delete(identity.session_id);
    }
    const state = await this.createState(identity, data);
    this.sessions.set(identity.session_id, state);
    return this.openedEvent(state, false);
  }

  async handleTurnStart(identity: TurnIdentity, data: TurnStartData): Promise<RuntimeEvent[]> {
    const state = this.requireSessionIgnoringExchange(identity);
    if (state.active) {
      // A new user message supersedes an in-flight turn: release the suspended
      // Pi prompt before starting the replacement.
      await this.cancelState(state, "superseded by new user input");
    }
    const prompt = typeof data.prompt === "string" ? data.prompt : "";
    if (prompt.trim().length === 0) {
      return [
        {
          type: "turn.failed",
          code: CODE_INTERNAL,
          message: "turn.start requires a non-empty prompt",
          retryable: false,
        },
      ];
    }
    state.identity = { session_id: identity.session_id, generation: identity.generation, exchange_id: identity.exchange_id };
    state.active = true;
    state.abortRequested = false;
    state.turnId = identity.turn_id;
    state.exchangeId = identity.exchange_id;
    state.assistantMessageCounter = 0;
    state.currentAssistantMessageId = undefined;
    state.tasksStartedThisTurn = 0;
    state.childTokensThisTurn = 0;
    const runToken = ++state.runToken;
    state.promptSettled = new Promise<void>((resolve) => {
      state.markPromptSettled = resolve;
    });
    void this.runPrompt(state, runToken, prompt);
    return [{ type: "turn.started" }];
  }

  async handleTurnResume(identity: TurnIdentity, data: TurnResumeData): Promise<RuntimeEvent[]> {
    const state = this.requireSessionIgnoringExchange(identity);
    this.requireActiveTurn(state, identity);
    // Subsequent events belong to the resuming exchange; keep the identity the
    // backend uses for correlation in sync.
    state.identity = { session_id: identity.session_id, generation: identity.generation, exchange_id: identity.exchange_id };
    state.exchangeId = identity.exchange_id;
    const results = Array.isArray(data.results) ? data.results : [];
    if (results.length === 0) {
      return [
        { type: "turn.failed", code: CODE_INTERNAL, message: "turn.resume requires at least one tool result", retryable: false },
      ];
    }
    const pending = new Set(state.broker.pendingIdsFor(state.ownerKey));
    for (const result of results) {
      if (typeof result?.tool_call_id !== "string" || !pending.has(result.tool_call_id)) {
        if (result?.status === "rejected") {
          // A rejection for a call that no longer exists is a duplicate of an
          // earlier decision: acknowledge and drop it rather than failing.
          this.emitter.diagnostic("warn", `ignoring duplicate rejection for tool call ${String(result?.tool_call_id)}`);
          continue;
        }
        if (typeof result?.tool_call_id === "string" && state.broker.wasSettled(result.tool_call_id)) {
          // Late result for a call that was already delivered or cancelled
          // (typical for a child call abandoned by a budget/cancel abort):
          // drop it instead of failing the parent turn.
          this.emitter.diagnostic("warn", `ignoring late result for settled tool call ${result.tool_call_id}`);
          continue;
        }
        // Protocol violation: report it to the backend without touching the
        // suspended Pi turn. Warp decides how to reconcile the unknown outcome.
        throw new Error(
          `${CODE_UNKNOWN_TOOL_RESULT}: tool result does not match a pending tool call in this session/turn: ${String(result?.tool_call_id)}`,
        );
      }
      pending.delete(result.tool_call_id);
      const content = typeof result.content === "string" ? result.content : "";
      const bounded = truncateUtf8(content, MAX_RESULT_CONTENT_BYTES).text;
      if (result.status === "rejected") {
        state.broker.cancelCall(result.tool_call_id, `The user rejected this tool call. ${bounded}`);
      } else {
        const delivered = state.broker.deliver(result.tool_call_id, bounded, result.status === "error");
        if (!delivered) {
          throw new Error(
            `${CODE_UNKNOWN_TOOL_RESULT}: tool result could not be delivered (already settled): ${result.tool_call_id}`,
          );
        }
      }
    }
    return [];
  }

  async handleTurnCancel(identity: TurnIdentity, reason: string): Promise<RuntimeEvent[]> {
    const state = this.requireSessionIgnoringExchange(identity);
    if (state.turnId !== identity.turn_id) {
      return [
        {
          type: "turn.cancelled",
          reason: `stale turn ignored: ${identity.turn_id}`,
        },
      ];
    }
    await this.cancelState(state, reason.length > 0 ? reason : "cancelled by client");
    return [{ type: "turn.cancelled", reason: reason.length > 0 ? reason : "cancelled by client" }];
  }

  async handleCompact(identity: SessionIdentity, data: CompactData): Promise<RuntimeEvent[]> {
    const state = this.requireSession(identity);
    if (state.active) {
      return [{ type: "turn.failed", code: CODE_INTERNAL, message: "cannot compact while a turn is running", retryable: true }];
    }
    const startedAt = Date.now();
    const result = await state.session.compact(data.instructions);
    this.emitContextUpdated(state, state.identity, { estimate: result?.estimatedTokensAfter, defer: true });
    return [
      {
        type: "compaction.finished",
        reason: "manual",
        summarized: result !== undefined,
        ...(result?.tokensBefore !== undefined ? { tokens_before: result.tokensBefore } : {}),
        ...(result?.estimatedTokensAfter !== undefined ? { tokens_after: result.estimatedTokensAfter } : {}),
        ...(result?.usage !== undefined ? { summary_usage: toAgentUsage(result.usage) } : {}),
        duration_ms: Math.max(0, Date.now() - startedAt),
      },
    ];
  }

  sessionIds(): string[] {
    return [...this.sessions.keys()];
  }

  /** Serialize frame handling per session; prompts themselves run detached. */
  async enqueue(sessionId: string, operation: () => Promise<void>): Promise<void> {
    const previous = this.queues.get(sessionId) ?? Promise.resolve();
    const current = previous.catch(() => undefined).then(operation);
    this.queues.set(sessionId, current);
    try {
      await current;
    } finally {
      if (this.queues.get(sessionId) === current) this.queues.delete(sessionId);
    }
  }

  /**
   * Frames that open or terminate an exchange (turn.start, turn.resume,
   * turn.cancel) legitimately carry an exchange id the helper has not seen, so
   * only the session identity is validated here. The turn identity is checked
   * separately where it matters.
   */
  private requireSessionIgnoringExchange(identity: { session_id: string; generation: number }): SessionState {
    return this.requireSession({ session_id: identity.session_id, generation: identity.generation });
  }

  private requireSession(identity: { session_id: string; generation: number; exchange_id?: string }): SessionState {
    const state = this.sessions.get(identity.session_id);
    if (state === undefined) throw new Error(`${CODE_NO_SESSION}: ${identity.session_id}`);
    if (state.identity.generation !== identity.generation) {
      throw new Error(`stale session generation: received ${identity.generation}, current ${state.identity.generation}`);
    }
    if (identity.exchange_id !== undefined && state.identity.exchange_id !== identity.exchange_id) {
      throw new Error(`stale exchange: received ${identity.exchange_id}, current ${state.identity.exchange_id}`);
    }
    return state;
  }

  private requireActiveTurn(state: SessionState, identity: TurnIdentity): void {
    if (!state.active || state.turnId !== identity.turn_id) {
      throw new Error(`${CODE_STALE_TURN}: ${identity.turn_id}`);
    }
    if (state.broker.pendingIdsFor(state.ownerKey).length === 0) {
      throw new Error(`turn ${identity.turn_id} has no suspended tool calls to resume`);
    }
  }

  private openedEvent(state: SessionState, resumed: boolean): RuntimeEvent {
    return {
      type: "session.opened",
      resumed,
      session_file: state.session.sessionFile,
      session_id: state.identity.session_id,
      active_tools: state.session.getActiveToolNames().slice().sort(),
      model_id: state.session.model?.id ?? "unknown",
      working_dir: state.session.sessionManager.getCwd?.() ?? "",
      context_window: state.contextWindow,
      max_output_tokens: state.maxOutputTokens,
    };
  }

  private async createState(identity: SessionIdentity, config: SessionOpenData): Promise<SessionState> {
    if (typeof config.agent_dir !== "string" || config.agent_dir.trim().length === 0) {
      throw new Error("session.open requires an explicit agent_dir");
    }
    if (typeof config.session_dir !== "string" || config.session_dir.trim().length === 0) {
      throw new Error("session.open requires an explicit session_dir");
    }
    if (typeof config.working_dir !== "string" || config.working_dir.trim().length === 0) {
      throw new Error("session.open requires an explicit working_dir");
    }
    const agentDir = config.agent_dir;
    const sessionDir = config.session_dir;
    const cwd = config.working_dir;
    await mkdir(agentDir, { recursive: true, mode: 0o700 });
    await mkdir(sessionDir, { recursive: true, mode: 0o700 });

    const derived = deriveCompactionSettings(config.provider.context_window, config.provider.max_output_tokens);
    const settingsManager = SettingsManager.inMemory({
      compaction: {
        enabled: config.compaction?.enabled ?? derived.enabled,
        reserveTokens: config.compaction?.reserve_tokens ?? derived.reserveTokens,
        keepRecentTokens: config.compaction?.keep_recent_tokens ?? derived.keepRecentTokens,
      },
      retry: {
        enabled: config.retry?.enabled ?? true,
        maxRetries: config.retry?.max_retries,
        baseDelayMs: config.retry?.base_delay_ms,
      },
      // Never install packages or phone home from a standalone build.
      enableInstallTelemetry: false,
      enableAnalytics: false,
    });

    const maxContextBytes = config.max_context_file_bytes ?? 64 * 1024;
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      noContextFiles: config.load_context_files !== true,
      appendSystemPrompt: [
        ...(typeof config.system_prompt === "string" && config.system_prompt.length > 0
          ? [config.system_prompt]
          : []),
        STANDALONE_SYSTEM_GUIDANCE,
      ],
      agentsFilesOverride: (base) => ({
        agentsFiles: base.agentsFiles.map((file) => {
          const bounded = truncateUtf8(file.content, maxContextBytes);
          return { path: file.path, content: bounded.text };
        }),
      }),
    });
    await resourceLoader.reload();

    const { modelRuntime, model } = await this.buildModel(config.provider);

    const sessionManager =
      typeof config.session_file === "string" && config.session_file.length > 0 && existsSync(config.session_file)
        ? SessionManager.open(config.session_file, sessionDir, cwd)
        : SessionManager.create(cwd, sessionDir);
    const repairedCalls = repairDanglingToolCalls(sessionManager);
    if (repairedCalls > 0) {
      this.emitter.diagnostic(
        "warn",
        `repaired ${repairedCalls} tool call(s) left without a result by an earlier session`,
      );
    }

    const subagentConfig = normalizeSubagentConfig(config.subagents, resolveSubagentsEnabled(config.subagents, process.env.WARPI_SUBAGENTS));
    if (config.subagents?.max_depth !== undefined && config.subagents.max_depth > MAX_SUBAGENT_DEPTH) {
      this.emitter.diagnostic(
        "warn",
        `subagents.max_depth ${config.subagents.max_depth} is not supported in v1; using depth ${MAX_SUBAGENT_DEPTH}`,
      );
    }
    const ownerKey = identity.session_id;
    const broker = new WorkspaceToolBroker({
      emitToolBatch: (owner, calls) => this.emitToolBatch(owner, calls),
      emitDiagnostic: (level, message) => this.emitter.diagnostic(level, message),
    });
    const customTools = createWorkspaceTools(ownerKey, broker) as unknown as ToolDefinition[];
    const toolNames: string[] = [...WORKSPACE_TOOL_NAMES];
    if (subagentConfig.enabled) {
      customTools.push(this.createTaskTool(identity.session_id));
      toolNames.push(TASK_TOOL_NAME);
    }
    const { session } = await createAgentSession({
      cwd,
      agentDir,
      modelRuntime,
      model,
      noTools: "all",
      tools: toolNames,
      customTools,
      resourceLoader,
      sessionManager,
      settingsManager,
    });
    session.setActiveToolsByName(toolNames);
    const active = session.getActiveToolNames().slice().sort();
    const expected = [...toolNames].sort();
    if (JSON.stringify(active) !== JSON.stringify(expected)) {
      session.dispose();
      throw new Error(`active tool set mismatch: expected ${expected.join(",")} but got ${active.join(",")}`);
    }

    const state: SessionState = {
      ownerKey,
      session,
      broker,
      identity,
      unsubscribe: () => undefined,
      active: false,
      turnId: "",
      exchangeId: identity.exchange_id,
      runToken: 0,
      abortRequested: false,
      promptSettled: Promise.resolve(),
      markPromptSettled: () => undefined,
      assistantMessageCounter: 0,
      currentAssistantMessageId: undefined,
      contextWindow: config.provider.context_window,
      maxOutputTokens: config.provider.max_output_tokens,
      assistantRequestStartedAt: undefined,
      firstTokenAt: undefined,
      compactionStartedAt: undefined,
      openConfig: config,
      workingDir: cwd,
      subagentConfig,
      subagentDepth: 0,
      childCounter: 0,
      childrenStarted: 0,
      tasksStartedThisTurn: 0,
      childTokensThisTurn: 0,
      liveChildren: new Map(),
    };
    state.unsubscribe = session.subscribe((event) => this.onSessionEvent(state, event));
    return state;
  }

  private async buildModel(provider: ProviderConfig): Promise<{ modelRuntime: ModelRuntime; model: NonNullable<ReturnType<ModelRuntime["getModel"]>> }> {
    const modelRuntime = await ModelRuntime.create({ modelsPath: null, refreshOnCreate: false, allowModelNetwork: false });
    modelRuntime.registerProvider(provider.provider_id, providerRegistration(provider));
    const model = modelRuntime.getModel(provider.provider_id, provider.model_id);
    if (model === undefined) {
      throw new Error(`model ${provider.provider_id}/${provider.model_id} was not registered`);
    }
    return { modelRuntime, model };
  }

  // -------------------------------------------------------------------------
  // Subagents (`task` tool): foreground, read-only, depth-1 child sessions.
  // -------------------------------------------------------------------------

  private createTaskTool(sessionId: string): ToolDefinition {
    return {
      name: TASK_TOOL_NAME,
      label: TASK_TOOL_NAME,
      description: TASK_TOOL_DESCRIPTION,
      parameters: taskParameters,
      executionMode: "sequential" as const,
      execute: async (
        _toolCallId: string,
        args: TaskParameters,
        signal: AbortSignal | undefined,
      ): Promise<AgentToolResult<unknown>> => {
        const state = this.sessions.get(sessionId);
        if (state === undefined) {
          return textToolResult(
            refusedTaskResult(args.description, args.subagent_type ?? "explore", "error", "The session is closed."),
          );
        }
        return textToolResult(await this.runTask(state, args, signal));
      },
    } as ToolDefinition;
  }

  /**
   * Foreground child lifecycle: validate budgets, create the child session,
   * run one prompt to completion, return the framed `task_result`. Never
   * throws: every refusal or failure comes back as an untrusted envelope the
   * parent can adapt to.
   */
  private async runTask(state: SessionState, args: TaskParameters, signal: AbortSignal | undefined): Promise<string> {
    const kind: SubagentType = args.subagent_type ?? "explore";
    const description = args.description.slice(0, 120);
    const refused = (status: TaskStatus, reason: string): string => refusedTaskResult(description, kind, status, reason);
    const cfg = state.subagentConfig;

    if (!cfg.enabled) return refused("error", "Subagents are disabled for this session.");
    if (state.subagentDepth >= MAX_SUBAGENT_DEPTH) {
      return refused("error", `Subagents cannot spawn children (depth limit ${MAX_SUBAGENT_DEPTH}).`);
    }
    if (state.liveChildren.size > 0) {
      return refused("error", "Another subagent is still running in this turn; wait for its task_result.");
    }
    if (state.tasksStartedThisTurn >= cfg.maxChildren) {
      return refused("budget_exceeded", `Per-turn subagent limit (${cfg.maxChildren}) reached.`);
    }
    if (state.childrenStarted >= cfg.maxChildrenPerSession) {
      return refused("budget_exceeded", `Per-session subagent limit (${cfg.maxChildrenPerSession}) reached.`);
    }
    if (state.childTokensThisTurn >= cfg.aggregateTokenCap) {
      return refused("budget_exceeded", `Aggregate child token budget (${cfg.aggregateTokenCap}) reached for this turn.`);
    }
    const identity = this.currentTurnIdentity(state);
    if (identity === undefined) return refused("error", "The parent turn is no longer active.");

    const taskId = `task-${++state.childCounter}`;
    const childId = `${state.identity.session_id}:${taskId}`;
    const maxTurns = Math.max(1, Math.min(40, Math.floor(args.max_turns ?? cfg.maxTurns)));
    const deadlineMs = Math.max(
      1_000,
      Math.min(1_800_000, Math.floor((args.deadline_seconds ?? cfg.deadlineMs / 1000) * 1000)),
    );
    const startedAt = Date.now();
    state.tasksStartedThisTurn += 1;
    state.childrenStarted += 1;
    this.emitter.emit(
      {
        type: "task.started",
        task_id: taskId,
        child_session_id: childId,
        description,
        subagent_type: kind,
        prompt_bytes: Buffer.byteLength(args.prompt, "utf8"),
        max_turns: maxTurns,
        deadline_ms: deadlineMs,
        token_cap: cfg.tokenCap,
      },
      identity,
    );

    let session: AgentSession;
    try {
      session = await this.createChildSession(state, taskId, childId, kind);
    } catch (error) {
      const reason = `failed to create the child session: ${describeError(error)}`;
      const framed = frameTaskResult({
        description,
        kind,
        status: "error",
        turns: 0,
        toolCalls: 0,
        usage: emptyUsage(),
        wallMs: Math.max(0, Date.now() - startedAt),
        body: reason,
      });
      this.emitter.emit(
        {
          type: "task.completed",
          task_id: taskId,
          child_session_id: childId,
          status: "error",
          reason,
          subagent_type: kind,
          turns: 0,
          tool_calls: 0,
          usage: emptyUsage(),
          wall_ms: Math.max(0, Date.now() - startedAt),
          summary_bytes: framed.summaryBytes,
        },
        identity,
      );
      return framed.text;
    }

    let settleResolve: (outcome: TaskOutcome) => void = () => undefined;
    const settle = new Promise<TaskOutcome>((resolve) => {
      settleResolve = resolve;
    });
    const child: ChildRun = {
      parent: state,
      taskId,
      childId,
      description,
      kind,
      identity,
      session,
      unsubscribe: () => undefined,
      cleanupAbort: undefined,
      startedAt,
      maxTurns,
      tokenCap: cfg.tokenCap,
      turns: 0,
      toolCalls: 0,
      usage: emptyUsage(),
      abortStatus: undefined,
      abortReason: undefined,
      promptSettled: Promise.resolve(),
      markPromptSettled: () => undefined,
      settle,
      settleResolve,
      outcome: undefined,
      heartbeat: undefined,
      deadline: undefined,
      settled: false,
    };
    child.promptSettled = new Promise<void>((resolve) => {
      child.markPromptSettled = resolve;
    });
    child.unsubscribe = session.subscribe((event) => this.onChildEvent(child, event));
    if (signal !== undefined) {
      const onAbort = (): void => {
        void this.abortChild(child, "cancelled", "the parent tool call was aborted");
      };
      if (signal.aborted) onAbort();
      else {
        signal.addEventListener("abort", onAbort, { once: true });
        child.cleanupAbort = () => signal.removeEventListener("abort", onAbort);
      }
    }
    state.liveChildren.set(taskId, child);
    child.heartbeat = setInterval(() => this.emitTaskProgress(child), cfg.heartbeatMs);
    child.deadline = setTimeout(() => {
      void this.abortChild(child, "timeout", `wall-clock budget (${deadlineMs} ms) reached`);
    }, deadlineMs);

    const prompt = `${childContextHeader(kind, state.workingDir, description)}\n\n${args.prompt}`;
    void this.runChildPrompt(child, prompt).catch((error) => {
      this.emitter.diagnostic("error", `subagent ${taskId} failed unexpectedly: ${describeError(error)}`);
      this.finalizeChild(child, error);
    });
    const outcome = await child.settle;
    return outcome.text;
  }

  private async createChildSession(
    state: SessionState,
    taskId: string,
    childId: string,
    kind: SubagentType,
  ): Promise<AgentSession> {
    const config = state.openConfig;
    const parentSegment = safePathSegment(state.identity.session_id);
    const childSessionDir = join(config.session_dir, "children", parentSegment);
    const childAgentDir = join(config.agent_dir, "children", parentSegment, taskId);
    await mkdir(childSessionDir, { recursive: true, mode: 0o700 });
    await mkdir(childAgentDir, { recursive: true, mode: 0o700 });

    const derived = deriveCompactionSettings(config.provider.context_window, config.provider.max_output_tokens);
    const settingsManager = SettingsManager.inMemory({
      compaction: {
        enabled: config.compaction?.enabled ?? derived.enabled,
        reserveTokens: config.compaction?.reserve_tokens ?? derived.reserveTokens,
        keepRecentTokens: config.compaction?.keep_recent_tokens ?? derived.keepRecentTokens,
      },
      retry: {
        enabled: config.retry?.enabled ?? true,
        maxRetries: config.retry?.max_retries,
        baseDelayMs: config.retry?.base_delay_ms,
      },
      enableInstallTelemetry: false,
      enableAnalytics: false,
    });
    const resourceLoader = new DefaultResourceLoader({
      cwd: state.workingDir,
      agentDir: childAgentDir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      noContextFiles: true,
      appendSystemPrompt: [childSystemPrompt(kind), STANDALONE_SYSTEM_GUIDANCE],
    });
    await resourceLoader.reload();

    const { modelRuntime, model } = await this.buildModel(config.provider);
    const sessionManager = SessionManager.create(state.workingDir, childSessionDir, {
      id: taskId,
      parentSession: state.identity.session_id,
    });
    const repairedCalls = repairDanglingToolCalls(sessionManager);
    if (repairedCalls > 0) {
      this.emitter.diagnostic("warn", `repaired ${repairedCalls} dangling call(s) in child session ${childId}`);
    }

    const childTools = createWorkspaceTools(
      state.ownerKey,
      state.broker,
      CHILD_TOOL_NAMES,
      childId,
    ) as unknown as ToolDefinition[];
    const { session } = await createAgentSession({
      cwd: state.workingDir,
      agentDir: childAgentDir,
      modelRuntime,
      model,
      noTools: "all",
      tools: [...CHILD_TOOL_NAMES],
      customTools: childTools,
      resourceLoader,
      sessionManager,
      settingsManager,
    });
    session.setActiveToolsByName([...CHILD_TOOL_NAMES]);
    const active = session.getActiveToolNames().slice().sort();
    const expected = [...CHILD_TOOL_NAMES].sort();
    if (JSON.stringify(active) !== JSON.stringify(expected)) {
      session.dispose();
      throw new Error(`child tool set mismatch: expected ${expected.join(",")} but got ${active.join(",")}`);
    }
    return session;
  }

  private async runChildPrompt(child: ChildRun, prompt: string): Promise<void> {
    let failure: unknown;
    try {
      await child.session.prompt(prompt);
    } catch (error) {
      failure = error;
    } finally {
      child.markPromptSettled();
    }
    this.finalizeChild(child, failure);
  }

  private onChildEvent(child: ChildRun, event: AgentSessionEvent): void {
    if (this.shuttingDown || child.settled) return;
    switch (event.type) {
      case "message_start": {
        const message = event.message as { role?: string };
        if (message?.role !== "assistant") return;
        child.turns += 1;
        if (child.turns > child.maxTurns && child.abortStatus === undefined) {
          void this.abortChild(child, "budget_exceeded", `model round trip limit (${child.maxTurns}) reached`);
        }
        return;
      }
      case "message_end": {
        const message = event.message as AssistantMessageLike;
        if (message.role !== "assistant") return;
        child.usage = addUsage(child.usage, toAgentUsage(message.usage));
        child.toolCalls += countToolCallBlocks(message);
        if (usageTokens(child.usage) > child.tokenCap && child.abortStatus === undefined) {
          void this.abortChild(child, "budget_exceeded", `token budget (${child.tokenCap}) reached`);
        }
        return;
      }
      case "agent_end": {
        if (event.willRetry) {
          this.emitter.diagnostic("info", `subagent ${child.taskId} scheduled a provider retry`);
        }
        return;
      }
      default:
        return;
    }
  }

  private emitTaskProgress(child: ChildRun): void {
    if (child.settled || this.shuttingDown) return;
    const usage = child.usage;
    this.emitter.emit(
      {
        type: "task.progress",
        task_id: child.taskId,
        child_session_id: child.childId,
        elapsed_ms: Math.max(0, Date.now() - child.startedAt),
        turns: child.turns,
        tool_calls: child.toolCalls,
        tokens: {
          input_tokens: usage.input_tokens,
          output_tokens: usage.output_tokens,
          total_tokens: usageTokens(usage),
        },
        pending_tools: child.parent.broker.pendingIdsForAgent(child.childId).length,
      },
      this.liveChildIdentity(child),
    );
  }

  /**
   * The turn identity a child event must carry to keep refreshing the bridge's
   * stall watchdog: the parent exchange id changes on every `turn.resume`, so
   * prefer the current identity and fall back to the one captured at start.
   */
  private liveChildIdentity(child: ChildRun): Partial<TurnIdentity> {
    return this.currentTurnIdentity(child.parent) ?? child.identity;
  }

  /**
   * Abort a live child and wait for it to settle. Idempotent: the first
   * terminal cause wins, and the settle path runs exactly once.
   */
  private async abortChild(child: ChildRun, status: TaskStatus, reason: string): Promise<void> {
    if (child.settled || child.abortStatus !== undefined) return;
    child.abortStatus = status;
    child.abortReason = reason;
    child.parent.broker.cancelAgent(child.childId, reason);
    await child.session.abort().catch(() => undefined);
    await Promise.race([child.promptSettled, delay(5_000)]);
    this.finalizeChild(child, undefined);
  }

  private finalizeChild(child: ChildRun, promptError: unknown): TaskOutcome {
    if (child.settled) {
      return child.outcome ?? emptyTaskOutcome();
    }
    child.settled = true;
    if (child.heartbeat !== undefined) clearInterval(child.heartbeat);
    if (child.deadline !== undefined) clearTimeout(child.deadline);
    child.cleanupAbort?.();
    child.unsubscribe();

    const last = lastAssistantMessage(child.session.messages as unknown[]);
    let status: TaskStatus;
    let reason: string | undefined;
    if (child.abortStatus !== undefined) {
      status = child.abortStatus;
      reason = child.abortReason;
    } else if (promptError !== undefined) {
      status = "error";
      reason = describeError(promptError);
    } else if (last === undefined) {
      status = "error";
      reason = "the child produced no assistant message";
    } else if (last.stopReason === "aborted") {
      status = "cancelled";
      reason = "the child provider call was aborted";
    } else if (last.stopReason === "error") {
      status = "error";
      reason = last.text || "the child provider reported an error";
    } else if (last.stopReason === "length") {
      status = "error";
      reason = "the child stopped because the output or context limit was reached";
    } else {
      status = "ok";
    }
    const usage =
      usageTokens(child.usage) === 0 && last?.usage !== undefined ? last.usage : child.usage;
    const lastText = (last?.text ?? "").trim();
    const body =
      status === "ok"
        ? lastText.length > 0
          ? lastText
          : "The subagent finished without a final message."
        : [reason ?? "The subagent did not complete.", lastText].filter((part) => part.length > 0).join("\n\n");
    const wallMs = Math.max(0, Date.now() - child.startedAt);
    const framed = frameTaskResult({
      description: child.description,
      kind: child.kind,
      status,
      turns: child.turns,
      toolCalls: child.toolCalls,
      usage,
      wallMs,
      body,
    });
    child.session.dispose();
    child.parent.liveChildren.delete(child.taskId);
    child.parent.childTokensThisTurn += usageTokens(usage);
    const outcome: TaskOutcome = {
      status,
      ...(reason !== undefined ? { reason } : {}),
      text: framed.text,
      turns: child.turns,
      toolCalls: child.toolCalls,
      usage,
      wallMs,
    };
    child.outcome = outcome;
    this.emitter.emit(
      {
        type: "task.completed",
        task_id: child.taskId,
        child_session_id: child.childId,
        status,
        ...(reason !== undefined ? { reason } : {}),
        subagent_type: child.kind,
        turns: child.turns,
        tool_calls: child.toolCalls,
        usage,
        wall_ms: wallMs,
        summary_bytes: framed.summaryBytes,
      },
      this.liveChildIdentity(child),
    );
    child.settleResolve(outcome);
    return outcome;
  }

  private onSessionEvent(state: SessionState, event: AgentSessionEvent): void {
    if (this.shuttingDown) return;
    const identity = this.turnIdentity(state);
    if (identity === undefined) return;
    switch (event.type) {
      case "turn_start": {
        state.assistantRequestStartedAt = Date.now();
        state.firstTokenAt = undefined;
        return;
      }
      case "message_start": {
        const message = event.message as { role?: string };
        if (message.role === "assistant") {
          state.assistantMessageCounter += 1;
          state.currentAssistantMessageId = `${state.turnId}:a${state.assistantMessageCounter}`;
          state.assistantRequestStartedAt ??= Date.now();
          state.firstTokenAt = undefined;
        }
        return;
      }
      case "message_update": {
        const update = event.assistantMessageEvent;
        const messageId = state.currentAssistantMessageId ?? `${state.turnId}:a${state.assistantMessageCounter}`;
        if (
          state.firstTokenAt === undefined &&
          (update.type === "text_delta" || update.type === "thinking_delta" || update.type === "toolcall_start")
        ) {
          state.firstTokenAt = Date.now();
        }
        if (update.type === "text_delta") {
          const bounded = truncateUtf8(update.delta, MAX_EVENT_TEXT_BYTES);
          if (bounded.text.length > 0) {
            this.emitter.emit({ type: "assistant.delta", message_id: messageId, text: bounded.text }, identity);
          }
        } else if (update.type === "thinking_delta") {
          const bounded = truncateUtf8(update.delta, MAX_EVENT_TEXT_BYTES);
          if (bounded.text.length > 0) {
            this.emitter.emit({ type: "assistant.reasoning", message_id: `${messageId}:r`, text: bounded.text }, identity);
          }
        }
        return;
      }
      case "message_end": {
        const message = event.message as AssistantMessageLike;
        if (message.role !== "assistant") return;
        const text = (message.content ?? [])
          .filter((block) => block.type === "text")
          .map((block) => block.text ?? "")
          .join("");
        const messageId = state.currentAssistantMessageId ?? `${state.turnId}:a${state.assistantMessageCounter}`;
        if (text.length > 0) {
          this.emitter.emit(
            { type: "assistant.message", message_id: messageId, text: truncateUtf8(text, MAX_EVENT_TEXT_BYTES).text },
            identity,
          );
        }
        this.emitAssistantUsage(state, identity, message, messageId);
        // Deferred: the SDK persists the message to the session manager after
        // notifying listeners, and its context reading consults session entries.
        this.emitContextUpdated(state, identity, { defer: true });
        return;
      }
      case "compaction_start": {
        state.compactionStartedAt = Date.now();
        this.emitter.emit({ type: "compaction.started", reason: event.reason }, identity);
        return;
      }
      case "compaction_end": {
        const result = event.result;
        this.emitter.emit(
          {
            type: "compaction.finished",
            reason: event.reason,
            summarized: result !== undefined,
            ...(result?.tokensBefore !== undefined ? { tokens_before: result.tokensBefore } : {}),
            ...(result?.estimatedTokensAfter !== undefined ? { tokens_after: result.estimatedTokensAfter } : {}),
            ...(result?.usage !== undefined ? { summary_usage: toAgentUsage(result.usage) } : {}),
            ...(state.compactionStartedAt !== undefined
              ? { duration_ms: Math.max(0, Date.now() - state.compactionStartedAt) }
              : {}),
          },
          identity,
        );
        state.compactionStartedAt = undefined;
        this.emitContextUpdated(state, identity, { estimate: result?.estimatedTokensAfter, defer: true });
        return;
      }
      case "agent_end": {
        if (event.willRetry) {
          this.emitter.diagnostic("info", "provider retry scheduled by Pi");
        }
        return;
      }
      default:
        return;
    }
  }

  private emitAssistantUsage(
    state: SessionState,
    identity: Partial<TurnIdentity>,
    message: AssistantMessageLike,
    messageId: string,
  ): void {
    const endedAt = Date.now();
    const startedAt = state.assistantRequestStartedAt;
    const durationMs = startedAt === undefined ? 0 : Math.max(0, endedAt - startedAt);
    const firstTokenMs =
      startedAt !== undefined && state.firstTokenAt !== undefined
        ? Math.max(0, state.firstTokenAt - startedAt)
        : undefined;
    const usage = toAgentUsage(message.usage);
    const tokensPerSecond =
      durationMs > 0 && usage.output_tokens > 0
        ? Math.round((usage.output_tokens / durationMs) * 10_000) / 10
        : undefined;
    this.emitter.emit(
      {
        type: "assistant.usage",
        message_id: messageId,
        model_id:
          typeof message.model === "string" && message.model.length > 0
            ? message.model
            : (state.session.model?.id ?? "unknown"),
        api: typeof message.api === "string" && message.api.length > 0 ? message.api : "unknown",
        usage,
        duration_ms: durationMs,
        ...(firstTokenMs !== undefined ? { first_token_ms: firstTokenMs } : {}),
        ...(tokensPerSecond !== undefined ? { output_tokens_per_second: tokensPerSecond } : {}),
        stop_reason:
          typeof message.stopReason === "string" && message.stopReason.length > 0 ? message.stopReason : "stop",
      },
      identity,
    );
    state.assistantRequestStartedAt = undefined;
    state.firstTokenAt = undefined;
  }

  private emitContextUpdated(
    state: SessionState,
    identity: Partial<TurnIdentity>,
    options: { estimate?: number | undefined; defer?: boolean } = {},
  ): void {
    const read = (): void => {
      if (this.shuttingDown) return;
      const usage = safeContextUsage(state.session);
      const window = usage !== undefined && usage.contextWindow > 0 ? usage.contextWindow : state.contextWindow;
      const tokens = options.estimate ?? usage?.tokens ?? null;
      const source: ContextUsageSource = options.estimate !== undefined ? "compaction_estimate" : "usage";
      const percent = tokens === null || window <= 0 ? null : sanitizePercent((tokens / window) * 100);
      this.emitter.emit(
        {
          type: "context.updated",
          tokens,
          ...(window > 0 ? { context_window: window } : {}),
          percent,
          source,
        },
        identity,
      );
    };
    if (options.defer === true) queueMicrotask(read);
    else read();
  }

  private turnIdentity(state: SessionState): Partial<TurnIdentity> | undefined {
    return this.currentTurnIdentity(state);
  }

  private currentTurnIdentity(state: SessionState): TurnIdentity | undefined {
    if (!state.active) return undefined;
    return {
      session_id: state.identity.session_id,
      generation: state.identity.generation,
      turn_id: state.turnId,
      exchange_id: state.exchangeId,
    };
  }

  private emitToolBatch(ownerKey: string, calls: ToolCallSpec[]): void {
    const state = this.sessions.get(ownerKey);
    const identity = state === undefined ? undefined : this.turnIdentity(state);
    if (state === undefined || identity === undefined) {
      // The owner went away (cancelled/disposed) between suspension and flush.
      // Reject rather than leaking a tool call nobody can resolve.
      state?.broker.cancelOwner(ownerKey, "session is no longer active");
      return;
    }
    const pending = state.broker.pendingIdsFor(ownerKey);
    this.emitter.emit({ type: "tool.calls", calls }, identity);
    this.emitter.emit({ type: "turn.awaiting_tools", pending }, identity);
  }

  private async runPrompt(state: SessionState, runToken: number, prompt: string): Promise<void> {
    let failure: { code: string; message: string; retryable: boolean } | undefined;
    try {
      await state.session.prompt(prompt);
    } catch (error) {
      if (state.runToken === runToken) {
        if (!state.abortRequested) failure = { code: CODE_PROVIDER_ERROR, message: describeError(error), retryable: false };
      }
    } finally {
      if (state.runToken === runToken) state.markPromptSettled();
    }
    if (state.runToken !== runToken) return;
    // Capture the identity before clearing `active`, otherwise the terminal
    // event would have nobody to reach.
    const identity = this.turnIdentity(state);
    state.active = false;
    if (identity === undefined) return;
    if (state.abortRequested) {
      // The cancel path (handleTurnCancel or supersede) owns the terminal event.
      return;
    }
    if (failure !== undefined) {
      this.emitter.emit({ type: "turn.failed", ...failure }, identity);
      return;
    }
    const last = lastAssistantMessage(state.session.messages as unknown[]);
    if (last?.stopReason === "aborted") {
      this.emitter.emit({ type: "turn.cancelled", reason: "cancelled" }, identity);
      return;
    }
    if (last?.stopReason === "error") {
      this.emitter.emit(
        { type: "turn.failed", code: CODE_PROVIDER_ERROR, message: last.text || "provider reported an error", retryable: false },
        identity,
      );
      return;
    }
    if (last?.stopReason === "length") {
      this.emitter.emit(
        {
          type: "turn.failed",
          code: CODE_MAX_OUTPUT,
          message: "the model stopped because the output or context limit was reached",
          retryable: false,
        },
        identity,
      );
      return;
    }
    this.emitter.emit(
      { type: "turn.completed", stop_reason: last?.stopReason ?? "stop", usage: last?.usage ?? null },
      identity,
    );
  }

  private async cancelState(state: SessionState, reason: string): Promise<void> {
    if (!state.active && state.liveChildren.size === 0) return;
    state.abortRequested = true;
    // Cascade first: children settle so their pending `task` tool calls resolve
    // and no child is ever left running after the parent turn is cancelled.
    const children = [...state.liveChildren.values()];
    const childSettles = children.map((child) =>
      this.abortChild(child, "cancelled", `parent turn cancelled: ${reason}`),
    );
    state.broker.cancelOwner(state.ownerKey, reason);
    if (children.length > 0) {
      await Promise.race([Promise.allSettled(childSettles), delay(10_000)]);
      for (const child of children) this.finalizeChild(child, undefined);
    }
    await state.session.abort().catch(() => undefined);
    await Promise.race([state.promptSettled, delay(10_000)]);
    state.active = false;
    state.runToken += 1;
  }

  private async disposeState(state: SessionState): Promise<void> {
    state.unsubscribe();
    state.abortRequested = true;
    const children = [...state.liveChildren.values()];
    const childSettles = children.map((child) =>
      this.abortChild(child, "cancelled", "session disposed"),
    );
    if (children.length > 0) {
      await Promise.race([Promise.allSettled(childSettles), delay(5_000)]);
      for (const child of children) this.finalizeChild(child, undefined);
    }
    state.broker.cancelOwner(state.ownerKey, "session disposed");
    await state.session.abort().catch(() => undefined);
    state.session.dispose();
  }
}

function emptyTaskOutcome(): TaskOutcome {
  return {
    status: "error",
    reason: "the task did not settle",
    text: "",
    turns: 0,
    toolCalls: 0,
    usage: emptyUsage(),
    wallMs: 0,
  };
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function textToolResult(text: string): AgentToolResult<unknown> {
  return { content: [{ type: "text", text }], details: {} };
}

function safePathSegment(value: string): string {
  const cleaned = value.replace(/[^A-Za-z0-9._-]/g, "_").replace(/^[^A-Za-z0-9]+/, "").slice(0, 128);
  return cleaned.length > 0 ? cleaned : "session";
}

function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

function safeContextUsage(session: AgentSession): ContextUsage | undefined {
  try {
    return session.getContextUsage();
  } catch {
    return undefined;
  }
}

/** Round a percentage to two decimals; non-finite or negative readings become null. */
function sanitizePercent(percent: number): number | null {
  if (!Number.isFinite(percent) || percent < 0) return null;
  return Math.round(percent * 100) / 100;
}

function providerRegistration(provider: ProviderConfig): Parameters<ModelRuntime["registerProvider"]>[1] {
  const authIsNone = provider.auth.kind === "none";
  // The SDK requires a configured credential before it will run a turn. For
  // auth=none we satisfy that preflight with a non-secret placeholder while
  // main.ts strips the Authorization header at the HTTP boundary for this
  // profile's origin. The auth=none test asserts that neither the placeholder
  // nor any Authorization header reaches the wire.
  const apiKey = provider.auth.kind === "none" ? "warpi-no-auth" : provider.auth.api_key;
  return {
    name: provider.name,
    baseUrl: provider.base_url,
    apiKey,
    api: provider.api,
    authHeader: !authIsNone,
    headers: provider.headers,
    models: [
      {
        id: provider.model_id,
        name: provider.model_name,
        api: provider.api,
        reasoning: provider.reasoning ?? false,
        input: provider.supports_image_input === true ? ["text", "image"] : ["text"],
        // Standalone mode never bills the user, so Pi's local cost estimate is
        // always zero. Warp never surfaces this value as pricing; see
        // PROVIDER_COMPATIBILITY.md.
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: provider.context_window,
        maxTokens: provider.max_output_tokens,
        compat: provider.compat,
      },
    ],
  };
}
