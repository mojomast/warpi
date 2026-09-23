/**
 * Minimal recording reverse proxy for live-provider validation: forwards
 * requests to an OpenAI-compatible upstream and keeps the `usage` objects from
 * its SSE chunks so a test can compare helper events against what the provider
 * actually reported. Requests and responses are never logged or persisted.
 */

import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { once } from "node:events";
import type { AddressInfo } from "node:net";

export class RecordingProxy {
  private server: Server | undefined;
  private port = 0;
  private upstream: URL | undefined;
  private requests = 0;
  /** Usage objects from every SSE chunk that carried one, in arrival order. */
  readonly usages: Array<Record<string, unknown>> = [];

  get baseUrl(): string {
    if (this.server === undefined) throw new Error("proxy not started");
    return `http://127.0.0.1:${this.port}/v1`;
  }

  get requestCount(): number {
    return this.requests;
  }

  async start(upstreamBaseUrl: string): Promise<void> {
    this.upstream = new URL(upstreamBaseUrl);
    this.server = createServer((request, response) => {
      void this.handle(request, response);
    });
    this.server.listen(0, "127.0.0.1");
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
    if (this.upstream === undefined) throw new Error("proxy not started");
    this.requests += 1;
    const chunks: Buffer[] = [];
    for await (const chunk of request) chunks.push(chunk as Buffer);
    const target = new URL(request.url ?? "/", this.upstream);
    const headers = new Headers();
    for (const [name, value] of Object.entries(request.headers)) {
      if (name === "host" || name === "content-length" || name === "accept-encoding") continue;
      if (Array.isArray(value)) for (const item of value) headers.append(name, item);
      else if (value !== undefined) headers.set(name, value);
    }
    const upstreamResponse = await fetch(target, {
      method: request.method ?? "POST",
      headers,
      body: chunks.length > 0 ? Buffer.concat(chunks) : undefined,
    });
    const responseHeaders: Record<string, string> = {};
    for (const [name, value] of upstreamResponse.headers) {
      if (name === "content-length" || name === "content-encoding" || name === "transfer-encoding") continue;
      responseHeaders[name] = value;
    }
    response.writeHead(upstreamResponse.status, responseHeaders);
    if (upstreamResponse.body === null) {
      response.end();
      return;
    }
    const reader = upstreamResponse.body.getReader();
    const decoder = new TextDecoder();
    let text = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      response.write(value);
      text += decoder.decode(value, { stream: true });
    }
    response.end();
    this.capture(text);
  }

  private capture(text: string): void {
    for (const line of text.split("\n")) {
      if (!line.startsWith("data:")) continue;
      const payload = line.slice("data:".length).trim();
      if (payload.length === 0 || payload === "[DONE]") continue;
      try {
        const parsed = JSON.parse(payload) as { usage?: unknown };
        if (typeof parsed.usage === "object" && parsed.usage !== null) {
          this.usages.push(parsed.usage as Record<string, unknown>);
        }
      } catch {
        // A partial SSE line; the usage chunk always arrives whole.
      }
    }
  }
}
