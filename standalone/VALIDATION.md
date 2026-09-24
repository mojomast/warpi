# Validation evidence (warpi)

Status legend: **PASS** (reproduced here), **FAIL**, **NOT RUN** (no runner or
not attempted). Every claim names the command or the artifact that produced it.

Status updated 2026-09-23: the tree gained the S1/S2/S4 turn-lifecycle suites,
the FIFO queueing tests, the `bash_output` tests, and the paused-turn/rejection/
reopen/reattach/compaction/transcript-repair suites after the last recorded full
run. Counts below are a source inventory at HEAD `9cd8443`; the recorded logs
predate them, so those suites are marked NOT RE-RUN rather than PASS.

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
| Rust backend (last recorded full run) | `cargo test -p standalone_agent` | **PASS** — 14 unit + 1 session-isolation + 9 vertical slice (`evidence/rust-tests.log`, recorded before the S1/S2/S4 additions) |
| Rust backend (current source inventory) | `cargo test -p standalone_agent` | **NOT RE-RUN** — 105 test functions at `9cd8443`: 26 `usage_ledger`, 23 `bridge`, 11 each vertical slice and `warp_events`, 8 `protocol`, 7 `provider`, 3 untranslatable calls, 2 each `tool_result_loss`, `session_reopen`, `duplicate_and_reattach`, `secrets`, 1 each `user_rejection`, `transcript_repair`, `session_isolation`, `real_provider`, `paused_turn_cancel`, `compaction_watchdog`, `cancel_settlement`, `helper` |
| Pi helper (last recorded full run) | `cd standalone/pi-helper && npm test` | **PASS** — 13 tests (`evidence/helper-tests.log`) |
| Pi helper (current source inventory) | `cd standalone/pi-helper && npm test` | **NOT RE-RUN** — 37 tests at `67bcb84`: 14 subagents, 6 broker, 4 protocol, 4 runtime smoke, 4 usage telemetry, 3 transcript repair, 2 usage live |
| App compile | `cargo check -p warp --bin warpi --features gui` | **PASS** at the recorded revision; not re-run here |
| App test targets | `cargo check -p warp --tests --features gui` | **PASS** at the recorded revision; not re-run here |
| Native build | `cargo build -p warp --bin warpi --features gui` | **PASS** — 4m16s, 1.0 GB debug binary (`evidence/gui-build.log`) |

Highlights covered by the Rust suites: the M1 round trip against a loopback
fixture with the real helper; `auth=none` wire assertions; two-session
isolation; cancellation, including a helper that never acknowledges `turn.cancel`
(cancel settlement) and a turn paused on an approval card (`paused_turn_cancel`);
FIFO queueing and cancelling a queued prompt without touching the running turn;
user rejection answered as `Rejected` while the call is still pending
(`user_rejection`);
provider-failure classification; foreign/duplicate/stale tool results; a dropped
result (synthesized in the same `turn.resume`); an untranslatable tool call
answered in place; a duplicate-only retry re-attaching to a live continuation
(`duplicate_and_reattach`); session reopen on a changed fingerprint
(`session_reopen`); transcript repair for dangling tool calls
(`transcript_repair`); the compaction watchdog (`compaction_watchdog`); new
conversations with no task context; and the `CreateTask` gating fix (a
server-backed task is never upgraded twice). The usage-ledger suite is pure Rust
and does not need a helper.

**Real provider (adapter level, PASS)**: `cargo test -p standalone_agent
--test real_provider` runs the full brokered loop against a live endpoint when
`WARPI_REAL_PROVIDER_KEY` is set. Reproduced against DeepSeek with both
`deepseek-flash` and `deepseek-v4-pro`:

```
real provider (deepseek-flash) final answer: RESULT=warpi-real-provider-marker
test result: ok. 1 passed
```

Helper suites cover: protocol validation (version, size, order, UTF-8-safe
truncation), broker semantics (batching, duplicates, oversized arguments,
aborts), compaction settings, usage telemetry, transcript repair for dangling
tool calls, the opt-in `task` subagent prototype, and four end-to-end provider
scenarios including `auth=none` and fragmented tool-call arguments.

