/**
 * Brokered workspace tools.
 *
 * Every workspace capability the model can invoke is registered here as a Pi
 * custom tool. When the model calls one, the tool does NOT execute anything:
 * it suspends on a promise and the runtime emits a `tool.calls` batch followed
 * by `turn.awaiting_tools` to the Rust backend. Warp executes the action (with
 * its own approval flow) and later sends `turn.resume` with the result, which
 * resolves the same suspended promise so the same Pi prompt continues.
 *
 * The model-facing tool names are intentionally small and stable. Each maps to
 * a canonical workspace call (`workspace.*`) that the Rust backend translates
 * into a typed Warp action. Only canonical calls cross the protocol.
 */

import { Type, type Static, type TSchema } from "typebox";
import type { AgentToolResult } from "@earendil-works/pi-coding-agent";
import { MAX_TOOL_ARGUMENT_BYTES, MAX_ID_LENGTH, truncateUtf8 } from "./protocol.js";

export const WORKSPACE_TOOL_NAMES = [
  "bash",
  "read",
  "write",
  "edit",
  "glob",
  "grep",
  "bash_output",
] as const;
export type WorkspaceToolName = (typeof WORKSPACE_TOOL_NAMES)[number];

/**
 * Standing instructions appended to every standalone session's system prompt.
 *
 * The harness cannot enforce any of this: a command that reads stdin or never
 * exits blocks the user's terminal until they stop it, so the model must avoid
 * those commands in the first place. The same rules are present in the `bash`
 * tool description, which is what Pi actually shows the model.
 */
export const STANDALONE_SYSTEM_GUIDANCE = [
  "Shell commands run headless with no stdin.",
  "Every command must be non-interactive and must terminate on its own.",
  "Never start ssh without -o BatchMode=yes, vim, nano, top, less, more, REPLs, watch, `tail -f`, or anything else that waits for input or runs forever.",
  "Bound commands yourself: pass -y/--batch/--yes/--no-pager, set connect timeouts (ssh -o BatchMode=yes -o ConnectTimeout=10), and wrap potentially long commands in an explicit timeout (timeout 60s <command>).",
  "If a command is still running, use the bash_output tool with its command id to wait in bounded steps, or tell the user it is still running; do not start another command while it occupies the terminal.",
].join(" ");

export interface ToolCallSpec {
  tool_call_id: string;
  name: string;
  arguments: unknown;
}

export interface BrokerHost {
  /** Emit the batch and then `turn.awaiting_tools`. Synchronous and non-blocking. */
  emitToolBatch(ownerKey: string, calls: ToolCallSpec[]): void;
  /** A tool call was aborted before its batch was emitted. */
  emitDiagnostic?(level: "info" | "warn" | "error", message: string): void;
}

interface PendingCall {
  ownerKey: string;
  /** Child session id when the call came from a subagent; undefined for the parent. */
  agentId: string | undefined;
  toolCallId: string;
  call: ToolCallSpec;
  resolve: (result: AgentToolResult<unknown>) => void;
  reject: (error: Error) => void;
  cleanup: () => void;
  settled: boolean;
}

/** Bounded memory of settled call ids so late results can be dropped safely. */
const SETTLED_ID_LIMIT = 512;

const textResult = (text: string): AgentToolResult<unknown> => ({
  content: [{ type: "text", text }],
  details: {},
});

/** Canonical workspace call produced for each model-facing tool call. */
export function canonicalCall(name: WorkspaceToolName, args: Record<string, unknown>): ToolCallSpec["name"] {
  switch (name) {
    case "bash":
      return "workspace.shell";
    case "read":
      return "workspace.read_file";
    case "write":
      return "workspace.write_file";
    case "edit":
      return "workspace.edit_file";
    case "glob":
      return "workspace.glob";
    case "grep":
      return "workspace.grep";
    case "bash_output":
      return "workspace.read_shell_command_output";
  }
}

export class WorkspaceToolBroker {
  private readonly pending = new Map<string, PendingCall>();
  private readonly flushScheduled = new Set<string>();
  private readonly settledIds = new Set<string>();

  constructor(private readonly host: BrokerHost) {}

  get pendingCount(): number {
    return this.pending.size;
  }

  pendingIdsFor(ownerKey: string): string[] {
    return [...this.pending.values()].filter((call) => call.ownerKey === ownerKey).map((call) => call.toolCallId);
  }

  pendingIdsForAgent(agentId: string): string[] {
    return [...this.pending.values()]
      .filter((call) => call.agentId === agentId)
      .map((call) => call.toolCallId);
  }

  /**
   * True when the call id was emitted and already settled (delivered or
   * cancelled). Used to drop late results for abandoned child calls instead of
   * failing the parent turn.
   */
  wasSettled(toolCallId: string): boolean {
    return this.settledIds.has(toolCallId);
  }

  hasPendingFor(ownerKey: string): boolean {
    for (const call of this.pending.values()) {
      if (call.ownerKey === ownerKey) return true;
    }
    return false;
  }

