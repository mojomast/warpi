/**
 * Live-provider validation for the token-observability events.
 *
 * Runs only when credentials are provided through the environment, mirroring
 * `crates/standalone_agent/tests/real_provider.rs`, so CI without a key stays
 * green:
 *
 * ```bash
 * WARPI_REAL_PROVIDER_KEY="$(cat /path/to/key)" \
 *   node --import tsx --test test/usage-live.test.ts
 * ```
 *
 * A short text turn is run for every model in `WARPI_REAL_PROVIDER_MODELS`
 * (default `deepseek-flash,deepseek-v4-pro`), plus one tool-call round for the
 * first model. Each `assistant.usage` event is checked against the usage object
 * the provider reported on the wire (captured by `RecordingProxy`) and against
 * the usage persisted in the Pi session JSONL. Prompts, responses and
 * credentials are never printed.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { HelperClient, type HelperFrame } from "./helper-client.ts";
import { RecordingProxy } from "./recording-proxy.ts";

const KEY = process.env.WARPI_REAL_PROVIDER_KEY?.trim();
const BASE_URL = (process.env.WARPI_REAL_PROVIDER_BASE_URL ?? "https://api.deepseek.com/v1").replace(/\/+$/, "");
const MODELS = (process.env.WARPI_REAL_PROVIDER_MODELS ?? process.env.WARPI_REAL_PROVIDER_MODEL ?? "deepseek-flash,deepseek-v4-pro")
  .split(",")
  .map((model) => model.trim())
  .filter((model) => model.length > 0);
if (MODELS.length === 0) MODELS.push("deepseek-flash");
const SKIP_REASON = "set WARPI_REAL_PROVIDER_KEY to run the live usage validation";
const TIMEOUT_MS = 120_000;

function identity(turnId: string): Partial<HelperFrame> {
  return { session_id: "live-conv", turn_id: turnId, exchange_id: `ex-${turnId}`, generation: 0 };
}

function liveSessionConfig(baseUrl: string, dataDir: string, model: string, apiKey: string) {
  return {
    working_dir: dataDir,
    agent_dir: join(dataDir, "agent"),
    session_dir: join(dataDir, "sessions"),
    load_context_files: false,
    system_prompt: "You are a terse assistant. Follow instructions exactly.",
    provider: {
      provider_id: "warpi-live",
      name: "deepseek-live",
      base_url: baseUrl,
      api: "openai-completions",
      auth: { kind: "api_key", api_key: apiKey },
      model_id: model,
      model_name: model,
      context_window: 65536,
      max_output_tokens: 1024,
      reasoning: false,
      // DeepSeek reports usage in the final streamed chunk; the Rust
      // real-provider test sets the same compatibility flags.
      compat: { supportsDeveloperRole: false, supportsReasoningEffort: false, supportsUsageInStreaming: true },
    },
    compaction: { enabled: true, reserve_tokens: 8192, keep_recent_tokens: 4096 },
    retry: { enabled: false, max_retries: 0 },
  };
}

async function openLiveSession(
  helper: HelperClient,
  baseUrl: string,
  model: string,
  apiKey: string,
): Promise<string> {
  helper.send("hello", {});
  await helper.waitFor((frame) => frame.kind === "hello.ok", TIMEOUT_MS);
  helper.send("session.open", liveSessionConfig(baseUrl, helper.dataDir, model, apiKey), identity("open"));
  const opened = await helper.waitFor((frame) => frame.kind === "session.opened", TIMEOUT_MS);
  const sessionFile = opened.data?.session_file;
  assert.ok(typeof sessionFile === "string" && sessionFile.length > 0, "session.opened should carry a session file");
  return sessionFile;
}

async function waitSettled(helper: HelperClient, turnId: string): Promise<HelperFrame> {
  const frame = await helper.waitFor(
    (candidate) =>
      (candidate.kind === "turn.completed" || candidate.kind === "turn.failed" || candidate.kind === "turn.cancelled") &&
      candidate.turn_id === turnId,
    TIMEOUT_MS,
  );
  if (frame.kind !== "turn.completed") {
    throw new Error(`live turn ${turnId} did not complete: ${frame.kind} ${JSON.stringify(frame.data)}`);
  }
  return frame;
}

function assistantUsagesFromSession(sessionFile: string): Array<Record<string, unknown>> {
  const usages: Array<Record<string, unknown>> = [];
  for (const line of readFileSync(sessionFile, "utf8").split("\n")) {
    if (line.trim().length === 0) continue;
    const entry = JSON.parse(line) as { type?: string; message?: { role?: string; usage?: unknown } };
    if (entry.type !== "message" || entry.message?.role !== "assistant") continue;
    if (typeof entry.message.usage === "object" && entry.message.usage !== null) {
      usages.push(entry.message.usage as Record<string, unknown>);
    }
  }
  return usages;
}

function finite(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

/** The helper event must be the provider's usage after the SDK's documented mapping. */
function assertMatchesProvider(usage: Record<string, unknown>, raw: Record<string, unknown>, label: string): void {
  const promptDetails = (raw.prompt_tokens_details ?? {}) as Record<string, unknown>;
  const completionDetails = (raw.completion_tokens_details ?? {}) as Record<string, unknown>;
  const promptTokens = finite(raw.prompt_tokens);
  const cached = finite(promptDetails.cached_tokens ?? raw.prompt_cache_hit_tokens);
  const cacheWrite = finite(promptDetails.cache_write_tokens);
  const input = Math.max(0, promptTokens - cached - cacheWrite);
  const output = finite(raw.completion_tokens);
  const reasoning = finite(completionDetails.reasoning_tokens);
  assert.equal(usage.input_tokens, input, `${label}: input`);
  assert.equal(usage.cache_read_tokens, cached, `${label}: cache_read`);
  assert.equal(usage.cache_write_tokens, cacheWrite, `${label}: cache_write`);
  assert.equal(usage.output_tokens, output, `${label}: output`);
  assert.equal(usage.reasoning_tokens ?? 0, reasoning, `${label}: reasoning`);
  assert.equal(usage.total_tokens, input + output + cached + cacheWrite, `${label}: total`);
  if (typeof raw.total_tokens === "number") {
    assert.equal(usage.total_tokens, raw.total_tokens, `${label}: raw total`);
  }
  assert.deepEqual(usage.cost, { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 }, `${label}: cost`);
}

