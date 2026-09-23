#!/usr/bin/env node
/**
 * Entry point for the private Pi runtime helper.
 *
 * Protocol: newline-delimited JSON frames on stdin/stdout (see protocol.ts).
 * Only frames are written to stdout; all diagnostics go to stderr.
 *
 * The helper never executes workspace side effects: every model tool call is
 * brokered back to the Warp backend and only resumes when Warp delivers a
 * result. No HTTP server, no global Pi install, no ambient configuration.
 */

import { createInterface } from "node:readline";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import {
  encodeFrame,
  FrameError,
  MAX_FRAME_BYTES,
  PROTOCOL_VERSION,
  parseFrame,
  type Envelope,
  type RuntimeEvent,
} from "./protocol.js";
import { HELPER_VERSION, PiAgentRuntime, type TurnIdentity } from "./runtime.js";
import { WORKSPACE_TOOL_NAMES } from "./workspace-tools.js";

const REQUIRED_NODE_MAJOR = 22;
const REQUIRED_NODE_MINOR = 19;

function enforceNodeVersion(): void {
  const [major = 0, minor = 0] = process.versions.node.split(".").map((part) => Number.parseInt(part, 10));
  if (major < REQUIRED_NODE_MAJOR || (major === REQUIRED_NODE_MAJOR && minor < REQUIRED_NODE_MINOR)) {
    process.stderr.write(
      `warpi-pi-runtime requires Node >= ${REQUIRED_NODE_MAJOR}.${REQUIRED_NODE_MINOR}.0, found ${process.versions.node}\n`,
    );
    process.exit(2);
  }
}

/**
 * The launcher sanitizes the environment; this is defense in depth so a
 * standalone build can never inherit a user's Pi configuration or provider
 * credentials from the ambient environment.
 */
function sanitizeEnvironment(): void {
  for (const key of Object.keys(process.env)) {
    if (
      key.startsWith("PI_") ||
      key === "OPENAI_API_KEY" ||
      key === "ANTHROPIC_API_KEY" ||
      key === "GEMINI_API_KEY" ||
      key === "GOOGLE_API_KEY" ||
      key === "XAI_API_KEY" ||
      key === "OPENROUTER_API_KEY" ||
      key.startsWith("AWS_")
    ) {
      delete process.env[key];
    }
  }
  // Block catalog/package network access inside the SDK.
  process.env.PI_OFFLINE = "1";
  process.env.PI_TELEMETRY = "0";
}

class FrameWriter {
  private readonly queue: string[] = [];
  private draining = false;
  private broken = false;

  constructor(private readonly out: NodeJS.WriteStream, private readonly onBrokenPipe: () => void) {}

  write(line: string): void {
    if (this.broken) return;
    // stdout is a pipe: `PIPE_BUF`-sized writes are atomic, and the Rust reader
    // uses line framing, so queue to keep frames from interleaving.
    this.queue.push(line);
    this.pump();
  }

  private pump(): void {
    if (this.draining) return;
    while (this.queue.length > 0) {
      const line = this.queue.shift();
      if (line === undefined) return;
      this.draining = true;
      const flushed = this.out.write(`${line}\n`, () => {
        this.draining = false;
        this.pump();
      });
      if (!flushed) {
        // Wait for drain; the write callback above resumes pumping.
        this.out.once("drain", () => {
          this.draining = false;
          this.pump();
        });
        return;
      }
      this.draining = false;
    }
  }

  markBroken(): void {
    this.broken = true;
    this.queue.length = 0;
    this.onBrokenPipe();
  }
}

/**
 * Origins whose provider profile is configured with auth=none.
 *
 * Pi's SDK requires a configured credential before it will run a turn, so
 * auth=none profiles register a non-secret placeholder key. This set is the
 * transport-level guarantee that the placeholder (and any other
 * Authorization header) is stripped before the request leaves the process.
 * The integration tests assert both halves of that contract.
 */
const NO_AUTH_ORIGINS = new Set<string>();

export function registerNoAuthOrigin(baseUrl: string): void {
  try {
    NO_AUTH_ORIGINS.add(new URL(baseUrl).origin);
  } catch (error) {
    process.stderr.write(`[pi-helper] ignoring unparsable provider base URL "${baseUrl}": ${String(error)}\n`);
  }
}

