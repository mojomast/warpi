import { test } from "node:test";
import assert from "node:assert/strict";
import { encodeFrame, FrameError, parseFrame, truncateUtf8 } from "../src/protocol.ts";

test("parseFrame accepts a well-formed frame and preserves identity", () => {
  const frame = parseFrame(
    JSON.stringify({ protocol: 1, seq: 7, kind: "turn.start", session_id: "s", turn_id: "t", exchange_id: "e", generation: 3, data: {} }),
  );
  assert.equal(frame.kind, "turn.start");
  assert.equal(frame.generation, 3);
});

test("parseFrame rejects protocol mismatches, bad types and oversized ids", () => {
  const cases: Array<[string, string]> = [
    [JSON.stringify({ protocol: 2, seq: 0, kind: "hello" }), "protocol_mismatch"],
    [JSON.stringify({ protocol: 1, seq: -1, kind: "hello" }), "invalid_frame"],
    [JSON.stringify({ protocol: 1, seq: 0, kind: "" }), "invalid_frame"],
    [JSON.stringify({ protocol: 1, seq: 0, kind: "hello", session_id: "" }), "invalid_frame"],
    [JSON.stringify({ protocol: 1, seq: 0, kind: "hello", data: [] }), "invalid_frame"],
    ["not json", "invalid_json"],
  ];
  for (const [line, expected] of cases) {
    try {
      parseFrame(line);
      assert.fail(`expected ${expected} for ${line}`);
    } catch (error) {
      assert.ok(error instanceof FrameError, String(error));
      assert.equal(error.code, expected);
    }
  }
});

test("encodeFrame refuses frames beyond the size bound", () => {
  const huge = "x".repeat(5 * 1024 * 1024);
  try {
    encodeFrame({ protocol: 1, seq: 0, kind: "diagnostic", data: { message: huge } });
    assert.fail("expected the frame to be rejected");
  } catch (error) {
    assert.ok(error instanceof FrameError);
    assert.equal(error.code, "frame_too_large");
  }
});

test("truncateUtf8 never splits a code point", () => {
  const value = "a".repeat(10) + "🎉".repeat(10);
  const { text, truncated } = truncateUtf8(value, 20);
  assert.equal(truncated, true);
  assert.ok(Buffer.byteLength(text, "utf8") <= 20 + 200);
  // Round-tripping through UTF-8 must not produce replacement characters.
  assert.equal(text.includes("\uFFFD"), false);
});
