/**
 * Private stdio protocol between the standalone Warp Rust backend and this
 * helper. Version 1.
 *
 * Design notes (see ../../../../docs/standalone/ARCHITECTURE.md):
 * - Newline-delimited JSON on stdin (Rust -> helper) and stdout (helper -> Rust).
 * - Only protocol frames go to stdout. Diagnostics go to stderr.
 * - Every session-scoped frame carries `session_id` and `generation`; every
 *   turn-scoped frame additionally carries `turn_id` and `exchange_id`. The
 *   helper echoes the identity it was given and never invents it.
 * - `seq` is a per-direction monotonic counter. Receivers reject regressions.
 * - Frames are bounded. Oversized or malformed frames are rejected without
 *   killing the helper.
 *
 * This file is the canonical TypeScript definition; the Rust mirror lives in
 * crates/standalone_agent/src/protocol.rs. Both must change together.
 */

export const PROTOCOL_VERSION = 1;

/** Maximum serialized frame size accepted in either direction. */
export const MAX_FRAME_BYTES = 4 * 1024 * 1024;
/** Maximum serialized tool-arguments payload accepted from the model. */
export const MAX_TOOL_ARGUMENT_BYTES = 512 * 1024;
/** Maximum single text payload in an event. */
export const MAX_EVENT_TEXT_BYTES = 1024 * 1024;
/** Maximum tool-result content accepted on resume. */
export const MAX_RESULT_CONTENT_BYTES = 4 * 1024 * 1024;
/** Maximum identifier length. */
export const MAX_ID_LENGTH = 256;

export interface Envelope {
  protocol: number;
  seq: number;
  kind: string;
  session_id?: string;
  turn_id?: string;
  exchange_id?: string;
  generation?: number;
  data?: unknown;
}

export class FrameError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "FrameError";
  }
}

function requireId(value: unknown, field: string): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_ID_LENGTH) {
    throw new FrameError("invalid_frame", `${field} must be a non-empty string of at most ${MAX_ID_LENGTH} characters`);
  }
  return value;
}