function installNoAuthTransportPolicy(): void {
  const baseFetch = globalThis.fetch;
  globalThis.fetch = async (input: Request | URL | string, init?: RequestInit): Promise<Response> => {
    const url =
      typeof input === "string" ? input : input instanceof URL ? input.href : (input as Request).url;
    let origin: string | undefined;
    try {
      origin = new URL(url).origin;
    } catch {
      origin = undefined;
    }
    if (origin === undefined || !NO_AUTH_ORIGINS.has(origin)) {
      return baseFetch(input, init);
    }
    if (input instanceof Request) {
      const headers = new Headers(input.headers);
      headers.delete("authorization");
      headers.delete("cf-aig-authorization");
      return baseFetch(new Request(input, { headers }), init);
    }
    const headers = new Headers(init?.headers ?? undefined);
    headers.delete("authorization");
    headers.delete("cf-aig-authorization");
    return baseFetch(input, { ...(init ?? {}), headers });
  };
}

function main(): void {
  enforceNodeVersion();
  sanitizeEnvironment();

  installNoAuthTransportPolicy();

  // Fork-private scratch directory for anything the SDK needs at startup.
  const scratchDir = process.env.WARPI_PI_SCRATCH_DIR;
  if (typeof scratchDir === "string" && scratchDir.length > 0) {
    mkdirSync(scratchDir, { recursive: true, mode: 0o700 });
    process.env.TMPDIR = scratchDir;
  }

  const out = process.stdout;
  const writer = new FrameWriter(out, () => {
    process.exitCode = 0;
  });

  let seq = 0;
  let lastInboundSeq = -1;

  const emit = (kind: string, data: unknown, identity: Partial<TurnIdentity> | undefined): void => {
    const envelope: Envelope = {
      protocol: PROTOCOL_VERSION,
      seq: seq++,
      kind,
      data,
      ...(identity?.session_id !== undefined ? { session_id: identity.session_id } : {}),
      ...(identity?.turn_id !== undefined ? { turn_id: identity.turn_id } : {}),
      ...(identity?.exchange_id !== undefined ? { exchange_id: identity.exchange_id } : {}),
      ...(identity?.generation !== undefined ? { generation: identity.generation } : {}),
    };
    try {
      writer.write(encodeFrame(envelope));
    } catch (error) {
      process.stderr.write(`[pi-helper] failed to encode outbound frame: ${String(error)}\n`);
    }
  };

  const emitter = {
    emit: (event: RuntimeEvent, identity: Partial<TurnIdentity>) => emit(event.type, stripType(event), identity),
    diagnostic: (level: "info" | "warn" | "error", message: string) => {
      process.stderr.write(`[pi-helper] ${level}: ${message}\n`);
    },
  };

  const runtime = new PiAgentRuntime(emitter);

  const emitErrorFrame = (identity: Partial<TurnIdentity>, code: string, message: string, retryable = false): void => {
    // Protocol violations (stale/cross-session/unknown identities) are not
    // terminal for the running Pi turn: report them as `error` frames so the
    // backend can reconcile, and leave the suspended turn untouched.
    const terminal = code === "internal_error" || code === "provider_error";
    if (terminal && identity.turn_id !== undefined) {
      emit("turn.failed", { code, message, retryable }, identity);
    } else {
      emit("error", { code, message, retryable }, identity);
    }
  };

  const dispatch = async (frame: Envelope): Promise<void> => {
    const identity = {
      session_id: frame.session_id,
      generation: frame.generation,
      turn_id: frame.turn_id,
      exchange_id: frame.exchange_id,
    } as Partial<TurnIdentity>;
    switch (frame.kind) {
      case "hello": {
        emit(
          "hello.ok",
          {
            helper_version: HELPER_VERSION,
            node_version: process.versions.node,
            capabilities: {
              protocol: PROTOCOL_VERSION,
              brokered_tools: [...WORKSPACE_TOOL_NAMES],
              compaction: true,
              cancellation: true,
              context_files: true,
              // Capability only: the `task` tool appears solely when a session
              // opens with `subagents.enabled: true`.
              subagents: true,
            },
          },
          undefined,
        );
        return;
      }
      case "session.open": {
        requireIdentity(frame, ["session_id", "exchange_id", "generation"]);
        const openData = frame.data as { provider?: { base_url?: string; auth?: { kind?: string } } } | undefined;
        if (openData?.provider?.auth?.kind === "none" && typeof openData.provider.base_url === "string") {
          registerNoAuthOrigin(openData.provider.base_url);
        }
        const event = await runtime.handleSessionOpen(
          { session_id: frame.session_id!, generation: frame.generation!, exchange_id: frame.exchange_id! },
          frame.data as never,
        );
        emit(event.type, stripType(event), identity as TurnIdentity);
        return;
      }
      case "turn.start": {
        requireIdentity(frame, ["session_id", "exchange_id", "turn_id", "generation"]);
        const events = await runtime.handleTurnStart(identity as TurnIdentity, frame.data as never);
        for (const event of events) emit(event.type, stripType(event), identity as TurnIdentity);
        return;
      }
      case "turn.resume": {
        requireIdentity(frame, ["session_id", "exchange_id", "turn_id", "generation"]);
        const events = await runtime.handleTurnResume(identity as TurnIdentity, frame.data as never);
        for (const event of events) emit(event.type, stripType(event), identity as TurnIdentity);
        return;
      }
      case "turn.cancel": {
        requireIdentity(frame, ["session_id", "exchange_id", "turn_id", "generation"]);
        const reason = (frame.data as { reason?: string } | undefined)?.reason ?? "cancelled";
        emit("turn.cancelling", { reason }, identity as TurnIdentity);
        const events = await runtime.handleTurnCancel(identity as TurnIdentity, reason);
        for (const event of events) emit(event.type, stripType(event), identity as TurnIdentity);
        return;
      }
      case "session.compact": {
        requireIdentity(frame, ["session_id", "exchange_id", "generation"]);
        const events = await runtime.handleCompact(
          { session_id: frame.session_id!, generation: frame.generation!, exchange_id: frame.exchange_id! },
          (frame.data ?? {}) as never,
        );
        for (const event of events) emit(event.type, stripType(event), identity as TurnIdentity);
        return;
      }
      case "shutdown": {
        await runtime.handleShutdown();
        emit("shutdown.ack", {}, undefined);
        setTimeout(() => process.exit(0), 10).unref();
        return;
      }
      default:
        throw new FrameError("unknown_frame", `unsupported frame kind: ${frame.kind}`);
    }
  };

  const sessionOf = (frame: Envelope): string =>
    frame.session_id ?? frame.turn_id ?? frame.exchange_id ?? `anonymous:${seq}`;

  const reader = createInterface({ input: process.stdin, crlfDelay: Infinity, terminal: false });

  reader.on("line", (line: string) => {
    if (line.length === 0) return;
    let frame: Envelope;
    try {
      frame = parseFrame(line);
    } catch (error) {
      const code = error instanceof FrameError ? error.code : "invalid_frame";
      process.stderr.write(`[pi-helper] rejected frame: ${String(error)}\n`);
      emit("error", { code, message: String(error) }, undefined);
      return;
    }
    if (frame.seq <= lastInboundSeq) {
      emit("error", { code: "out_of_order", message: `duplicate or out-of-order frame seq ${frame.seq}` }, undefined);
      return;
    }
    lastInboundSeq = frame.seq;
    if (Buffer.byteLength(line, "utf8") > MAX_FRAME_BYTES) {
      emit("error", { code: "frame_too_large", message: "frame exceeded the maximum size" }, undefined);
      return;
    }
    // Frames are handled per session but never await the whole prompt: a
    // resume/cancel arriving while a prompt runs must be dispatched promptly.
    const sessionId = sessionOf(frame);
    void runtime
      .enqueue(sessionId, async () => {
        try {
          await dispatch(frame);
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          const code = classifyError(message);
          emitErrorFrame(
            {
              session_id: frame.session_id,
              turn_id: frame.turn_id,
              exchange_id: frame.exchange_id,
              generation: frame.generation,
            },
            code,
            message,
          );
        }
      })
      .catch((error) => {
        process.stderr.write(`[pi-helper] internal error while handling frame: ${String(error)}\n`);
      });
  });

  reader.on("close", () => {
    void runtime.handleShutdown().finally(() => process.exit(0));
  });

  for (const signal of ["SIGTERM", "SIGINT"] as const) {
    process.on(signal, () => {
      process.stderr.write(`[pi-helper] received ${signal}, shutting down\n`);
      void runtime.handleShutdown().finally(() => process.exit(0));
    });
  }

  process.on("uncaughtException", (error) => {
    process.stderr.write(`[pi-helper] uncaught exception: ${String(error)}\n`);
    emit("error", { code: "internal_error", message: String(error) }, undefined);
  });

  out.on("error", () => writer.markBroken());
  void join;
}

function requireIdentity(frame: Envelope, fields: Array<keyof Envelope>): void {  for (const field of fields) {
    const value = frame[field];
    if (value === undefined || value === null || (typeof value === "string" && value.length === 0)) {
      throw new FrameError("invalid_frame", `frame ${frame.kind} requires ${String(field)}`);
    }
  }
}

function stripType(event: RuntimeEvent): Record<string, unknown> {
  const { type: _type, ...rest } = event as unknown as { type: string } & Record<string, unknown>;
  return rest;
}

function classifyError(message: string): string {
  for (const code of ["session_not_open", "stale_turn", "unknown_tool_result"]) {
    if (message.includes(code)) return code;
  }
  return "internal_error";
}

main();
