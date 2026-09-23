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

export const WORKSPACE_TOOL_NAMES = ["bash", "read", "write", "edit", "glob", "grep"] as const;
export type WorkspaceToolName = (typeof WORKSPACE_TOOL_NAMES)[number];

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
  toolCallId: string;
  call: ToolCallSpec;
  resolve: (result: AgentToolResult<unknown>) => void;
  reject: (error: Error) => void;
  cleanup: () => void;
  settled: boolean;
}

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
  }
}

export class WorkspaceToolBroker {
  private readonly pending = new Map<string, PendingCall>();
  private readonly flushScheduled = new Set<string>();

  constructor(private readonly host: BrokerHost) {}

  get pendingCount(): number {
    return this.pending.size;
  }

  pendingIdsFor(ownerKey: string): string[] {
    return [...this.pending.values()].filter((call) => call.ownerKey === ownerKey).map((call) => call.toolCallId);
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
          reject(new Error("Tool call aborted"));
        }
      };
      const entry: PendingCall = {
        ownerKey,
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
    const { text, truncated } = truncateUtf8(content, 4 * 1024 * 1024);
    const suffix = truncated ? "" : "";
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
    call.reject(new Error(reason));
    return true;
  }
}

const shellSchema = Type.Object({
  command: Type.String({ description: "The shell command to execute." }),
  workdir: Type.Optional(Type.String({ description: "Working directory; defaults to the workspace root." })),
  timeout_ms: Type.Optional(Type.Integer({ minimum: 1, description: "Timeout in milliseconds." })),
  run_in_background: Type.Optional(
    Type.Boolean({ description: "Run without waiting for completion; returns a command id in the result." }),
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
 * Build the six brokered tools. `ownerKey` identifies the Pi session that owns
 * the calls, so results can never be delivered across sessions.
 */
export function createWorkspaceTools(ownerKey: string, broker: WorkspaceToolBroker) {
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
      broker.execute(ownerKey, toolCallId, name, args as Record<string, unknown>, signal),
  });

  return [
    tool("bash", "Execute a shell command in the user's workspace. Approval and execution are handled by Warp.", shellSchema),
    tool("read", "Read a text file from the user's workspace.", readSchema),
    tool("write", "Create or overwrite a file in the user's workspace.", writeSchema),
    tool("edit", "Apply an exact-text edit to an existing file after review.", editSchema),
    tool("glob", "List files in the workspace matching a glob pattern.", globSchema),
    tool("grep", "Search file contents in the workspace with a regular expression.", grepSchema),
  ];
}
