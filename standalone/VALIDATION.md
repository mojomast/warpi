# Validation evidence

Status legend: **PASS** (reproduced here), **FAIL**, **NOT RUN** (no runner or
not attempted). Every claim below names the command that produced it.

## Source pins

| Item | Revision |
| --- | --- |
| Warp baseline | `71088ba18d27114ccfb358901c66b54220cf30d0` |
| warp-proto-apis | `f5c1878026bc11f429260c5e798e7bab6ed4e118` (unchanged pin) |
| Donor (OpenWarp adapter) | `5045d30a98de5432cedfd256e15e56675696d0b7` |
| Pi SDK | `@earendil-works/pi-coding-agent@0.84.2`, `typebox@1.3.7` |

## Environment

- Linux x86_64, 4 cores, 15 GiB RAM, no sudo.
- Rust 1.92.0 (pinned by `rust-toolchain.toml`), Node v22.19.0.
- Development-only shims used because the machine has no apt access:
  `protoc 25.1`, `cmake 3.31.6` (venv wheel), and an `alsa.pc` stub pointing at
  the system `libasound.so.2`. See `standalone/dev-env.sh`; these are **not**
  part of the product.

## Automated tests

### Helper (`standalone/pi-helper`)

Command:

```bash
cd standalone/pi-helper
npx tsc -p tsconfig.json          # typecheck + build (PASS, no diagnostics)
node --import tsx --test test/*.test.ts
```

Result: **PASS** — 13 tests, 0 failures.

| Test | What it proves |
| --- | --- |
| `runtime-smoke: round trip (api key)` | Model → `bash` tool call (fragmented JSON) → helper suspends → Rust-style resume → second provider request → final text. Asserts exactly two provider requests, one user message, one tool result, `Authorization: Bearer sk-fixture`. |
| `runtime-smoke: round trip (auth none)` | Same flow with `auth=none`, asserting **no** `Authorization` header and no placeholder on the wire. |
| `runtime-smoke: cancel` | Cancelling while awaiting a tool settles the turn; a later turn in the same session still works; a rejected tool result surfaces as a failed tool call. |
| `runtime-smoke: provider error` | A 401 becomes one terminal `turn.failed` with retry disabled, exactly one provider request. |
| `protocol` | Version mismatch, non-JSON, bad types, oversized frames, UTF-8-safe truncation. |
| `workspace-tools` | Canonical tool mapping, per-owner batching, duplicate ids, oversized arguments, foreign delivery, abort handling. |
| `compaction-settings` | Reserve/keep-recent derivations scale with the context window. |

### Rust backend (`crates/standalone_agent`)

Command: `cargo test -p standalone_agent`

Result: **PASS** — 20 tests, 0 failures (13 unit + 7 integration).

| Test | What it proves |
| --- | --- |
| `native_prompt_tool_result_second_request_and_final_text` | M1 vertical slice with the real helper and a real HTTP fixture: exchange 1 = `init, create_task, add_messages(tool call), finished` with a `RunShellCommand` classified `NONTRIVIAL_LOCAL_CHANGE`; exchange 2 = resumed run, streamed deltas as `add_messages` + `append_text`, `finished`; provider request 2 contains the tool result once and exactly one user message. |
| `auth_none_never_sends_an_authorization_header` | Header audit on the fixture's captured request. |
| `foreign_and_duplicate_tool_results_are_rejected_without_corrupting_the_turn` | Foreign result ⇒ `unknown_tool_result`; genuine result still resumes; a settled run refuses later results. |
| `cancellation_settles_the_run_and_keeps_the_session_usable` | Cancel while awaiting tools, then a second turn plus a rejected tool result in the same session. |
| `provider_failures_surface_as_a_single_terminal_failure` | One terminal failure, one provider request (no retry storm). |
| `request_extraction_reads_the_native_request_shape`, `tool_result_rendering_maps_shell_results_for_the_model` | Warp request/result protobuf handling, including denial rendering. |
| `protocol`, `provider`, `secrets`, `helper` unit tests | Frame validation, URL normalization (no duplicated `/v1` or `/chat/completions`), config-key rejection, secret redaction, environment sanitization. |

### Warp app compilation

Command (requires the dev shims):

```bash
source standalone/dev-env.sh
cargo check -p warp --bin warp-oss --features gui
```

Result: **PASS** — the standalone branch, `RequestParams` change, module
registration, and the `AISettings` exception compile in the real app crate.

## Manual / native verification

| Item | Platform | Status | Notes |
| --- | --- | --- | --- |
| GUI build of `warp-oss` | Linux x86_64 | see below | `cargo build -p warp --bin warp-oss --features gui` |
| GUI launch under Xvfb + software Vulkan | Linux x86_64 | see below | `xvfb-run` + Mesa lavapipe |
| Real native GUI tool/approval/diff/history verification | Linux | **NOT RUN** | No display session or input automation runner available; see risks. |
| Native Windows / macOS build and run | Windows, macOS | **NOT RUN** | No runner. WSL is not a native Windows GUI test and was not attempted. |
| Real provider (non-fixture) inference | any | **NOT RUN** | No authorized credentials were available; all provider tests use the deterministic loopback fixture. No compatibility or coding-performance claims are made. |
| Egress audit of the running GUI | Linux | **NOT RUN** | Source-level inventory only (`NETWORK_DEPENDENCIES.md`). |

## Reproduced M1 sequence (abridged)

From `native_prompt_tool_result_second_request_and_final_text`:

```
exchange 1 events: init, create_task, add_messages(tool_call), finished
  tool call: tool_call_id=call_abc, RunShellCommand{command="echo hello",
             risk_category=NONTRIVIAL_LOCAL_CHANGE, is_read_only=false}
exchange 2 events: init, add_messages(text "all"), append_text(" done"), finished
provider requests: 2
  request 2 messages: [system, user, assistant(tool_calls), tool(call_abc, "hello\n")]
  request 2 headers: authorization: Bearer sk-fixture
```

## Honest risk register

1. **No GUI verification.** The end-to-end proof is at the Warp event boundary
   (`ResponseEvent`s) with the real helper, bridge, and provider wire path. The
   native rendering/approval path is unchanged upstream code, but it has not
   been exercised in a window on this machine.
2. **No real provider run.** Protocol compatibility flags exist and are tested
   against the fixture only.
3. **Unknown-outcome reconciliation is surfaced, not automated.** A crash after
   an effect but before its result leaves an unknown state; the UI reports the
   failure and does not retry.
4. **Helper process control is single-process.** No POSIX process group or job
   object is used; the helper itself spawns no children in v1.
5. **M2 UX gap.** Endpoint/profile configuration currently requires writing
   `standalone/config.json`; there is no settings page yet.
6. **CreateTask heuristic.** If the app restarts mid-optimistic-turn, a second
   `CreateTask` may be sent for an already server-backed task (the client logs a
   local error and continues). Documented in `ARCHITECTURE.md`.
