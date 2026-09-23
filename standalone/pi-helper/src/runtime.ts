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
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import {
  MAX_EVENT_TEXT_BYTES,
  MAX_RESULT_CONTENT_BYTES,
  PROTOCOL_VERSION,
  truncateUtf8,
  type AgentUsage,
  type CompactData,
  type ProviderConfig,
  type RuntimeEvent,
  type SessionOpenData,
  type ToolCallSpec,
  type TurnResumeData,
  type TurnStartData,
} from "./protocol.js";
import {
  createWorkspaceTools,
  WORKSPACE_TOOL_NAMES,
  WorkspaceToolBroker,
  type WorkspaceToolName,
} from "./workspace-tools.js";

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
      usage?: { input: number; output: number; cacheRead: number; cacheWrite: number; totalTokens: number };
    };
    if (message?.role !== "assistant") continue;
    const text = (message.content ?? [])
      .filter((block) => block.type === "text")
      .map((block) => block.text ?? "")
      .join("");
    const usage =
      message.usage === undefined
        ? undefined
        : {
            input_tokens: message.usage.input,
            output_tokens: message.usage.output,
            cache_read_tokens: message.usage.cacheRead,
            cache_write_tokens: message.usage.cacheWrite,
            total_tokens: message.usage.totalTokens,
          };
    return { stopReason: message.stopReason, text: message.errorMessage ?? text, usage };
  }
  return undefined;
}

export function deriveCompactionSettings(contextWindow: number, maxOutputTokens: number) {
  const reserveTokens = Math.min(16384, maxOutputTokens, Math.max(2048, Math.floor(contextWindow / 4)));
  const keepRecentTokens = Math.min(20000, Math.max(2048, Math.floor(contextWindow / 4)));
  return { enabled: true, reserveTokens, keepRecentTokens };
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
    const result = await state.session.compact(data.instructions);
    return [
      {
        type: "compaction.finished",
        reason: "manual",
        summarized: result !== undefined,
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
    if (!state.active || state.turnId !== identity.turn_id || state.turnId !== state.turnId) {
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
      appendSystemPrompt: typeof config.system_prompt === "string" && config.system_prompt.length > 0 ? [config.system_prompt] : [],
      agentsFilesOverride: (base) => ({
        agentsFiles: base.agentsFiles.map((file) => {
          const bounded = truncateUtf8(file.content, maxContextBytes);
          return { path: file.path, content: bounded.text };
        }),
      }),
    });
    await resourceLoader.reload();

    const modelRuntime = await ModelRuntime.create({ modelsPath: null, refreshOnCreate: false, allowModelNetwork: false });
    modelRuntime.registerProvider(config.provider.provider_id, providerRegistration(config.provider));
    const model = modelRuntime.getModel(config.provider.provider_id, config.provider.model_id);
    if (model === undefined) {
      throw new Error(`model ${config.provider.provider_id}/${config.provider.model_id} was not registered`);
    }

    const sessionManager =
      typeof config.session_file === "string" && config.session_file.length > 0 && existsSync(config.session_file)
        ? SessionManager.open(config.session_file, sessionDir, cwd)
        : SessionManager.create(cwd, sessionDir);

    const ownerKey = identity.session_id;
    const broker = new WorkspaceToolBroker({
      emitToolBatch: (owner, calls) => this.emitToolBatch(owner, calls),
      emitDiagnostic: (level, message) => this.emitter.diagnostic(level, message),
    });
    const customTools = createWorkspaceTools(ownerKey, broker) as unknown as ToolDefinition[];
    const { session } = await createAgentSession({
      cwd,
      agentDir,
      modelRuntime,
      model,
      noTools: "all",
      tools: [...WORKSPACE_TOOL_NAMES],
      customTools,
      resourceLoader,
      sessionManager,
      settingsManager,
    });
    session.setActiveToolsByName([...WORKSPACE_TOOL_NAMES]);
    const active = session.getActiveToolNames().slice().sort();
    const expected = [...WORKSPACE_TOOL_NAMES].sort();
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
    };
    state.unsubscribe = session.subscribe((event) => this.onSessionEvent(state, event));
    return state;
  }

  private onSessionEvent(state: SessionState, event: AgentSessionEvent): void {
    if (this.shuttingDown) return;
    const identity = this.turnIdentity(state);
    if (identity === undefined) return;
    switch (event.type) {
      case "message_start": {
        const message = event.message as { role?: string };
        if (message.role === "assistant") {
          state.assistantMessageCounter += 1;
          state.currentAssistantMessageId = `${state.turnId}:a${state.assistantMessageCounter}`;
        }
        return;
      }
      case "message_update": {
        const update = event.assistantMessageEvent;
        const messageId = state.currentAssistantMessageId ?? `${state.turnId}:a${state.assistantMessageCounter}`;
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
        const message = event.message as { role?: string; content?: Array<{ type?: string; text?: string }> };
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
        return;
      }
      case "compaction_start": {
        this.emitter.emit({ type: "compaction.started", reason: event.reason }, identity);
        return;
      }
      case "compaction_end": {
        this.emitter.emit(
          {
            type: "compaction.finished",
            reason: event.reason,
            summarized: event.result !== undefined,
          },
          identity,
        );
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

  private turnIdentity(state: SessionState): Partial<TurnIdentity> | undefined {
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

  private async cancelState(state: SessionState, reason: string): Promise<void> {    if (!state.active) return;
    state.abortRequested = true;
    state.broker.cancelOwner(state.ownerKey, reason);
    await state.session.abort().catch(() => undefined);
    await Promise.race([state.promptSettled, new Promise<void>((resolve) => setTimeout(resolve, 10_000))]);
    state.active = false;
    state.runToken += 1;
  }

  private async disposeState(state: SessionState): Promise<void> {
    state.unsubscribe();
    state.broker.cancelOwner(state.ownerKey, "session disposed");
    await state.session.abort().catch(() => undefined);
    state.session.dispose();
  }
}

function describeError(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
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
