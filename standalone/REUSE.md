# Reuse, attribution, and source pins (warpi)

> Rename note: this fork is **warpi**. The channel/app identity is
> `dev.warpi.warpi`, the binary is `warpi`, its config/data directories are
> `warpi`/`.warpi`, its URL scheme is `warpi`, and its environment overrides are
> `WARPI_*`. Stock Warp identities (`warp`, `warp-oss`, `.warp`, `warposs`)
> are untouched, so the two can coexist on one machine.

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
- License of the donor adapter: MIT (see `standalone/DONOR-ATTRIBUTION.md`; the donor's
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
| Canonical workspace calls (`workspace.shell`, `workspace.read_file`, ...) | `standalone/pi-helper/src/workspace-tools.ts` | Kept as the stable seam; tool set is the seven v1 tools (`bash`, `bash_output`, `read`, `write`, `edit`, `glob`, `grep`). |
| First-chunk/append-text client actions with the `agent_output.text` field mask | `crates/standalone_agent/src/warp_events.rs` | Reimplemented in Rust against `warp_multi_agent_api`; shapes match the donor's Go (`sendFirstTextChunk` / `sendAppendText`). |
| `CreateTask` before output for tasks the client has not upgraded | `crates/standalone_agent/src/warp_events.rs` | Same semantics, with the additional `server_data`/`server_message_data` heuristic. |
| Pending-tool barrier: synthesize errors for unresolved tools, drop late duplicates | `crates/standalone_agent/src/bridge.rs` | Reworked for exactly-one-result semantics: pending calls the app does not answer are closed with a synthesized cancelled result in the same `turn.resume`, duplicates are ignored, and a batch with no matching result fails without consuming state (see `SECURITY.md`, `ARCHITECTURE.md`). |
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
3. Tool-result reconciliation is stricter: duplicates are ignored, foreign/stale
   results never consume pending state, and a call the app drops is closed with a
   synthesized cancelled error rather than a fabricated success. The donor
   synthesized generic error results and continued; Warp already records action
   outcomes, and inventing a success would hide lost side effects.
4. Background shell management is partial: `bash_output`
   (`workspace.read_shell_command_output`) polls a command that is already
   running with a bounded wait, and `bash` can start a command without waiting
   (`run_in_background`), but `bash_write`/`bash_cancel` are deferred. There is
   no way for the model to write to or stop a running command.
5. No WebSocket/HTTP listener anywhere in the helper.

## Files added by this fork

```
crates/standalone_agent/            Rust backend (new crate)
standalone/pi-helper/               Private Node helper embedding the Pi SDK
app/src/ai/standalone/mod.rs        Standalone wiring + local config
app/src/settings_view/local_provider_page.rs  Local Pi provider settings page
app/src/ai/llms.rs                  Standalone entries in the model picker
app/src/ai/blocklist/queued_query.rs  Standalone defaults to the prompt queue
app/src/terminal/input.rs           Send-queued-prompt-now keybinding + hints
app/src/terminal/view/queued_prompts_panel.rs  Queued-prompt panel header hint
app/src/bin/warpi.rs                warpi GUI entry point
standalone/**                       Docs, helper, evidence, dev shims
packaging/package-warpi.sh          No-sudo development bundle
.github/workflows/warpi-build.yml   Linux/Windows CI job (not run yet)
```

Several entries in this list are edits to upstream files rather than new files;
the two sections below separate the two.

## Upstream files changed (summary; 2026-09-23)

120 files differ from the pinned baseline. The authoritative list is
`git diff --name-only 71088ba18d27114ccfb358901c66b54220cf30d0..HEAD`; no
upstream file was deleted. The main groups:

- **Standalone wiring**: `app/src/ai/{mod,llms}.rs`,
  `app/src/ai/agent/api.rs`, `app/src/ai/agent/api/impl.rs`,
  `app/src/ai/blocklist/queued_query.rs`, `app/src/ai/standalone/mod.rs`,
  `app/src/settings/ai.rs`, `app/src/settings_view/*.rs`,
  `app/src/terminal/{input,input_tests}.rs`,
  `app/src/terminal/view/queued_prompts_panel.rs`,
  `app/src/server/telemetry/events.rs`.
- **Fork identity/channel**: `app/src/bin/warpi.rs`,
  `crates/warp_core/src/{channel,paths}.rs`, `crates/warp_tui/**`,
  `app/src/autoupdate/*`, `app/src/crash_reporting/mod.rs`,
  `crates/http_server/src/lib.rs`.
- **Secure storage**: `crates/warpui_extras/src/secure_storage/linux_tests.rs`,
  `crates/warpui_extras/src/secure_storage/windows.rs`.
- **Packaging/CI/licensing**: `.github/workflows/warpi-build.yml`,
  `packaging/**`, `LICENSE-NOTES.md`, `Cargo.lock`, `app/Cargo.toml`,
  `.gitignore`.

Changes are meant to stay guarded by `#[cfg(not(target_family = "wasm"))]`, the
standalone config check, or the fork channel (`Channel::Warpi`).