## Native GUI verification (PASS)

Everything below was driven in the real `warpi` window under Xvfb + Mesa
lavapipe with the deterministic loopback fixture. No Warp account existed, and
no Warp server received agent traffic.

| Step | Result | Artifact |
| --- | --- | --- |
| Native window, onboarding, terminal | **PASS** | `evidence/gui-native-window.png` |
| Fresh agent conversation starts with the local model selected (`GUI Fixture (local endpoint · fixture-model)` in the model chip) | **PASS** | `evidence/gui-round-trip-complete.png` |
| Native model selector lists every enabled model of every profile (`DeepSeek · deepseek-flash`, `DeepSeek · deepseek-v4-pro`, `Local fixture`) and selecting one switches the active profile (terminal output "The picker switched the active profile.") | **PASS** | `evidence/gui-model-picker-all-providers.png` (predates the cloud-entry hiding: the `auto` entry in the screenshot no longer appears while standalone mode is on) |
| Per-model enable/disable removes a model from the picker (only `deepseek-flash` offered after disabling the other) | **PASS** | `evidence/gui-model-toggle.png` |
| Prompt → local provider receives the brokered tool schema and returns a `bash` tool call | **PASS** | fixture captures: request 1 = `[system, user]`, tool schema = our seven tools |
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

## Runtime preflight hardening (2026-09-24)

Added after a Windows 0.1.0 report of `Assertion failed: ncrypto::CSPRNG(nullptr, 0)`
during Node's own startup (see `WINDOWS.md` section 8). The bridge now probes the
configured helper runtime before spawning the helper and reports an actionable
`BridgeError::Runtime` (not found / version `< 22.19.0` / failed to start with
stderr) instead of a raw crash tail. The helper environment also sets
`TEMP`/`TMP` to the private scratch dir and passes through non-secret Windows OS
variables.

| Check | Command | Result |
| --- | --- | --- |
| Preflight unit tests (8 new, incl. crash-stderr surfacing) | `cargo test -p standalone_agent --lib helper::` | **PASS** — 9 passed (2026-09-24) |
| Bridge unit tests after wiring the probe | `cargo test -p standalone_agent --lib bridge::` | **PASS** — 24 passed (2026-09-24) |
| Full Rust backend suite after wiring the probe | `cargo test -p standalone_agent --no-fail-fast` | **PASS** (2026-09-24) |
| App compile | `cargo check -p warp --bin warpi --features gui` | **PASS** (2026-09-24) |

**Not verified:** the probe has not been exercised against a Windows Node whose
startup aborts; the new Windows environment variables and the user-visible
message string were not observed on Windows. Unit coverage uses shell-script
stand-ins for the runtime.

## Not run

| Item | Status | Notes |
| --- | --- | --- |
| Diff review / file-edit approvals in the GUI | **NOT RUN** | The `edit`/`write` paths are wired to `ApplyFileDiffs` and unit-tested at the event level; no GUI run yet. |
| Conversation restart / idle restore in the GUI | **NOT RUN** | Durable Pi session mapping is implemented and exercised by the app code path; not yet driven through a restart in the GUI. |
| Real (hosted) provider driven through the GUI | **NOT RUN** | The adapter-level round trip passes against DeepSeek (see above); no hosted provider has been driven through the GUI. No coding-performance claims are made. |
| Queueing / send-now (`Ctrl+Alt+Shift+Enter`) in the GUI | **NOT RUN** | FIFO queueing is covered by Rust tests (app `send_queued_prompt_now_action_fires_the_head_row`, bridge queue tests, vertical-slice FIFO ordering); the hint, panel header, and keybinding have not been driven in the GUI. |
| Approval rejection → `Rejected` in the GUI | **NOT RUN** | The bridge behavior (answer a still-pending denied call with `Rejected`) is covered by `crates/standalone_agent/tests/user_rejection.rs`; the app now sends a converted `cancelled` error result for a rejected shell call (`9cd8443`), so the typed status is not guaranteed end-to-end. Neither path is driven in the GUI, and the diff-review Reject path is not wired at all (see risk 5). |
| Long-running command snapshot + `bash_output` in the GUI | **NOT RUN** | Snapshot-to-error rendering and the `bash_output` clamp are unit-tested in Rust and the helper; no GUI run yet. |
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
   and the full inference round trip are verified natively. Diff review, restart,
   cancellation, queueing/send-now, and `bash_output` are not yet driven in the
   GUI.