/** The persisted SDK usage and the emitted event must agree field for field. */
function assertMatchesSession(usage: Record<string, unknown>, persisted: Record<string, unknown>, label: string): void {
  assert.equal(usage.input_tokens, persisted.input, `${label}: session input`);
  assert.equal(usage.output_tokens, persisted.output, `${label}: session output`);
  assert.equal(usage.cache_read_tokens, persisted.cacheRead, `${label}: session cache_read`);
  assert.equal(usage.cache_write_tokens, persisted.cacheWrite, `${label}: session cache_write`);
  assert.equal(usage.total_tokens, persisted.totalTokens, `${label}: session total`);
}

function assertPlausibleTimings(data: Record<string, unknown>, label: string): void {
  const durationMs = data.duration_ms as number;
  assert.ok(typeof durationMs === "number" && durationMs > 0, `${label}: expected duration_ms > 0`);
  const firstTokenMs = data.first_token_ms as number | undefined;
  if (firstTokenMs !== undefined) {
    assert.ok(
      typeof firstTokenMs === "number" && firstTokenMs >= 0 && firstTokenMs <= durationMs,
      `${label}: first_token_ms ${firstTokenMs} outside 0..${durationMs}`,
    );
  }
  const tokensPerSecond = data.output_tokens_per_second as number | undefined;
  if (tokensPerSecond !== undefined) {
    assert.ok(
      typeof tokensPerSecond === "number" && tokensPerSecond > 0 && tokensPerSecond < 5000,
      `${label}: implausible tokens/sec ${String(tokensPerSecond)}`,
    );
  }
}

/** Numbers-only diagnostic so a live run records the observed event shapes. */
function recordUsage(label: string, data: Record<string, unknown>): void {
  const usage = data.usage as Record<string, unknown>;
  console.log(
    `[live-usage] ${label} message=${String(data.message_id)} stop=${String(data.stop_reason)} ` +
      `in=${String(usage.input_tokens)} out=${String(usage.output_tokens)} ` +
      `cache_read=${String(usage.cache_read_tokens)} cache_write=${String(usage.cache_write_tokens)} ` +
      `reasoning=${String(usage.reasoning_tokens ?? 0)} total=${String(usage.total_tokens)} ` +
      `duration_ms=${String(data.duration_ms)} first_token_ms=${String(data.first_token_ms ?? "n/a")} ` +
      `tok_s=${String(data.output_tokens_per_second ?? "n/a")}`,
  );
}

