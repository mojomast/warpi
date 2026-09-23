/**
 * Deterministic OpenAI-compatible Chat Completions fixture.
 *
 * Used by the helper's tests and (via the same wire contract) by the Rust
 * adapter's integration tests. It supports:
 * - streamed text deltas and fragmented tool-call arguments,
 * - arbitrary scripted responses per request index,
 * - capturing request bodies and headers for assertions.
 */

import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { once } from "node:events";
import type { AddressInfo } from "node:net";

export interface ChatRequestCapture {
  body: Record<string, unknown>;
  headers: Record<string, string | string[] | undefined>;
}

export interface ScriptedChunk {
  /** SSE-style incremental content delta. */
  text?: string;
  /** Incremental tool call arguments (may be split arbitrarily across chunks). */
  toolArguments?: string;
  toolName?: string;
  toolCallId?: string;
  /** Finish reason for the final chunk of this response. */
  finishReason?: string | null;
}

export type ScriptStep =
  /** `usage` is the raw OpenAI usage object for the final chunk; `false` omits the usage chunk. */
  | { kind: "text"; chunks: string[]; finishReason?: string; usage?: Record<string, unknown> | false }
  | { kind: "tool_call"; toolCallId: string; toolName: string; argumentChunks: string[] }
  | { kind: "error"; status: number; body: string }
  | { kind: "truncate"; chunks: string[] };

export class FakeProvider {
  readonly captures: ChatRequestCapture[] = [];
  private readonly steps: ScriptStep[];
  private server: Server | undefined;
  private port = 0;
  private stepIndex = 0;
  /** Invoked after every captured request (used by the cross-language fixture). */
  onCapture: (() => void) | undefined;

  constructor(steps: ScriptStep[]) {
    this.steps = steps;
  }

  get baseUrl(): string {
    if (this.server === undefined) throw new Error("fake provider not started");
    return `http://127.0.0.1:${this.port}/v1`;
  }

  get requestCount(): number {
    return this.captures.length;
  }

  async start(port = 0): Promise<void> {
    this.server = createServer((request, response) => {
      void this.handle(request, response);
    });
    this.server.listen(port, "127.0.0.1");
    await once(this.server, "listening");
    this.port = (this.server.address() as AddressInfo).port;
  }

  async stop(): Promise<void> {
    if (this.server === undefined) return;
    this.server.closeAllConnections();
    this.server.close();
    await once(this.server, "close");
    this.server = undefined;
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    // `/models` is optional for the adapter (manual model ids are supported),
    // so the fixture answers it like an OpenAI-compatible server would.
    if (request.method === "GET" && (request.url ?? "").split("?")[0].endsWith("/models")) {
      this.captures.push({ body: { path: request.url ?? "" }, headers: { ...request.headers } });
      this.onCapture?.();
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          object: "list",
          data: [{ id: "fixture-model", object: "model", owned_by: "fixture" }],
        }),
      );
      return;
    }
    const chunks: Buffer[] = [];
    for await (const chunk of request) chunks.push(chunk as Buffer);
    const raw = Buffer.concat(chunks).toString("utf8");
    let body: Record<string, unknown> = {};
    try {
      body = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      body = { _raw: raw };
    }
    this.captures.push({ body, headers: { ...request.headers } });
    this.onCapture?.();
    const step = this.steps[this.stepIndex];
    this.stepIndex += 1;
    if (step === undefined) {
      response.writeHead(500, { "content-type": "application/json" });
      response.end(JSON.stringify({ error: { message: "no scripted response" } }));
      return;
    }
    if (step.kind === "error") {
      response.writeHead(step.status, { "content-type": "application/json" });
      response.end(step.body);
      return;
    }
    response.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    const write = (payload: unknown) => response.write(`data: ${JSON.stringify(payload)}\n\n`);
    const model = (body.model as string | undefined) ?? "fixture-model";
    const created = Math.floor(Date.now() / 1000);
    const base = { id: "chatcmpl-fixture", object: "chat.completion.chunk", created, model };
    if (step.kind === "truncate") {
      // Emit a partial response and drop the connection without a finish chunk.
      for (const text of step.chunks) {
        write({ ...base, choices: [{ index: 0, delta: { content: text }, finish_reason: null }] });
      }
      await new Promise((resolve) => setTimeout(resolve, 10));
      response.destroy();
      return;
    }
    if (step.kind === "text") {
      for (const text of step.chunks) {
        write({ ...base, choices: [{ index: 0, delta: { content: text }, finish_reason: null }] });
      }
      write({ ...base, choices: [{ index: 0, delta: {}, finish_reason: step.finishReason ?? "stop" }] });
      if (step.usage !== false) {
        write({
          ...base,
          choices: [],
          usage: step.usage ?? { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
        });
      }
      response.write("data: [DONE]\n\n");
      response.end();
      return;
    }
    // tool_call
    write({
      ...base,
      choices: [
        {
          index: 0,
          delta: {
            tool_calls: [
              {
                index: 0,
                id: step.toolCallId,
                type: "function",
                function: { name: step.toolName, arguments: "" },
              },
            ],
          },
          finish_reason: null,
        },
      ],
    });
    for (const argumentChunk of step.argumentChunks) {
      write({
        ...base,
        choices: [
          {
            index: 0,
            delta: { tool_calls: [{ index: 0, function: { arguments: argumentChunk } }] },
            finish_reason: null,
          },
        ],
      });
    }
    write({ ...base, choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
    write({ ...base, choices: [], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } });
    response.write("data: [DONE]\n\n");
    response.end();
  }
}

/** Split a string into fixed-size fragments to exercise fragmented streams. */
export function fragment(value: string, size: number): string[] {
  const parts: string[] = [];
  for (let index = 0; index < value.length; index += size) {
    parts.push(value.slice(index, index + size));
  }
  return parts;
}
