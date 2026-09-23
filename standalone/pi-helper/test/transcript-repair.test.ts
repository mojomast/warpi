import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SessionManager, type SessionEntry } from "@earendil-works/pi-coding-agent";
import { repairDanglingToolCalls } from "../src/runtime.ts";

function messageEntries(manager: SessionManager) {
  return (manager.getEntries() as SessionEntry[]).filter((entry) => entry.type === "message");
}

test("repairDanglingToolCalls appends a synthetic result for a dangling call", async () => {
  const dir = await mkdtemp(join(tmpdir(), "warpi-repair-"));
  try {
    const manager = SessionManager.create(dir, dir);
    manager.appendMessage({
      role: "user",
      content: "please run the command",
      timestamp: Date.now(),
    } as never);
    manager.appendMessage({
      role: "assistant",
      content: [
        {
          type: "toolCall",
          id: "call_dangle",
          name: "bash",
          arguments: { command: "echo hi" },
        },
      ],
      api: "openai-completions",
      provider: "warpi-fixture",
      model: "fixture-model",
      usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2 },
      stopReason: "toolUse",
      timestamp: Date.now(),
    } as never);

    const repaired = repairDanglingToolCalls(manager);

    assert.equal(repaired, 1);
    const entries = messageEntries(manager);
    const result = entries.at(-1)?.message as unknown as {
      role: string;
      toolCallId: string;
      isError: boolean;
      content: Array<{ text?: string }>;
    };
    assert.equal(result.role, "toolResult");
    assert.equal(result.toolCallId, "call_dangle");
    assert.equal(result.isError, true);
    assert.match(result.content[0]?.text ?? "", /never ran/);

    // A second repair is a no-op: the call now has a result.
    assert.equal(repairDanglingToolCalls(manager), 0);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("repairDanglingToolCalls repairs a child session transcript in the children dir", async () => {
  const dir = await mkdtemp(join(tmpdir(), "warpi-repair-child-"));
  try {
    const childDir = join(dir, "children", "conv-1");
    await mkdir(childDir, { recursive: true });
    const manager = SessionManager.create(dir, childDir, { id: "task-1", parentSession: "conv-1" });
    manager.appendMessage({
      role: "assistant",
      content: [
        { type: "toolCall", id: "call_dangle_child", name: "read", arguments: { path: "x.txt" } },
      ],
      api: "openai-completions",
      provider: "warpi-fixture",
      model: "fixture-model",
      usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2 },
      stopReason: "toolUse",
      timestamp: Date.now(),
    } as never);

    assert.equal(repairDanglingToolCalls(manager), 1);
    const file = manager.getSessionFile();
    assert.ok(file !== undefined && file.includes(join("children", "conv-1")), "child transcript is scoped");
    const entries = messageEntries(manager);
    const result = entries.at(-1)?.message as unknown as { role: string; toolCallId: string };
    assert.equal(result.role, "toolResult");
    assert.equal(result.toolCallId, "call_dangle_child");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("repairDanglingToolCalls leaves answered calls alone", async () => {
  const dir = await mkdtemp(join(tmpdir(), "warpi-repair-"));
  try {
    const manager = SessionManager.create(dir, dir);
    manager.appendMessage({
      role: "assistant",
      content: [
        { type: "toolCall", id: "call_ok", name: "bash", arguments: { command: "echo hi" } },
      ],
      api: "openai-completions",
      provider: "warpi-fixture",
      model: "fixture-model",
      usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2 },
      stopReason: "toolUse",
      timestamp: Date.now(),
    } as never);
    manager.appendMessage({
      role: "toolResult",
      toolCallId: "call_ok",
      toolName: "bash",
      content: [{ type: "text", text: "hi" }],
      isError: false,
      timestamp: Date.now(),
    } as never);

    assert.equal(repairDanglingToolCalls(manager), 0);
    assert.equal(messageEntries(manager).length, 2);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
