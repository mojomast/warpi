# Validation evidence (warpi)

Status legend: **PASS** (reproduced here), **FAIL**, **NOT RUN** (no runner or
not attempted). Every claim names the command or the artifact that produced it.

## Source pins

| Item | Revision |
| --- | --- |
| Warp baseline | `71088ba18d27114ccfb358901c66b54220cf30d0` |
| warp-proto-apis | `f5c1878026bc11f429260c5e798e7bab6ed4e118` (unchanged pin) |
| Donor (OpenWarp adapter) | `5045d30a98de5432cedfd256e15e56675696d0b7`, MIT — `DONOR-ATTRIBUTION.md` |
| Pi SDK | `@earendil-works/pi-coding-agent@0.84.2`, `typebox@1.3.7` |
| Fork branch | `standalone-pi-backend` on top of the pinned baseline; nothing pushed |

## Environment

Linux x86_64, 4 cores, 15 GiB RAM, no sudo, Rust 1.92.0, Node v22.19.0.
Development-only shims (no apt access): `protoc 25.1`, `cmake 3.31.6`,
an `alsa.pc` stub, `RUST_FONTCONFIG_DLOPEN=1`, `CARGO_INCREMENTAL=0` — see
`dev-env.sh`; none of them ship with the product.

## Automated tests

| Suite | Command | Result |
| --- | --- | --- |
| Rust backend | `cargo test -p standalone_agent` | **PASS** — 24 tests: 14 unit + 1 session-isolation + 9 vertical slice (`evidence/rust-tests.log`) |
| Pi helper | `cd standalone/pi-helper && npm test` | **PASS** — 13 tests (`evidence/helper-tests.log`) |
| App compile | `cargo check -p warp --bin warpi --features gui` | **PASS** |
| App test targets | `cargo check -p warp --tests --features gui` | **PASS** |
| Native build | `cargo build -p warp --bin warpi --features gui` | **PASS** — 4m16s, 1.0 GB debug binary (`evidence/gui-build.log`) |

Highlights covered by the Rust suites: the M1 round trip against a loopback
fixture with the real helper; `auth=none` wire assertions; two-session
isolation; cancellation; provider-failure classification; foreign/duplicate/
stale tool results; new conversations with no task context; and the
`CreateTask` gating fix (a server-backed task is never upgraded twice).

Helper suites cover: protocol validation (version, size, order, UTF-8-safe
truncation), broker semantics (batching, duplicates, oversized arguments,
aborts), compaction settings, and four end-to-end provider scenarios including
`auth=none` and fragmented tool-call arguments.

## Native GUI verification (PASS)

Everything below was driven in the real `warpi` window under Xvfb + Mesa
lavapipe with the deterministic loopback fixture. No Warp account existed, and
no Warp server received agent traffic.

| Step | Result | Artifact |
| --- | --- | --- |
| Native window, onboarding, terminal | **PASS** | `evidence/gui-native-window.png` |
| Fresh agent conversation starts with the local model selected (`GUI Fixture (local endpoint · fixture-model)` in the model chip) | **PASS** | `evidence/gui-round-trip-complete.png` |
| Prompt → local provider receives the brokered tool schema and returns a `bash` tool call | **PASS** | fixture captures: request 1 = `[system, user]`, tool schema = our six tools |
| **Native tool approval card** ("OK if I run this command and read the output?", Reject/Edit/Run) | **PASS** | `evidence/gui-tool-approval.png` |
| Click **Run** → the command executes in a normal Warp block | **PASS** | `evidence/gui-round-trip-complete.png` |
| Result returns to Pi → **second** provider request → final assistant text "Warpi round trip complete." | **PASS** | fixture captures: request 2 = `[system, user, assistant, tool]`; screenshot above |
| Provider settings page (Settings → Agents → **Local Pi provider**) | **PASS** | `evidence/gui-provider-settings.png` |
| **Test connection** action → `GET /v1/models` → "Endpoint reachable (HTTP 200 OK); 1 model(s) reported." | **PASS** | `evidence/gui-test-connection.png` |
| Fork-specific identity: separate data dirs, own window class, onboarding state, `warpi.log` | **PASS** | app log line `channel: Warpi … AppId { dev.warpi.Warpi }` |

Three real integration bugs were found by running the GUI and fixed with
regression tests:

1. a hand-written config with a partial `compat` object was rejected
   (`supports_developer_role` had no serde default);
2. a brand-new conversation sends **no task context** (the real server
   generates the task id) — the adapter now does the same;
3. `CreateTask` was re-sent for a server-backed task after a restart, failing
   with `UnexpectedUpgrade` — the adapter now sends it only for new
   conversations.

## Not run

| Item | Status | Notes |
| --- | --- | --- |
| Diff review / file-edit approvals in the GUI | **NOT RUN** | The `edit`/`write` paths are wired to `ApplyFileDiffs` and unit-tested at the event level; no GUI run yet. |
| Conversation restart / idle restore in the GUI | **NOT RUN** | Durable Pi session mapping is implemented and exercised by the app code path; not yet driven through a restart in the GUI. |
| Real (non-fixture) provider | **NOT RUN** | No authorized credentials available. No compatibility or coding-performance claims are made. |
| Windows / macOS native build and GUI | **NOT RUN** | No runners. WSL is not a native Windows test. |
| Full egress audit of the GUI process | **NOT RUN** | Source-level inventory in `NETWORK_DEPENDENCIES.md`; the GUI run did confirm that agent traffic went to the loopback fixture only. |
| `warp_core` path tests on a machine without XDG overrides | **NOT RUN** | 5 of the 47 tests assert home-relative defaults and fail under this harness's `XDG_*` overrides; they pass with those unset. |

## Reproduced sequences (abridged)

M1 slice (automated):

```
exchange 1: init, create_task, add_messages(tool_call), finished
  tool call: call_abc RunShellCommand{command="echo hello", risk_category=NONTRIVIAL_LOCAL_CHANGE}
exchange 2: init, add_messages("all"), append_text(" done"), finished
provider request 2 messages: [system, user, assistant(tool_calls), tool(call_abc, "hello\n")]
```

GUI slice (manual, same shape):

```
prompt  : "Run echo hello-from-warpi in the shell"
tool    : native approval card for `echo hello-from-warpi`
approve : Run
result  : command block with output; provider request 2 = [system, user, assistant, tool]
final   : "Warpi round trip complete."
```

## Honest risk register

1. **GUI coverage is partial.** Tools, approvals, model chip, provider settings,
   and the full inference round trip are verified natively. Diff review,
   restart, and cancellation are not yet driven in the GUI.
2. **No real provider run.** Provider compatibility flags exist and are tested
   against the fixture only.
3. **Unknown-outcome reconciliation is surfaced, not automated.** A crash after
   an effect but before its result leaves an unknown state; no automatic retry.
4. **Helper process control is single-process.** No POSIX process group or job
   object; the helper spawns no children in v1.
5. **Packaging is not updated for the rename.** `script/linux/*` and
   `script/windows/*` bundlers still contain the old `oss` channel case; the
   development entrypoints (`script/run`, `script/run-tui`) and the CI check
   job were updated. Release bundling is M5 work.
6. **Compile warnings remain** (5 in the app crate: an unused standalone
   helper, a deprecated proto field, and dead-code notes). None affect
   behaviour.
7. **macOS identity** was not exercised; the fork's bundle id/URL scheme
   (`dev.warpi.warpi`, `warpi`) are set in the binary but unverified in a real
   bundle.
