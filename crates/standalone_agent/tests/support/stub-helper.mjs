// Scripted stdio helper for the Rust bridge failure-path tests.
//
// The real Pi helper cannot be driven into every state the bridge must
// survive: a cancel it ignores, tool calls whose arguments the Warp executor
// cannot represent, or a suspended turn whose result never arrives. This stub
// speaks just enough of protocol v1 to reach those states deterministically.
//
// Modes (WARPI_STUB_MODE):
// - `two_calls`          two shell calls in one batch, continuing only when
//                        both receive results
// - `fixture`            same shape, driven by WARPI_STUB_FIXTURE
// - `mixed_calls`        one shell call plus one untranslatable grep call
// - `untranslatable_call` one untranslatable grep call only
// - `ignore_cancel`      starts a turn and never answers turn.cancel
// - `pending_never`      suspends on a call and never answers anything
// - `slow_resume`        pauses on two calls, then completes 1s after the
//                        first resume (a live turn with a dropped stream)
// - `duplicate_call`     re-emits a delivered call id after its resume
// - `slow_compaction`    compacts for 600ms inside a turn before completing
// - `subagents`          emits task.started/progress/completed then completes
//
// Every relevant inbound frame is appended as one JSON line to
// WARPI_STUB_CAPTURE so tests can assert what the bridge actually delivered.

import { appendFileSync, readFileSync } from "node:fs";

const mode = process.env.WARPI_STUB_MODE ?? "two_calls";
const capturePath = process.env.WARPI_STUB_CAPTURE;
const fixturePath = process.env.WARPI_STUB_FIXTURE;

const state = {
  seq: 0,
  sessionId: undefined,
  generation: 0,
  turnId: undefined,
  exchangeId: undefined,
  pending: [],
  duplicateSent: false,
};

function emit(kind, data) {
  state.seq += 1;
  process.stdout.write(
    `${JSON.stringify({
      protocol: 1,
      seq: state.seq,
      kind,
      session_id: state.sessionId,
      generation: state.generation,
      turn_id: state.turnId,
      exchange_id: state.exchangeId,
      data,
    })}\n`,
  );
}

function capture(entry) {
  if (capturePath === undefined) return;
  appendFileSync(capturePath, `${JSON.stringify(entry)}\n`);
}

function fixtureScript() {
  if (fixturePath === undefined) throw new Error("WARPI_STUB_FIXTURE is not set");
  return JSON.parse(readFileSync(fixturePath, "utf8"));
}

function scriptedCalls() {
  const shell = (id, command) => ({
    tool_call_id: id,
    name: "workspace.shell",
    arguments: { command },
  });
  if (mode === "fixture") return fixtureScript().calls;
  const badGrep = (id) => ({
    tool_call_id: id,
    name: "workspace.grep",
    // `glob` is valid for the helper's schema but has no Warp executor
    // representation, so the bridge must answer it without asking the app.
    arguments: { pattern: "needle", glob: "*.rs" },
  });
  switch (mode) {
    case "two_calls":
      return [shell("call_a", "echo a"), shell("call_b", "echo b")];
    case "mixed_calls":
      return [shell("call_good", "echo good"), badGrep("call_bad")];
    case "untranslatable_call":
      return [badGrep("call_bad")];
    case "pending_never":
      return [shell("call_hang", "echo hang")];
    case "slow_resume":
      return [shell("call_a", "echo a"), shell("call_b", "echo b")];
    case "duplicate_call":
      return [shell("call_dup", "echo dup")];
    default:
      return [];
  }
}

function complete(text) {
  emit("assistant.message", { message_id: "stub:1", text });
  emit("turn.completed", { stop_reason: "stop", usage: null });
  state.turnId = undefined;
  state.pending = [];
}

function handleTurnStart(frame) {
  state.turnId = frame.turn_id;
  state.exchangeId = frame.exchange_id;
  state.pending = [];
  state.duplicateSent = false;
  capture({
    dir: "in",
    kind: "turn.start",
    generation: frame.generation,
    prompt: frame.data?.prompt,
  });
  emit("turn.started", {});
  if (mode === "subagents") {
    const childSessionId = `${state.sessionId}:task-1`;
    emit("task.started", {
      task_id: "task-1",
      child_session_id: childSessionId,
      description: "explore auth",
      subagent_type: "explore",
      prompt_bytes: 100,
      max_turns: 5,
      deadline_ms: 60000,
      token_cap: 50000,
    });
    emit("task.progress", {
      task_id: "task-1",
      child_session_id: childSessionId,
      elapsed_ms: 1000,
      turns: 1,
      tool_calls: 0,
      tokens: { input_tokens: 10, output_tokens: 5, total_tokens: 15 },
      pending_tools: 0,
    });
    emit("task.completed", {
      task_id: "task-1",
      child_session_id: childSessionId,
      status: "ok",
      subagent_type: "explore",
      turns: 2,
      tool_calls: 3,
      usage: { input_tokens: 1000, output_tokens: 200, total_tokens: 1200 },
      wall_ms: 2000,
      summary_bytes: 512,
    });
    complete("subagent done");
    return;
  }
  if (mode === "slow_compaction") {
    emit("compaction.started", { reason: "auto" });
    setTimeout(() => {
      emit("compaction.finished", { reason: "auto", summarized: true });
      complete("compacted");
    }, 600);
    return;
  }
  const calls = scriptedCalls();
  if (calls.length === 0) return;
  emit("tool.calls", { calls });
  state.pending = calls.map((call) => call.tool_call_id);
  emit("turn.awaiting_tools", { pending: state.pending });
}