2. **Real-provider coverage is adapter-level only.** DeepSeek `deepseek-flash`
   and `deepseek-v4-pro` pass the brokered round trip through
   `crates/standalone_agent/tests/real_provider.rs`; the same flow has not been
   driven through the GUI, the Kimi preset is a configuration preset only (not a
   compatibility claim), and every other provider is unverified. The
   compatibility tests themselves use the deterministic loopback fixture.
3. **Unknown-outcome reconciliation is surfaced, not automated.** A crash after
   an effect but before its result leaves an unknown state; no automatic retry.
4. **Helper process control is single-process.** No POSIX process group or job
   object; the helper spawns no children in v1.
5. **Known turn/queue caveats (updated 2026-09-23).** Stop / send-now now cancel
   a turn paused on an approval card (`bda8df4`), turn-death events withdraw its
   approval cards (`251da37`), a profile/model/key/cwd change reopens the session
   on the next fresh prompt (`251da37`), and the session-map write is locked and
   atomic (`251da37`). What is still open: a rejected shell call now returns a
   converted `CommandFinished{exit_code:-1}` error result (`9cd8443`), but the
   bridge's typed `Rejected` path is not guaranteed end-to-end because the
   app-sent result takes precedence, delivery waits for the next request, and the
   denial-vs-follow-up ordering is untested; the diff-review Reject path is not
   wired to the deny registry; malformed-frame scoping has no dedicated test; a
   compaction longer than 600 s still cancels the turn; and helper shutdown on
   app teardown is best-effort (`try_lock`, and a session whose lock is held is
   skipped).
6. **Packaging (updated 2026-09-23).** `script/linux/*` and `script/windows/*`
   no longer carry the stale `oss` channel case: the `warp-oss`/`WarpOss`
   mappings were replaced with the `warpi` channel (binary `warpi`/`warpi-tui`,
   app name `Warpi`, bundle id `dev.warpi.Warpi`), and the Inno Setup
   channel/mutex/CLI-script naming now matches `Channel::Warpi`. The shipping
   Linux artifact is the portable `packaging/package-warpi.sh` directory,
   archived to `warpi-linux-x86_64.tar.gz`, with the no-sudo `packaging/install.sh`;
   this bundle was built and installed and its binary run on the Linux audit
   machine (`warpi --version`). It is **not signed** — the workflow wires
   GitHub build-provenance attestations (`actions/attest-build-provenance`) for
   the uploaded artifacts, which is provenance, not Authenticode or GPG (no
   signing certificate or key exists). The Windows `.zip` remains a manual
   staging step, and the Inno Setup installer is wired into the Windows CI job;
   its script now compiles under Inno Setup 6.7.1 (verified locally under Wine
   for the warpi/dev/stable channels) but the produced installer has not been
   run, so both Windows artifacts remain end-to-end unverified. The generic
   AppImage/.deb/.rpm bundlers are not used for warpi because they do not ship
   the Node Pi helper that warpi spawns.
7. **Compile warnings remain** (5 in the app crate at the last check: an unused
   standalone helper, a deprecated proto field, and dead-code notes). None affect
   behaviour; not re-checked for the current tree.
8. **macOS identity** was not exercised; the fork's bundle id/URL scheme
   (`dev.warpi.warpi`, `warpi`) are set in the binary but unverified in a real
   bundle.
