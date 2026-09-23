# Reuse, attribution, and source pins

This fork is built from pinned sources. Nothing here is merged from an
unrelated fork wholesale; the items below are the complete set of reused
artifacts, their exact commits, and what was changed.

## Pinned baseline

| Component | Source | Exact revision | Notes |
| --- | --- | --- | --- |
| Warp client | https://github.com/warpdotdev/warp | `71088ba18d27114ccfb358901c66b54220cf30d0` (2026-09-22) | Shallow checkout; baseline for the fork. AGPL-3.0-only, except `warpui*` (MIT). |
| Warp multi-agent protobuf | https://github.com/warpdotdev/warp-proto-apis | `f5c1878026bc11f429260c5e798e7bab6ed4e118` | Already pinned in `Cargo.toml:351` and `Cargo.lock`; unchanged. |
| Pi coding-agent SDK | npm `@earendil-works/pi-coding-agent` | `0.84.2` (exact) | `standalone/pi-helper/package.json` + lockfile; Node >= 22.19. |
| TypeBox | npm `typebox` | `1.3.7` (exact) | Tool parameter schemas, matches the SDK's own version. |

Toolchain: `rust-toolchain.toml` pins Rust 1.92.0 (unchanged).

## Donor logic ported (with attribution)

Donor: **sasuke39/openwarp**, a Go+TypeScript adapter that bridges Warp's
multi-agent protobuf to a Pi helper over NDJSON.

- Repo: https://github.com/sasuke39/openwarp
- Commit: `5045d30a98de5432cedfd256e15e56675696d0b7` (2026-09-22)
- License of the donor adapter: MIT (see
  `donors/openwarp-ATTRIBUTION.md` in the working tree of the audit; the donor's
  separate `openwarp-client` repository is AGPL-3.0 and was **not** used).
- Donor files inspected: `integrations/pi-agent/src/{runtime,workspace-tools,
  protocol,main}.ts`, `integrations/pi-agent/test/runtime-smoke.test.ts`,
  `cmd/server/{external_runtime,external_pending}.go`,
  `internal/agentruntime/*`, `internal/workspacetools/translate.go`.

What was **ported** (rewritten in Rust/TypeScript for this fork, not copied):

| Donor behaviour | Where it lives now | Change |
| --- | --- | --- |
| NDJSON envelope over stdio with per-exchange correlation | `crates/standalone_agent/src/protocol.rs`, `standalone/pi-helper/src/protocol.ts` | Rewritten with a version handshake, strict sequence validation, per-frame identity (session/turn/exchange/generation), bounded frames, and typed events. |
| Tool brokering: custom tool suspends on a promise; batch is emitted; reply resumes the same prompt | `standalone/pi-helper/src/workspace-tools.ts`, `runtime.ts` | Rewritten; uses the SDK's own `toolCallId` for correlation, adds duplicate/id-length rejection, abort handling, and per-owner batching. |
| Canonical workspace calls (`workspace.shell`, `workspace.read_file`, ...) | `standalone/pi-helper/src/workspace-tools.ts` | Kept as the stable seam; tool set reduced to six v1 tools. |
| First-chunk/append-text client actions with the `agent_output.text` field mask | `crates/standalone_agent/src/warp_events.rs` | Reimplemented in Rust against `warp_multi_agent_api`; shapes match the donor's Go (`sendFirstTextChunk` / `sendAppendText`). |
| `CreateTask` before output for tasks the client has not upgraded | `crates/standalone_agent/src/warp_events.rs` | Same semantics, with the additional `server_data`/`server_message_data` heuristic. |
| Pending-tool barrier: synthesize errors for unresolved tools, drop late duplicates | `crates/standalone_agent/src/bridge.rs` | Split across the bridge and the helper; unknown/duplicate results become protocol errors instead of being silently synthesized (see `SECURITY.md`). |
| Cancellation handshake (`turn.cancel` → `turn.cancelling` → `turn.cancelled`) | both sides | Kept; terminal event emitted once, after the Pi turn actually settles. |
| Compaction settings derivation | `standalone/pi-helper/src/runtime.ts` | Same formula, unit-tested. |
| Fake OpenAI-compatible fixture (streamed text, fragmented tool arguments, scripted errors) | `standalone/pi-helper/test/fake-provider.ts` | Rewritten in TypeScript, reused by both helper and Rust tests. |

What was **not** ported: the intermediate Go HTTP server, Warp protobuf
encoding in Go, `TaskContext` parsing, todo lists, steer registry, managed
background-job shell wrappers, and the SSH-specific tool variants. The Rust
bridge consumes `warp_multi_agent_api` directly.

## Deliberate deviations from the donor

1. One helper process per application, one session per conversation, owned by
   the Rust bridge (the donor spawned a process per HTTP request).
2. Tool call ids come from the Pi SDK rather than being generated in the
   broker, so the transcript and the Warp tool card share one id.
3. Unknown/duplicate/stale tool results are surfaced as protocol errors; the
   donor synthesized error results and continued. Warp already records action
   outcomes, and inventing results would hide lost side effects.
4. Background shell management (`bash_output`/`bash_write`/`bash_cancel`) is
   deferred; v1 only runs foreground commands (with an explicit
   `run_in_background` reject-path documented in `PROVIDER_COMPATIBILITY.md`).
5. No WebSocket/HTTP listener anywhere in the helper.

## Files added by this fork

```
crates/standalone_agent/            Rust backend (new crate)
app/src/ai/standalone/mod.rs        Standalone wiring + local config
app/Cargo.toml                      + standalone_agent dependency
app/src/ai/mod.rs                   + module registration
app/src/ai/agent/api.rs             + RequestParams.standalone
app/src/ai/agent/api/impl.rs        + local-backend branch
app/src/settings/ai.rs              + standalone exception for agent mode
standalone/pi-helper/               Private Node helper embedding the Pi SDK
standalone/*.md                     These documents
standalone/dev-env.sh               Local dev environment for this audit machine
```

## Upstream files changed (complete list)

`app/Cargo.toml`, `app/src/ai/mod.rs`, `app/src/ai/agent/api.rs`,
`app/src/ai/agent/api/impl.rs`, `app/src/settings/ai.rs`. No upstream file was
deleted or rewritten; every change is additive and guarded by
`#[cfg(not(target_family = "wasm"))]` and/or the standalone config check.
