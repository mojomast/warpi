/**
 * End-to-end helper smoke test: model -> brokered tool -> Warp result ->
 * same Pi prompt resumes -> second provider request -> final text.
 *
 * This is the helper half of the M1 vertical slice. The Rust adapter test
 * (`crates/standalone_agent/tests/vertical_slice.rs`) runs the same helper with
 * the real Warp protobuf event layer.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { FakeProvider, fragment } from "./fake-provider.ts";
import { HelperClient, type HelperFrame } from "./helper-client.ts";

function sessionConfig(providerBaseUrl: string, apiKey: string | undefined, sessionDir: string) {
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
      auth: apiKey === undefined ? { kind: "none" } : { kind: "api_key", api_key: apiKey },
      model_id: "fixture-model",
      model_name: "fixture-model",
      context_window: 32768,
      max_output_tokens: 4096,
      reasoning: false,
      compat: { supportsDeveloperRole: false, supportsReasoningEffort: false },
    },
    compaction: { enabled: true, reserve_tokens: 4096, keep_recent_tokens: 2048 },
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

for (const apiKey of ["sk-fixture-key", undefined]) {
  test(`round trip: brokered tool, resume, second request (api key: ${apiKey ?? "none"})`, async () => {
    const provider = new FakeProvider([
      {
        kind: "tool_call",
        toolCallId: "call_abc",
        toolName: "bash",
        // Fragment the JSON arguments to exercise partial-stream handling.
        argumentChunks: fragment(JSON.stringify({ command: "echo hello" }), 5),
      },
      { kind: "text", chunks: fragment("all done", 3) },
    ]);
    await provider.start();
    const helper = await HelperClient.start();
    try {
      helper.send("hello", {});
      const hello = await helper.waitFor((frame) => frame.kind === "hello.ok");
      assert.equal(hello.data?.helper_version, "0.1.0");
      const capabilities = hello.data?.capabilities as { brokered_tools: string[] };
      assert.deepEqual(capabilities.brokered_tools.slice().sort(), ["bash", "edit", "glob", "grep", "read", "write"]);

      helper.send("session.open", sessionConfig(provider.baseUrl, apiKey, helper.dataDir), identity());
      const opened = await helper.waitFor((frame) => frame.kind === "session.opened");
      assert.deepEqual((opened.data?.active_tools as string[]).slice().sort(), [
        "bash",
        "edit",
        "glob",
        "grep",
        "read",
        "write",
      ]);
      assert.equal(opened.data?.model_id, "fixture-model");
      assert.ok(typeof opened.data?.session_file === "string");

      helper.send("turn.start", { prompt: "run echo hello" }, identity());
      await helper.waitFor((frame) => frame.kind === "turn.started");
      const calls = await helper.waitFor((frame) => frame.kind === "tool.calls");
      const callList = calls.data?.calls as Array<{ tool_call_id: string; name: string; arguments: unknown }>;
      assert.equal(callList.length, 1);
      assert.equal(callList[0].tool_call_id, "call_abc");
      assert.equal(callList[0].name, "workspace.shell");
      assert.deepEqual(callList[0].arguments, { command: "echo hello" });
      const awaiting = await helper.waitFor((frame) => frame.kind === "turn.awaiting_tools");
      assert.deepEqual(awaiting.data?.pending, ["call_abc"]);
      assert.equal(provider.requestCount, 1);

      // Foreign and stale results must be rejected without touching the pending call.
      helper.send("turn.resume", { results: [{ tool_call_id: "someone-elses-call", status: "success", content: "x" }] }, identity());
      const rejected = await helper.waitFor((frame) => frame.kind === "error");
      assert.equal(rejected.data?.code, "unknown_tool_result");
      assert.equal(provider.requestCount, 1);

      helper.send("turn.resume", { results: [{ tool_call_id: "call_abc", status: "success", content: "hello\n" }] }, identity());
      const finalText = await helper.waitFor((frame) => frame.kind === "assistant.message");
      assert.equal(finalText.data?.text, "all done");
      const completed = await helper.waitFor((frame) => frame.kind === "turn.completed");
      assert.equal(completed.data?.stop_reason, "stop");
      const usage = completed.data?.usage as { input_tokens: number; output_tokens: number };
      assert.ok(usage.input_tokens > 0);

      assert.equal(provider.requestCount, 2, "exactly two provider requests");
      const second = provider.captures[1].body as { messages: Array<Record<string, unknown>> };
      const roles = second.messages.map((message) => message.role);
      assert.equal(roles.filter((role) => role === "user").length, 1, "no duplicated user history");
      assert.ok(roles.includes("tool"), "tool result present in the continuation");
      const toolMessage = second.messages.find((message) => message.role === "tool") as {
        content: string;
        tool_call_id: string;
      };
      assert.equal(toolMessage.tool_call_id, "call_abc");
      assert.match(toolMessage.content, /hello/);

      // Authentication behavior on the wire.
      const authHeader = provider.captures[0].headers.authorization;
      if (apiKey === undefined) {
        assert.equal(authHeader, undefined, "auth=none must not send an Authorization header");
      } else {
        assert.equal(authHeader, `Bearer ${apiKey}`);
      }

      // Same-session idempotent open must not reset the session.
      helper.send("session.open", sessionConfig(provider.baseUrl, apiKey, helper.dataDir), identity());
      const reopened = await helper.waitFor(
        (frame) => frame.kind === "session.opened" && (frame.data?.resumed as boolean) === true,
      );
      assert.equal(reopened.data?.session_file, opened.data?.session_file);

      helper.send("shutdown", {});
      await helper.waitFor((frame) => frame.kind === "shutdown.ack");
    } finally {
      await helper.dispose();
      await provider.stop();
    }
  });
}

test("turn.cancel settles the turn and leaves the session usable", async () => {
  const provider = new FakeProvider([
    { kind: "tool_call", toolCallId: "call_hang", toolName: "bash", argumentChunks: [JSON.stringify({ command: "sleep 30" })] },
    { kind: "tool_call", toolCallId: "call_two", toolName: "read", argumentChunks: [JSON.stringify({ path: "a.txt" })] },
    { kind: "text", chunks: ["after cancel"] },
  ]);
  await provider.start();
  const helper = await HelperClient.start();
  try {
    helper.send("hello", {});
    await helper.waitFor((frame) => frame.kind === "hello.ok");
    helper.send("session.open", sessionConfig(provider.baseUrl, "sk-x", helper.dataDir), identity());
    await helper.waitFor((frame) => frame.kind === "session.opened");
    helper.send("turn.start", { prompt: "start a long command" }, identity());
    await helper.waitFor((frame) => frame.kind === "turn.awaiting_tools");

    helper.send("turn.cancel", { reason: "user cancelled" }, identity());
    const cancelled = await helper.waitFor((frame) => frame.kind === "turn.cancelled");
    assert.match(String(cancelled.data?.reason), /user cancelled/);

    helper.send("turn.start", { prompt: "second turn" }, identity({ turn_id: "turn-2", exchange_id: "ex-2" }));
    await helper.waitFor((frame) => frame.kind === "turn.started" && frame.turn_id === "turn-2");
    const secondCall = await helper.waitFor((frame) => frame.kind === "tool.calls" && frame.turn_id === "turn-2");
    const calls = secondCall.data?.calls as Array<{ tool_call_id: string; name: string }>;
    assert.deepEqual(calls.map((call) => call.tool_call_id), ["call_two"]);
    helper.send(
      "turn.resume",
      { results: [{ tool_call_id: "call_two", status: "rejected", content: "no" }] },
      identity({ turn_id: "turn-2", exchange_id: "ex-2" }),
    );
    const completed = await helper.waitFor(
      (frame) => (frame.kind === "turn.failed" || frame.kind === "turn.completed") && frame.turn_id === "turn-2",
    );
    if (completed.kind === "turn.failed") throw new Error(`unexpected failure: ${JSON.stringify(completed.data)}`);
    assert.equal(provider.requestCount, 3);
  } finally {
    await helper.dispose();
    await provider.stop();
  }
});

test("provider error surfaces as turn.failed without retry storm", async () => {
  const provider = new FakeProvider([{ kind: "error", status: 401, body: JSON.stringify({ error: { message: "bad key" } }) }]);
  await provider.start();
  const helper = await HelperClient.start();
  try {
    helper.send("hello", {});
    await helper.waitFor((frame) => frame.kind === "hello.ok");
    helper.send("session.open", sessionConfig(provider.baseUrl, "sk-bad", helper.dataDir), identity());
    await helper.waitFor((frame) => frame.kind === "session.opened");
    helper.send("turn.start", { prompt: "hello" }, identity());
    const failed = await helper.waitFor((frame) => frame.kind === "turn.failed");
    assert.ok(["provider_error", "internal_error"].includes(String(failed.data?.code)));
    assert.equal(provider.requestCount, 1, "no automatic retry when retry is disabled");
  } finally {
    await helper.dispose();
    await provider.stop();
  }
});