/** Parse and validate one inbound frame (Rust -> helper). */
export function parseFrame(line: string): Envelope {
  if (Buffer.byteLength(line, "utf8") > MAX_FRAME_BYTES) {
    throw new FrameError("frame_too_large", `frame exceeds ${MAX_FRAME_BYTES} bytes`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch (error) {
    throw new FrameError("invalid_json", `frame is not valid JSON: ${String(error)}`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new FrameError("invalid_frame", "frame must be a JSON object");
  }
  const record = parsed as Record<string, unknown>;
  if (record.protocol !== PROTOCOL_VERSION) {
    throw new FrameError(
      "protocol_mismatch",
      `expected protocol ${PROTOCOL_VERSION}, received ${String(record.protocol)}`,
    );
  }
  if (typeof record.seq !== "number" || !Number.isInteger(record.seq) || record.seq < 0) {
    throw new FrameError("invalid_frame", "seq must be a non-negative integer");
  }
  if (typeof record.kind !== "string" || record.kind.length === 0 || record.kind.length > 64) {
    throw new FrameError("invalid_frame", "kind must be a non-empty string of at most 64 characters");
  }
  if (record.generation !== undefined) {
    if (typeof record.generation !== "number" || !Number.isInteger(record.generation) || record.generation < 0) {
      throw new FrameError("invalid_frame", "generation must be a non-negative integer");
    }
  }
  if (record.data !== undefined && (typeof record.data !== "object" || record.data === null || Array.isArray(record.data))) {
    throw new FrameError("invalid_frame", "data must be an object when present");
  }
  return {
    protocol: PROTOCOL_VERSION,
    seq: record.seq,
    kind: record.kind,
    session_id: requireId(record.session_id, "session_id"),
    turn_id: requireId(record.turn_id, "turn_id"),
    exchange_id: requireId(record.exchange_id, "exchange_id"),
    generation: record.generation as number | undefined,
    data: record.data,
  };
}

export function encodeFrame(envelope: Envelope): string {
  const line = JSON.stringify(envelope);
  if (Buffer.byteLength(line, "utf8") > MAX_FRAME_BYTES) {
    throw new FrameError("frame_too_large", `outbound frame exceeds ${MAX_FRAME_BYTES} bytes`);
  }
  return line;
}

export function truncateUtf8(value: string, maxBytes: number): { text: string; truncated: boolean } {
  if (Buffer.byteLength(value, "utf8") <= maxBytes) return { text: value, truncated: false };
  let low = 0;
  let high = value.length;
  while (low < high) {
    const mid = Math.ceil((low + high) / 2);
    if (Buffer.byteLength(value.slice(0, mid), "utf8") <= maxBytes - 64) low = mid;
    else high = mid - 1;
  }
  return { text: `${value.slice(0, low)}\n[truncated by Warp standalone helper: ${maxBytes} byte limit]`, truncated: true };
}

// ---------------------------------------------------------------------------
// Rust -> helper payloads
// ---------------------------------------------------------------------------

export interface ProviderConfig {
  /** Provider id registered with the Pi model runtime (never a secret). */
  provider_id: string;
  name: string;
  base_url: string;
  /** Wire protocol; v1 supports only OpenAI Chat Completions. */
  api: "openai-completions";
  /** "none" must result in no Authorization header on the wire. */
  auth: { kind: "none" } | { kind: "api_key"; api_key: string };
  headers?: Record<string, string>;
  model_id: string;
  model_name: string;
  context_window: number;
  max_output_tokens: number;
  supports_image_input?: boolean;
  reasoning?: boolean;
  compat?: {
    supportsDeveloperRole?: boolean;
    supportsReasoningEffort?: boolean;
    supportsUsageInStreaming?: boolean;
    maxTokensField?: "max_tokens" | "max_completion_tokens";
    toolChoice?: boolean;
  };
}

export interface SubagentBudgetConfig {
  /** Default model round trips per child; clamped to 1..40. */
  max_turns?: number;
  /** Default per-child wall-clock budget; clamped to 1..1800 seconds. */
  deadline_seconds?: number;
  /** Per-child token cap (input + output, cache included); default 200_000. */
  token_cap?: number;
  /** Per-parent-turn aggregate child token cap; default 600_000. */
  aggregate_token_cap?: number;
  /** `task.progress` cadence; default 30_000 ms, clamped to 250..120_000. */
  heartbeat_ms?: number;
}

/**
 * Opt-in subagent prototype configuration. Absent or `enabled: false` keeps
 * every existing conversation byte-identical: no `task` tool, no child
 * sessions, no extra events.
 */
export interface SubagentConfig {
  enabled?: boolean;
  /** Children per parent turn; default 3, hard cap 8. */
  max_children?: number;
  /** Depth limit; v1 supports 1 only (children never get `task`). */
  max_depth?: number;
  budget?: SubagentBudgetConfig;
}

export interface SessionOpenData {
  working_dir: string;
  /** Fork-specific private data directory. Never the user's ~/.pi. */
  agent_dir: string;
  /** Directory for Pi session JSONL files. */
  session_dir: string;
  /** Existing Pi session file to restore, if any. */
  session_file?: string;
  system_prompt?: string;
  load_context_files?: boolean;
  max_context_file_bytes?: number;
  provider: ProviderConfig;
  compaction?: { enabled?: boolean; reserve_tokens?: number; keep_recent_tokens?: number };
  retry?: { enabled?: boolean; max_retries?: number; base_delay_ms?: number };
  subagents?: SubagentConfig;
}

export type ToolResultStatus = "success" | "rejected" | "error";

export interface ToolResultInput {
  tool_call_id: string;
  status: ToolResultStatus;
  content: string;
}

export interface TurnStartData {
  prompt: string;
  /** Provenance only; the helper does not merge history into an existing session. */
  is_first_turn?: boolean;
}

export interface TurnResumeData {
  results: ToolResultInput[];
}

export interface TurnCancelData {
  reason: string;
}

export interface CompactData {
  instructions?: string;
}

// ---------------------------------------------------------------------------
// helper -> Rust events
// ---------------------------------------------------------------------------

export interface ToolCallSpec {
  tool_call_id: string;
  /** Canonical workspace call, e.g. `workspace.read_file`. */
  name: string;
  arguments: unknown;
}

export interface AgentUsageCost {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  /** USD; the SDK reports zeros because standalone profiles register zero rates. */
  total: number;
}

export interface AgentUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens?: number;
  cache_write_tokens?: number;
  /** Subset of `output_tokens` when the provider reports a breakdown. */
  reasoning_tokens?: number;
  total_tokens?: number;
  cost?: AgentUsageCost;
}

/** Where a `context.updated` reading came from. */
export type ContextUsageSource = "usage" | "estimate" | "compaction_estimate";

export type RuntimeEvent =
  | {
      type: "hello.ok";
      helper_version: string;
      node_version: string;
      capabilities: {
        protocol: number;
        brokered_tools: string[];
        compaction: boolean;
        cancellation: boolean;
        context_files: boolean;
        /**
         * The helper can host read-only depth-1 child sessions when a session
         * opens with `subagents.enabled: true`. Capability only; sessions are
         * unchanged unless the session-open payload opts in.
         */
        subagents: boolean;
      };
    }
  | {
      type: "session.opened";
      resumed: boolean;
      session_file?: string;
      session_id: string;
      active_tools: string[];
      model_id: string;
      working_dir: string;
      context_window: number;
      max_output_tokens: number;
    }
  | { type: "turn.started" }
  | { type: "assistant.delta"; message_id: string; text: string }
  | { type: "assistant.message"; message_id: string; text: string }
  | { type: "assistant.reasoning"; message_id: string; text: string }
  | { type: "tool.calls"; calls: ToolCallSpec[] }
  | { type: "turn.awaiting_tools"; pending: string[] }
  | { type: "turn.completed"; stop_reason: string; usage: AgentUsage | null }
  | { type: "turn.cancelled"; reason: string }
  | { type: "turn.failed"; code: string; message: string; retryable: boolean }
  | { type: "error"; code: string; message: string; retryable?: boolean }
  | {
      /**
       * One per assistant message, including tool-call-only and errored
       * messages (zero usage). Timings are wall-clock approximations: the
       * pinned SDK exposes no generation timestamps, so `duration_ms` spans the
       * agent turn start (or the assistant `message_start`) to `message_end`,
       * and `first_token_ms` spans the same start to the first streamed
       * text/thinking/tool-call event. Both include request build and
       * time-to-first-byte.
       */
      type: "assistant.usage";
      message_id: string;
      model_id: string;
      api: string;
      usage: AgentUsage;
      duration_ms: number;
      first_token_ms?: number;
      /** `output_tokens / duration_ms * 1000`, omitted when not measurable. */
      output_tokens_per_second?: number;
      stop_reason: string;
    }
  | {
      /**
       * Context-window reading. `tokens: null` means unknown (e.g. right after
       * compaction); `context_window` is omitted only when the effective limit
       * is unknown, and `percent` is null whenever `tokens` is.
       */
      type: "context.updated";
      tokens: number | null;
      context_window?: number;
      /** `tokens / context_window * 100` (the SDK scale), or null when unknown. */
      percent: number | null;
      source: ContextUsageSource;
    }
  | {
      /**
       * A `task` tool call started a child session. Diagnostic-level; carries
       * the parent turn identity and the child attribution ids.
       */
      type: "task.started";
      task_id: string;
      child_session_id: string;
      description: string;
      subagent_type: "explore" | "verify";
      prompt_bytes: number;
      max_turns: number;
      deadline_ms: number;
      token_cap: number;
    }
  | {
      /**
       * Periodic liveness heartbeat while a child runs. The parent exchange
       * emits no provider events during a child, so this keeps the bridge's
       * stall watchdog from killing an otherwise healthy turn.
       */
      type: "task.progress";
      task_id: string;
      child_session_id: string;
      elapsed_ms: number;
      turns: number;
      tool_calls: number;
      tokens: { input_tokens: number; output_tokens: number; total_tokens: number };
      pending_tools: number;
    }
  | {
      /**
       * Terminal child outcome. `usage` is the child's own token usage, so
       * the ledger/UI can attribute child cost without a Warp proto change.
       */
      type: "task.completed";
      task_id: string;
      child_session_id: string;
      status: "ok" | "error" | "timeout" | "cancelled" | "budget_exceeded";
      reason?: string;
      subagent_type: "explore" | "verify";
      turns: number;
      tool_calls: number;
      usage: AgentUsage;
      wall_ms: number;
      summary_bytes: number;
    }
  | { type: "compaction.started"; reason: string }
  | {
      type: "compaction.finished";
      reason: string;
      summarized: boolean;
      tokens_before?: number;
      tokens_after?: number;
      /** Usage of the summarization LLM call, when the SDK reports it. */
      summary_usage?: AgentUsage;
      duration_ms?: number;
    }
  | { type: "diagnostic"; level: "info" | "warn" | "error"; message: string };