function handleTurnResume(frame) {
  const results = Array.isArray(frame.data?.results) ? frame.data.results : [];
  capture({
    dir: "in",
    kind: "turn.resume",
    exchange_id: frame.exchange_id,
    results,
  });
  // The resuming exchange owns the continuation's identity.
  state.exchangeId = frame.exchange_id;
  const answered = new Set(results.map((result) => result.tool_call_id));
  state.pending = state.pending.filter((id) => !answered.has(id));
  if (state.pending.length > 0) return;
  if (mode === "slow_resume") {
    // Completes well after the caller has dropped the first continuation
    // stream, so a second all-duplicate resume lands while the turn is live.
    setTimeout(() => complete("slow done"), 1000);
    return;
  }
  if (mode === "duplicate_call" && !state.duplicateSent) {
    state.duplicateSent = true;
    emit("tool.calls", {
      calls: [
        {
          tool_call_id: "call_dup",
          name: "workspace.shell",
          arguments: { command: "echo dup" },
        },
      ],
    });
    state.pending = ["call_dup"];
    emit("turn.awaiting_tools", { pending: state.pending });
    return;
  }
  if (mode === "untranslatable_call") {
    complete("handled in place");
  } else if (mode === "mixed_calls") {
    complete("mixed done");
  } else if (mode === "fixture") {
    complete(fixtureScript().completion ?? "fixture done");
  } else {
    complete("both done");
  }
}

function handleTurnCancel(frame) {
  capture({
    dir: "in",
    kind: "turn.cancel",
    exchange_id: frame.exchange_id,
    turn_id: frame.turn_id,
  });
  if (mode === "ignore_cancel" || mode === "pending_never") return;
  emit("turn.cancelled", { reason: "cancelled by client" });
  state.turnId = undefined;
  state.pending = [];
}

function handleFrame(frame) {
  if (frame.session_id !== undefined && frame.session_id !== null) {
    state.sessionId = frame.session_id;
  }
  if (typeof frame.generation === "number") state.generation = frame.generation;
  switch (frame.kind) {
    case "hello":
      emit("hello.ok", {
        helper_version: "stub",
        node_version: process.version,
        capabilities: {
          protocol: 1,
          brokered_tools: ["bash", "bash_output", "edit", "glob", "grep", "read", "write"],
          compaction: true,
          cancellation: true,
          context_files: true,
          subagents: true,
        },
      });
      return;
    case "session.open":
      capture({
        dir: "in",
        kind: "session.open",
        generation: frame.generation,
        provider: frame.data?.provider?.provider_id,
        model: frame.data?.provider?.model_id,
        base_url: frame.data?.provider?.base_url,
        working_dir: frame.data?.working_dir,
        subagents: frame.data?.subagents ?? null,
      });
      emit("session.opened", {
        resumed: false,
        session_file: null,
        session_id: frame.session_id,
        active_tools: ["bash", "bash_output", "edit", "glob", "grep", "read", "write"],
        model_id: frame.data?.provider?.model_id ?? "stub-model",
        working_dir: frame.data?.working_dir ?? "/tmp",
      });
      return;
    case "turn.start":
      handleTurnStart(frame);
      return;
    case "turn.resume":
      handleTurnResume(frame);
      return;
    case "turn.cancel":
      handleTurnCancel(frame);
      return;
    case "shutdown":
      emit("shutdown.ack", {});
      process.exit(0);
      return;
    default:
      return;
  }
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index).trim();
    buffer = buffer.slice(index + 1);
    if (line.length === 0) continue;
    try {
      handleFrame(JSON.parse(line));
    } catch (error) {
      process.stderr.write(`stub-helper: ${String(error)}\n`);
    }
  }
});
process.stdin.on("end", () => process.exit(0));