  /**
   * Suspend a tool call until Warp delivers a result. Resolves with the
   * approved execution result; rejects if the call is cancelled or aborted.
   */
  execute(
    ownerKey: string,
    toolCallId: string,
    name: WorkspaceToolName,
    args: Record<string, unknown>,
    signal: AbortSignal | undefined,
    agentId?: string,
  ): Promise<AgentToolResult<unknown>> {
    if (typeof toolCallId !== "string" || toolCallId.length === 0 || toolCallId.length > MAX_ID_LENGTH) {
      throw new Error(`invalid tool call id from model: ${String(toolCallId)}`);
    }
    if (this.pending.has(toolCallId)) {
      throw new Error(`duplicate tool call id from model: ${toolCallId}`);
    }
    const serialized = JSON.stringify(args ?? {});
    if (Buffer.byteLength(serialized, "utf8") > MAX_TOOL_ARGUMENT_BYTES) {
      return Promise.resolve(
        textResult(`Tool arguments exceeded the ${MAX_TOOL_ARGUMENT_BYTES} byte limit and were not executed.`),
      );
    }
    const call: ToolCallSpec = { tool_call_id: toolCallId, name: canonicalCall(name, args), arguments: args ?? {} };
    return new Promise<AgentToolResult<unknown>>((resolve, reject) => {
      const onAbort = () => {
        if (this.pending.delete(toolCallId)) {
          this.rememberSettled(toolCallId);
          reject(new Error("Tool call aborted"));
        }
      };
      const entry: PendingCall = {
        ownerKey,
        agentId,
        toolCallId,
        call,
        resolve: (result) => {
          entry.settled = true;
          entry.cleanup();
          resolve(result);
        },
        reject: (error) => {
          entry.settled = true;
          entry.cleanup();
          reject(error);
        },
        cleanup: () => {
          signal?.removeEventListener("abort", onAbort);
        },
        settled: false,
      };
      this.pending.set(toolCallId, entry);
      if (signal !== undefined) {
        if (signal.aborted) {
          onAbort();
          return;
        }
        signal.addEventListener("abort", onAbort, { once: true });
      }
      this.scheduleFlush(ownerKey);
    });
  }

  private scheduleFlush(ownerKey: string): void {
    if (this.flushScheduled.has(ownerKey)) return;
    this.flushScheduled.add(ownerKey);
    queueMicrotask(() => {
      this.flushScheduled.delete(ownerKey);
      const calls = [...this.pending.values()].filter((call) => call.ownerKey === ownerKey).map((call) => call.call);
      if (calls.length === 0) return;
      this.host.emitToolBatch(ownerKey, calls);
    });
  }

  /**
   * Deliver a result to a suspended call. Returns false for unknown ids so the
   * caller can reject foreign/stale/duplicate results instead of guessing.
   */
  deliver(toolCallId: string, content: string, isError: boolean): boolean {
    const call = this.pending.get(toolCallId);
    if (call === undefined || call.settled) return false;
    this.pending.delete(toolCallId);
    this.rememberSettled(toolCallId);
    const { text, truncated } = truncateUtf8(content, 4 * 1024 * 1024);
    const suffix = truncated ? "\n[truncated by helper: result exceeded the 4 MiB limit]" : "";
    if (isError) call.reject(new Error(`Tool call failed: ${text}${suffix}`));
    else call.resolve(textResult(text));
    return true;
  }

  /** Cancel every suspended call owned by `ownerKey`. */
  cancelOwner(ownerKey: string, reason: string): string[] {
    const cancelled: string[] = [];
    for (const call of [...this.pending.values()]) {
      if (call.ownerKey !== ownerKey) continue;
      this.pending.delete(call.toolCallId);
      this.rememberSettled(call.toolCallId);
      call.reject(new Error(reason));
      cancelled.push(call.toolCallId);
    }
    return cancelled;
  }

  /** Cancel every suspended call attributed to one child session. */
  cancelAgent(agentId: string, reason: string): string[] {
    const cancelled: string[] = [];
    for (const call of [...this.pending.values()]) {
      if (call.agentId !== agentId) continue;
      this.pending.delete(call.toolCallId);
      this.rememberSettled(call.toolCallId);
      call.reject(new Error(reason));
      cancelled.push(call.toolCallId);
    }
    return cancelled;
  }

  /** Cancel a single call (used when Warp reports a rejection for one id). */
  cancelCall(toolCallId: string, reason: string): boolean {
    const call = this.pending.get(toolCallId);
    if (call === undefined) return false;
    this.pending.delete(toolCallId);
    this.rememberSettled(toolCallId);
    call.reject(new Error(reason));
    return true;
  }

  private rememberSettled(toolCallId: string): void {
    this.settledIds.add(toolCallId);
    if (this.settledIds.size > SETTLED_ID_LIMIT) {
      const oldest = this.settledIds.values().next().value;
      if (oldest !== undefined) this.settledIds.delete(oldest);
    }
  }
}

const shellSchema = Type.Object({
  command: Type.String({ description: "The shell command to execute." }),
  workdir: Type.Optional(Type.String({ description: "Working directory; defaults to the workspace root." })),
  run_in_background: Type.Optional(
    Type.Boolean({
      description:
        "Run without waiting for completion; the result is a snapshot with a command id. Commands still run in the user's terminal.",
    }),
  ),
});