for (const model of MODELS) {
  test(
    `live provider [${model}]: assistant.usage matches the provider report`,
    { skip: KEY === undefined ? SKIP_REASON : false, timeout: TIMEOUT_MS + 30_000 },
    async () => {
      const apiKey = KEY as string;
      const proxy = new RecordingProxy();
      await proxy.start(BASE_URL);
      const helper = await HelperClient.start();
      try {
        const sessionFile = await openLiveSession(helper, proxy.baseUrl, model, apiKey);
        helper.send("turn.start", { prompt: "Reply with exactly one word: pong" }, identity("turn-1"));
        await waitSettled(helper, "turn-1");

        const usageFrames = helper.seen.filter((frame) => frame.kind === "assistant.usage");
        assert.equal(usageFrames.length, 1, `expected one assistant.usage, saw ${helper.seen.map((f) => f.kind).join(", ")}`);
        const data = usageFrames[0].data as Record<string, unknown>;
        const usage = data.usage as Record<string, unknown>;
        assert.equal(data.model_id, model);
        assert.equal(data.stop_reason, "stop");
        assert.equal(proxy.usages.length, 1, "expected one provider usage report");
        assertMatchesProvider(usage, proxy.usages[0], `${model} text`);
        const persisted = assistantUsagesFromSession(sessionFile);
        assert.equal(persisted.length, 1);
        assertMatchesSession(usage, persisted[0], `${model} text`);
        assert.ok((usage.input_tokens as number) + (usage.cache_read_tokens as number) > 0, "prompt tokens should be positive");
        assert.ok((usage.output_tokens as number) > 0, "output tokens should be positive");
        assertPlausibleTimings(data, `${model} text`);
        recordUsage(`${model} text`, data);

        const contextFrames = helper.seen.filter((frame) => frame.kind === "context.updated");
        assert.ok(contextFrames.length >= 1, "context.updated missing");
        const context = contextFrames.at(-1)?.data as Record<string, unknown>;
        assert.equal(context.source, "usage");
        assert.equal(context.context_window, 65536);
        assert.equal(typeof context.tokens, "number");
        assert.ok((context.tokens as number) > 0);
        assert.equal(typeof context.percent, "number");
        assert.ok((context.percent as number) > 0 && (context.percent as number) <= 100);
      } finally {
        await helper.dispose();
        await proxy.stop();
      }
    },
  );
}

test(
  `live provider [${MODELS[0]}]: usage fires for tool-call rounds`,
  { skip: KEY === undefined ? SKIP_REASON : false, timeout: TIMEOUT_MS + 30_000 },
  async () => {
    const apiKey = KEY as string;
    const model = MODELS[0];
    const proxy = new RecordingProxy();
    await proxy.start(BASE_URL);
    const helper = await HelperClient.start();
    try {
      const sessionFile = await openLiveSession(helper, proxy.baseUrl, model, apiKey);
      helper.send(
        "turn.start",
        {
          prompt:
            "Call the bash tool now, before writing any text. Run exactly this command: echo live-usage-tool-marker",
        },
        identity("turn-2"),
      );
      const calls = await helper.waitFor((frame) => frame.kind === "tool.calls" && frame.turn_id === "turn-2", TIMEOUT_MS);
      const callList = calls.data?.calls as Array<{ tool_call_id: string; name: string }>;
      assert.ok(callList.length >= 1, "expected a brokered tool call");
      assert.equal(callList[0].name, "workspace.shell");
      helper.send(
        "turn.resume",
        { results: [{ tool_call_id: callList[0].tool_call_id, status: "success", content: "live-usage-tool-marker\n" }] },
        identity("turn-2"),
      );
      await waitSettled(helper, "turn-2");

      const usageFrames = helper.seen.filter(
        (frame) => frame.kind === "assistant.usage" && frame.turn_id === "turn-2",
      );
      assert.ok(usageFrames.length >= 2, `expected a usage event per assistant message, saw ${usageFrames.length}`);
      const toolRound = usageFrames[0].data as Record<string, unknown>;
      const finalRound = usageFrames.at(-1)?.data as Record<string, unknown>;
      assert.equal(toolRound.stop_reason, "toolUse", "the first assistant message is the tool-call round");
      assert.equal(finalRound.stop_reason, "stop");
      assert.notEqual(toolRound.message_id, finalRound.message_id);

      assert.equal(proxy.usages.length, usageFrames.length, "one provider usage report per assistant message");
      const persisted = assistantUsagesFromSession(sessionFile);
      assert.equal(persisted.length, usageFrames.length, "one persisted assistant usage per event");
      usageFrames.forEach((frame, index) => {
        const usage = frame.data?.usage as Record<string, unknown>;
        assertMatchesProvider(usage, proxy.usages[index], `${model} round ${index}`);
        assertMatchesSession(usage, persisted[index], `${model} round ${index}`);
        assertPlausibleTimings(frame.data as Record<string, unknown>, `${model} round ${index}`);
        assert.ok((usage.input_tokens as number) > 0, `round ${index}: input tokens should be positive`);
        recordUsage(`${model} round ${index}`, frame.data as Record<string, unknown>);
      });
      const toolContext = (toolRound.usage as Record<string, unknown>).input_tokens as number;
      const toolCacheRead = (toolRound.usage as Record<string, unknown>).cache_read_tokens as number;
      const finalContext = (finalRound.usage as Record<string, unknown>).input_tokens as number;
      const finalCacheRead = (finalRound.usage as Record<string, unknown>).cache_read_tokens as number;
      assert.ok(
        finalContext + finalCacheRead >= toolContext + toolCacheRead,
        "the final round should carry at least the tool-round context",
      );
    } finally {
      await helper.dispose();
      await proxy.stop();
    }
  },
);
