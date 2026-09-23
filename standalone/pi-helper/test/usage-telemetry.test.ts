/**
 * Token-observability events: `assistant.usage`, `context.updated`, and the
 * extended `compaction.finished` (spec:
 * `standalone/research/token-observability-spec.md` §2). Drives the real helper
 * over the v1 stdio protocol against the deterministic fake provider.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { FakeProvider, fragment } from "./fake-provider.ts";
import { HelperClient, type HelperFrame } from "./helper-client.ts";
import { toAgentUsage } from "../src/runtime.ts";

interface SessionOverrides {
  contextWindow?: number;
  maxOutputTokens?: number;
  reserveTokens?: number;
  keepRecentTokens?: number;
}

function sessionConfig(providerBaseUrl: string, sessionDir: string, overrides: SessionOverrides = {}) {
  return {
    working_dir: sessionDir,
    agent_dir: join(sessionDir, "agent"),
    session_dir: join(sessionDir, "sessions"),
    load_context_files: false,
    system_prompt: "You are a fixture agent.",
    provider: {
      provider_id: "warpi-fixture",
      name: "fixture",
      base_url: providerBaseUrl,
      api: "openai-completions",
      auth: { kind: "none" },
      model_id: "fixture-model",
      model_name: "fixture-model",
      context_window: overrides.contextWindow ?? 32768,
      max_output_tokens: overrides.maxOutputTokens ?? 4096,
      reasoning: false,
      compat: { supportsDeveloperRole: false, supportsReasoningEffort: false },
    },
    compaction: {
      enabled: true,
      reserve_tokens: overrides.reserveTokens ?? 4096,
      keep_recent_tokens: overrides.keepRecentTokens ?? 2048,
    },
    retry: { enabled: false, max_retries: 0 },
  };
}

const identity = (overrides: Partial<HelperFrame> = {}): Partial<HelperFrame> => ({
  session_id: "conv-1",
  turn_id: "turn-1",
  exchange_id: "ex-1",
  generation: 0,
  ...overrides,
});

async function openSession(
  helper: HelperClient,
  providerBaseUrl: string,
  overrides: SessionOverrides = {},
): Promise<HelperFrame> {
  helper.send("hello", {});
  await helper.waitFor((frame) => frame.kind === "hello.ok");
  helper.send("session.open", sessionConfig(providerBaseUrl, helper.dataDir, overrides), identity());
  return helper.waitFor((frame) => frame.kind === "session.opened");
}

test("assistant.usage carries SDK usage and timings; context.updated reports the window", async () => {
  const provider = new FakeProvider([
    {
      kind: "text",
      chunks: fragment("hello there", 4),
      usage: {
        prompt_tokens: 100,
        completion_tokens: 20,
        total_tokens: 120,
        completion_tokens_details: { reasoning_tokens: 7 },
      },
    },
  ]);
  await provider.start();
  const helper = await HelperClient.start();
  try {
    const opened = await openSession(helper, provider.baseUrl);
    assert.equal(opened.data?.context_window, 32768);
    assert.equal(opened.data?.max_output_tokens, 4096);

    helper.send("turn.start", { prompt: "say hi" }, identity());
    const usageFrame = await helper.waitFor((frame) => frame.kind === "assistant.usage");
    assert.equal(usageFrame.session_id, "conv-1");
    assert.equal(usageFrame.turn_id, "turn-1");
    assert.equal(usageFrame.exchange_id, "ex-1");
    const data = usageFrame.data as Record<string, unknown>;
    const message = helper.seen.find((frame) => frame.kind === "assistant.message");
    assert.equal(data.message_id, message?.data?.message_id);
    assert.equal(data.model_id, "fixture-model");
    assert.equal(data.api, "openai-completions");
    assert.equal(data.stop_reason, "stop");
    assert.deepEqual(data.usage, {
      input_tokens: 100,
      output_tokens: 20,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      reasoning_tokens: 7,
      total_tokens: 120,
      cost: { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 },
    });

    const durationMs = data.duration_ms as number;
    assert.equal(typeof durationMs, "number");
    assert.ok(durationMs >= 0, `duration_ms should be non-negative, got ${durationMs}`);
    const firstTokenMs = data.first_token_ms as number | undefined;
    if (firstTokenMs !== undefined) {
      assert.ok(firstTokenMs >= 0 && firstTokenMs <= durationMs, `first_token_ms ${firstTokenMs} vs ${durationMs}`);
    }
    const tokensPerSecond = data.output_tokens_per_second as number | undefined;
    if (durationMs > 0) {
      assert.ok(
        tokensPerSecond !== undefined && tokensPerSecond > 0,
        `expected a positive tokens/sec reading, got ${String(tokensPerSecond)}`,
      );
    }

    const contextFrame = await helper.waitFor((frame) => frame.kind === "context.updated");
    const context = contextFrame.data as Record<string, unknown>;
    assert.equal(context.source, "usage");
    assert.equal(context.tokens, 120);
    assert.equal(context.context_window, 32768);
    assert.equal(context.percent, 0.37);

    const completed = await helper.waitFor((frame) => frame.kind === "turn.completed");
    const completedUsage = completed.data?.usage as Record<string, unknown>;
    assert.equal(completedUsage.reasoning_tokens, 7);
    assert.deepEqual(completedUsage.cost, { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 });
  } finally {
    await helper.dispose();
    await provider.stop();
  }
});

test("empty usage still emits a well-formed assistant.usage", async () => {
  const provider = new FakeProvider([{ kind: "text", chunks: ["no usage reported"], usage: false }]);
  await provider.start();
  const helper = await HelperClient.start();
  try {
    await openSession(helper, provider.baseUrl);
    helper.send("turn.start", { prompt: "say hi" }, identity());
    const usageFrame = await helper.waitFor((frame) => frame.kind === "assistant.usage");
    const data = usageFrame.data as Record<string, unknown>;
    const usage = data.usage as Record<string, unknown>;
    assert.equal(usage.input_tokens, 0);
    assert.equal(usage.output_tokens, 0);
    assert.equal(usage.total_tokens, 0);
    assert.equal(typeof data.duration_ms, "number");
    assert.equal(data.stop_reason, "stop");
    assert.equal(data.message_id, "turn-1:a1");

    const contextFrame = await helper.waitFor((frame) => frame.kind === "context.updated");
    const context = contextFrame.data as Record<string, unknown>;
    assert.equal(context.source, "usage");
    assert.equal(context.context_window, 32768);
    assert.equal(context.tokens === null, context.percent === null);
    await helper.waitFor((frame) => frame.kind === "turn.completed");
  } finally {
    await helper.dispose();
    await provider.stop();
  }
});

test("compaction.finished reports token deltas, summary usage and duration", async () => {
  const provider = new FakeProvider([
    {
      kind: "text",
      chunks: fragment("acknowledged", 4),
      usage: {
        prompt_tokens: 900,
        completion_tokens: 10,
        total_tokens: 910,
      },
    },
    {
      kind: "text",
      chunks: fragment("done", 4),
      usage: {
        prompt_tokens: 900,
        completion_tokens: 10,
        total_tokens: 910,
      },
    },
    {
      kind: "text",
      chunks: fragment("summary of the prior turn", 6),
      usage: {
        prompt_tokens: 300,
        completion_tokens: 40,
        total_tokens: 340,
      },
    },
  ]);
  await provider.start();
  const helper = await HelperClient.start();
  try {
    await openSession(helper, provider.baseUrl, {
      contextWindow: 1024,
      maxOutputTokens: 512,
      reserveTokens: 256,
      keepRecentTokens: 64,
    });
    helper.send("turn.start", { prompt: "hello" }, identity());
    await helper.waitFor((frame) => frame.kind === "turn.completed" && frame.turn_id === "turn-1");

    // A second turn gives the compactor history to summarize while keeping the
    // recent turn intact (the first turn alone has nothing to cut).
    helper.send(
      "turn.start",
      { prompt: `please continue ${"x".repeat(300)}` },
      identity({ turn_id: "turn-2", exchange_id: "ex-2" }),
    );
    await helper.waitFor((frame) => frame.kind === "turn.completed" && frame.turn_id === "turn-2");

    const finished = helper.seen.find((frame) => frame.kind === "compaction.finished");
    assert.ok(finished !== undefined, `compaction.finished missing; saw ${helper.seen.map((f) => f.kind).join(", ")}`);
    const data = finished.data as Record<string, unknown>;
    assert.equal(data.reason, "threshold");
    assert.equal(data.summarized, true);
    assert.equal(data.tokens_before, 910);
    const tokensAfter = data.tokens_after as number;
    assert.equal(typeof tokensAfter, "number");
    assert.ok(tokensAfter < 910, `tokens_after ${tokensAfter} should be below tokens_before`);
    assert.deepEqual(data.summary_usage, {
      input_tokens: 300,
      output_tokens: 40,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      reasoning_tokens: 0,
      total_tokens: 340,
      cost: { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 },
    });
    const durationMs = data.duration_ms as number;
    assert.ok(typeof durationMs === "number" && durationMs > 0, `expected duration_ms > 0, got ${durationMs}`);

    const contextFrames = helper.seen.filter((frame) => frame.kind === "context.updated");
    const before = contextFrames.find((frame) => frame.data?.source === "usage");
    const after = contextFrames.find((frame) => frame.data?.source === "compaction_estimate");
    assert.ok(before !== undefined && after !== undefined, "both usage and compaction-estimate readings expected");
    assert.equal(before.data?.tokens, 910);
    assert.equal(before.data?.percent, 88.87);
    assert.equal(after.data?.tokens, tokensAfter);
    assert.equal(after.data?.context_window, 1024);
    assert.equal(after.data?.percent, Math.round((tokensAfter / 1024) * 10_000) / 100);
    assert.ok((after.data?.percent as number) < (before.data?.percent as number));
  } finally {
    await helper.dispose();
    await provider.stop();
  }
});

test("toAgentUsage maps missing usage to zeros and passes cost through", () => {
  assert.deepEqual(toAgentUsage(undefined), {
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    total_tokens: 0,
  });
  assert.deepEqual(
    toAgentUsage({
      input: 12,
      output: 3,
      cacheRead: 4,
      cacheWrite: 5,
      reasoning: 2,
      totalTokens: 24,
      cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.3 },
    }),
    {
      input_tokens: 12,
      output_tokens: 3,
      cache_read_tokens: 4,
      cache_write_tokens: 5,
      reasoning_tokens: 2,
      total_tokens: 24,
      cost: { input: 0.1, output: 0.2, cache_read: 0, cache_write: 0, total: 0.3 },
    },
  );
});