const bashOutputSchema = Type.Object({
  command_id: Type.String({
    description: "The command id from a bash result that reported the command was still running.",
  }),
  wait_seconds: Type.Optional(
    Type.Integer({
      minimum: 1,
      maximum: 120,
      description: "How long to wait for output or completion before returning a snapshot (default 30, maximum 120).",
    }),
  ),
});

const readSchema = Type.Object({
  path: Type.String({ description: "Path to the file to read, absolute or relative to the workspace root." }),
  offset: Type.Optional(Type.Integer({ minimum: 0, description: "First line to read (1-based)." })),
  limit: Type.Optional(Type.Integer({ minimum: 1, description: "Maximum number of lines to read." })),
});

const writeSchema = Type.Object({
  path: Type.String({ description: "Path of the file to create or overwrite." }),
  content: Type.String({ description: "Full file contents." }),
});

const editSchema = Type.Object({
  path: Type.String({ description: "Path of the file to edit." }),
  old_string: Type.String({ description: "Exact existing text to replace. Must be unique unless replace_all is set." }),
  new_string: Type.String({ description: "Replacement text." }),
  replace_all: Type.Optional(Type.Boolean({ description: "Replace every occurrence instead of requiring uniqueness." })),
});

const globSchema = Type.Object({
  pattern: Type.String({ description: "File name glob pattern, e.g. `**/*.rs`." }),
  path: Type.Optional(Type.String({ description: "Directory to search; defaults to the workspace root." })),
});

const grepSchema = Type.Object({
  pattern: Type.String({ description: "Regular expression to search for." }),
  path: Type.Optional(Type.String({ description: "Directory or file to search; defaults to the workspace root." })),
  glob: Type.Optional(Type.String({ description: "Restrict the search to files matching this glob." })),
  ignore_case: Type.Optional(Type.Boolean({ description: "Case-insensitive search." })),
});

export interface WorkspaceToolContext {
  ownerKey: string;
}

/**
 * Build the brokered workspace tools. `ownerKey` identifies the Pi session that
 * owns the calls, so results can never be delivered across sessions. Child
 * sessions share the parent's `ownerKey` (their calls must flow through the
 * parent exchange) but tag every call with their own `agentId` so a child can
 * be cancelled or metered without touching the parent's calls.
 */
export function createWorkspaceTools(
  ownerKey: string,
  broker: WorkspaceToolBroker,
  names: readonly WorkspaceToolName[] = WORKSPACE_TOOL_NAMES,
  agentId?: string,
) {
  const tool = <T extends TSchema>(name: WorkspaceToolName, description: string, parameters: T) => ({
    name,
    label: name,
    description,
    parameters,
    executionMode: "sequential" as const,
    execute: async (
      toolCallId: string,
      args: Static<T>,
      signal: AbortSignal | undefined,
    ): Promise<AgentToolResult<unknown>> =>
      broker.execute(ownerKey, toolCallId, name, args as Record<string, unknown>, signal, agentId),
  });

  const bashDescription = [
    "Execute a shell command in the user's workspace. Approval and execution are handled by Warp.",
    "Commands run headless with no stdin: never start interactive programs (ssh without BatchMode, vim, nano, top, less, more, REPLs, watch, `tail -f`) and never run something that waits forever.",
    "Put an explicit timeout inside the command itself (for example `timeout 60s <command>`); the harness does not enforce a timeout argument. Prefer non-interactive flags (-y/--batch/--yes/--no-pager, `ssh -o BatchMode=yes -o ConnectTimeout=10 host 'cmd'`).",
    "If the result says the command is still running, it did not finish: use bash_output with the returned command id to wait in bounded steps, or keep working on something else. Do not start another command while it occupies the terminal.",
  ].join(" ");

  const bashOutputDescription = [
    "Wait for a command that a previous bash call reported as still running.",
    "Returns the finished output with its exit code, or a fresh snapshot after wait_seconds (default 30, maximum 120).",
    "It never writes to the command and cannot stop it, so it cannot answer a prompt; use it only with a command id from a still-running result.",
  ].join(" ");

  const definitions: Array<[WorkspaceToolName, string, TSchema]> = [
    ["bash", bashDescription, shellSchema],
    ["bash_output", bashOutputDescription, bashOutputSchema],
    ["read", "Read a text file from the user's workspace.", readSchema],
    ["write", "Create or overwrite a file in the user's workspace.", writeSchema],
    ["edit", "Apply an exact-text edit to an existing file after review.", editSchema],
    ["glob", "List files in the workspace matching a glob pattern.", globSchema],
    ["grep", "Search file contents in the workspace with a regular expression.", grepSchema],
  ];
  const requested = new Set<WorkspaceToolName>(names);
  for (const name of requested) {
    if (!definitions.some(([defined]) => defined === name)) {
      throw new Error(`unknown workspace tool requested: ${name}`);
    }
  }
  return definitions
    .filter(([name]) => requested.has(name))
    .map(([name, description, parameters]) => tool(name, description, parameters));
}
