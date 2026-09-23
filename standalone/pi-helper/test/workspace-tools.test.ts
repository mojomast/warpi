import { test } from "node:test";
import assert from "node:assert/strict";
import {
  canonicalCall,
  createWorkspaceTools,
  STANDALONE_SYSTEM_GUIDANCE,
  WORKSPACE_TOOL_NAMES,
  WorkspaceToolBroker,
  type BrokerHost,
} from "../src/workspace-tools.ts";
import { deriveCompactionSettings } from "../src/runtime.ts";

class RecordingHost implements BrokerHost {
  batches: Array<{ owner: string; calls: Array<{ tool_call_id: string; name: string }> }> = [];
  emitToolBatch(ownerKey: string, calls: Array<{ tool_call_id: string; name: string }>): void {
    this.batches.push({ owner: ownerKey, calls });
  }
}

test("canonical calls map model tools to workspace calls", () => {
  assert.equal(canonicalCall("bash", {}), "workspace.shell");
  assert.equal(canonicalCall("read", {}), "workspace.read_file");
  assert.equal(canonicalCall("write", {}), "workspace.write_file");
  assert.equal(canonicalCall("edit", {}), "workspace.edit_file");
  assert.equal(canonicalCall("glob", {}), "workspace.glob");
  assert.equal(canonicalCall("grep", {}), "workspace.grep");
  assert.equal(canonicalCall("bash_output", {}), "workspace.read_shell_command_output");
});

test("registered tools expose the non-interactive guardrails to the model", () => {
  const tools = createWorkspaceTools("owner-1", new WorkspaceToolBroker(new RecordingHost()));
  assert.deepEqual(
    tools.map((tool) => tool.name).sort(),
    ["bash", "bash_output", "edit", "glob", "grep", "read", "write"],
  );

  const bash = tools.find((tool) => tool.name === "bash");
  assert.ok(bash !== undefined, "bash tool is registered");
  for (const phrase of ["no stdin", "non-interactive", "BatchMode", "timeout 60s", "bash_output"]) {
    assert.match(bash.description, new RegExp(phrase), `bash description should mention ${phrase}`);
  }

  const bashOutput = tools.find((tool) => tool.name === "bash_output");
  assert.ok(bashOutput !== undefined, "bash_output tool is registered");
  assert.match(bashOutput.description, /maximum 120/);
  assert.match(bashOutput.description, /cannot stop it/);

  for (const phrase of ["no stdin", "non-interactive", "tail -f", "BatchMode", "timeout 60s"]) {
    assert.match(STANDALONE_SYSTEM_GUIDANCE, new RegExp(phrase), `guidance should mention ${phrase}`);
  }
  assert.deepEqual(
    [...WORKSPACE_TOOL_NAMES].sort(),
    ["bash", "bash_output", "edit", "glob", "grep", "read", "write"],
  );
});

test("broker batches calls per owner, then resolves them independently", async () => {
  const host = new RecordingHost();
  const broker = new WorkspaceToolBroker(host);
  const first = broker.execute("s1", "call-1", "read", { path: "a.txt" }, undefined);
  const second = broker.execute("s1", "call-2", "grep", { pattern: "x" }, undefined);
  const foreign = broker.execute("s2", "call-3", "read", { path: "b.txt" }, undefined);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(host.batches.length, 2, "one batch per owner");
  assert.deepEqual(host.batches[0].calls.map((call) => call.tool_call_id).sort(), ["call-1", "call-2"]);
  assert.deepEqual(host.batches[1].calls.map((call) => call.tool_call_id), ["call-3"]);

  assert.equal(broker.deliver("call-3", "content-b", false), true);
  const resolvedForeign = await foreign;
  assert.equal((resolvedForeign.content[0] as { text: string }).text, "content-b");
  assert.equal(await broker.deliver("call-1", "content-a", false), true);
  assert.equal(broker.deliver("call-1", "duplicate", false), false, "duplicate delivery is rejected");
  assert.equal(broker.deliver("unknown", "x", false), false, "foreign delivery is rejected");
  const resolvedFirst = await first;
  assert.equal((resolvedFirst.content[0] as { text: string }).text, "content-a");
  assert.equal(broker.pendingIdsFor("s1").length, 1, "the other call is still suspended");
  broker.cancelOwner("s1", "cancelled");
  await assert.rejects(second, /cancelled/);
});

test("broker rejects duplicate tool call ids and oversized arguments", async () => {
  const broker = new WorkspaceToolBroker(new RecordingHost());
  const first = broker.execute("s1", "call-1", "read", { path: "a" }, undefined);
  assert.throws(() => broker.execute("s1", "call-1", "read", { path: "b" }, undefined), /duplicate/);
  const oversized = broker.execute("s1", "call-2", "write", { path: "a", content: "x".repeat(600 * 1024) }, undefined);
  const result = await oversized;
  assert.match((result.content[0] as { text: string }).text, /exceeded the .* byte limit/);
  assert.deepEqual(broker.pendingIdsFor("s1"), ["call-1"], "the unrelated pending call is untouched");
  broker.cancelOwner("s1", "cancelled");
  await assert.rejects(first, /cancelled/);
});

test("aborting a signal releases the suspended call", async () => {
  const broker = new WorkspaceToolBroker(new RecordingHost());
  const controller = new AbortController();
  const pending = broker.execute("s1", "call-1", "bash", { command: "ls" }, controller.signal);
  controller.abort();
  await assert.rejects(pending, /aborted/);
  assert.equal(broker.pendingCount, 0);
});

test("compaction settings scale with the context window", () => {
  const small = deriveCompactionSettings(8192, 2048);
  assert.equal(small.enabled, true);
  assert.equal(small.reserveTokens, 2048);
  assert.equal(small.keepRecentTokens, 2048);
  const large = deriveCompactionSettings(200_000, 64_000);
  assert.equal(large.reserveTokens, 16384);
  assert.equal(large.keepRecentTokens, 20000);
});
