# Milestone plan and current status (warpi)

Branch: `standalone-pi-backend` in the fork checkout. Upstream baseline
`71088ba18d27114ccfb358901c66b54220cf30d0`.

## Status at 2026-09-23

Since the milestone summaries below were written, the tree gained:

- **Prompt queueing** (FIFO at the app and bridge layers; `SessionBusy` removed)
  and the standalone send-now keybinding `Ctrl+Alt+Shift+Enter` /
  `Cmd+Alt+Shift+Enter` (`ARCHITECTURE.md` §Prompt queueing and cancellation).
- **Turn-lifecycle guarantees** S1/S2/S4: exactly one result per forwarded tool
  call, untranslatable calls answered in place, cancel deadline plus
  `RunCancelled` → `Finished(Done)` (`ARCHITECTURE.md` §Turn lifecycle
  guarantees).
- **Tool-loop hardening**: a still-running command is an error result, and the
  seventh tool `bash_output` (`workspace.read_shell_command_output`, 1–120 s
  bounded wait, default 30 s) polls an already-running command. The model gets
  non-interactive guidance in the tool descriptions and the session system
  prompt.
- **Credentials**: Linux file fallback (AES-256-GCM, mode 0600) works for nested
  keys like `warpi/profile/<id>`; the key never enters the config file.
- **Providers**: the Kimi (Moonshot) preset, and the multi-model picker listing
  every enabled model of every configured profile.

Known caveats as of the same date: stop / send-now cannot cancel a turn paused on
an approval card (the queued prompt waits for the 30-minute pending-tool
deadline), an open conversation keeps the profile/model it opened with, and the
session map has a read-modify-write race. See `ARCHITECTURE.md` and
`VALIDATION.md` for the exact status; fixes are in progress but uncommitted.

## M0 — Pin sources, baseline, ownership, dependencies ✅

- Shallow checkout of the pinned upstream commit; `warp-proto-apis` stays at
  `f5c1878…`; Pi SDK pinned exactly at `0.84.2` (lockfile committed).
- Ownership map and seam analysis (single inference choke point
  `generate_multi_agent_output`, exchange vs run semantics) recorded in
  `ARCHITECTURE.md`; donor attribution in `REUSE.md`.
- Task branch created locally. Nothing pushed.

## M1 — Prove Warp → Pi → brokered tool → Pi continuation ✅

- Private v1 stdio protocol (`protocol.rs` / `protocol.ts`) with handshake,
  identity, sequence and size validation.
- Supervised helper: explicit executable/argv, sanitized environment, bounded
  frames, independent stderr drain, cooperative-then-forced shutdown.
- Bridge state machine with per-conversation sessions, suspended tool batches,
  stale/foreign/duplicate rejection, and cancellation.
- Warp event translation producing native `ResponseEvent`s.
- Evidence: 22 Rust tests (14 unit + 8 integration) and 13 helper tests at the
  time, including the full round trip against a loopback fixture with the real
  helper, `auth=none` wire assertions, two-session isolation, and a clean
  `cargo check`/`cargo build` of the native GUI binary. (The suites have grown
  since — seven tools, queueing, S1/S2/S4; see `VALIDATION.md`.)

## M2 — Standalone onboarding/settings, credentials, model picker ✅

Done:
- Local-only config (`<data dir>/standalone/config.json` or
  `WARPI_STANDALONE_CONFIG`) with a multi-profile registry, active-profile
  selection, and validation; the legacy single-profile form is still read.
- Native settings page (Settings → Agents → Local Pi provider) for endpoint,
  model id (plus additional model ids), limits, auth mode, credential entry (OS
  secret store), provider presets (including Kimi (Moonshot)), profile
  add/delete, per-model enable/disable, and "Test connection".
- Every enabled model of every configured profile appears in the native model
  picker and the model chip follows the selection, refreshing after a save
  without a restart. Selecting a model applies when the conversation next opens
  a helper session (known gap: an open conversation keeps its session).
- Credential resolution through `SecureStorage` into `SecretString`; `auth=none`
  path proven on the wire.
- Native control flow wiring: `RequestParams.standalone`,
  `generate_multi_agent_output` branch, `is_any_ai_enabled` exception,
  per-conversation helper sessions, durable conversation → Pi session file map.

Remaining (nice-to-have):
- First-run guidance inside the provider page (the page is fully usable today,
  but a brand-new profile starts with placeholder values).

## M3 — Core tools, approvals, native UI, durable mapping 🚧 partial

Done:
- Seven brokered tools mapped to native actions; exact active tool set asserted at
  session open; built-in Pi tools disabled.
- Approvals, diffs, and execution reuse Warp's own path unchanged: verified
  natively (prompt → tool card → Run → result → second provider request →
  final text) with screenshots in `evidence/`.
- Durable session mapping (`session-map.json`); restart resumes the exact Pi
  session file. Idle restart is exercised manually (see `VALIDATION.md`), but the
  map's write is not atomic (known race, `ARCHITECTURE.md`).

Remaining:
- MCP via Warp's existing integration (not in v1 by design).
- Reviewed `write`/`edit` diff UX: wired to `ApplyFileDiffs` and tested at the
  event level, but not yet driven in a GUI session.
- `bash_write`/`bash_cancel` (write to or stop a running command); `bash_output`
  bounded polling is done.

## M4 — Cancellation, retries, crashes, compaction, isolation, egress 🚧 partial

Done:
- Cancellation while streaming and while awaiting tools; the helper settles the
  run before accepting the next message (tested).
- Turn-lifecycle guarantees S1/S2/S4: one result per forwarded call (dropped
  results synthesized in the same `turn.resume`), untranslatable calls answered
  in place, cancel deadline and `RunCancelled` → `Finished(Done)`.
- Prompt queueing: FIFO at the app and bridge layers; a queued prompt can be
  cancelled without touching the running turn; the stall watchdog also drains the
  queue.
- Duplicate/foreign/stale result rejection; unknown outcomes surfaced.
- Two independent sessions with different profiles/directories, isolated by
  construction and verified by `two_sessions_with_different_profiles_stay_isolated`.
- Network dependency matrix (`NETWORK_DEPENDENCIES.md`).

Remaining:
- Crash-injection matrix (kill the helper before/after tool execution and
  assert no automatic re-execution).
- Compaction exercised end-to-end (settings derived and unit-tested; a
  forced-compaction integration test is missing).
- Bounded retry exhaustion test at the bridge level. (There is no retry layer
  yet: `retry.enabled: false` at session open.)
- Full egress audit of the GUI process.
- Idle eviction: a conversation's helper session stays alive until app exit;
  add an idle timeout/close policy before shipping long-running builds.
- The per-conversation session lock is held across the helper handshake/IPC for
  `session.open` and `turn.start`; move to a per-session actor before the UI
  needs to interleave requests for one conversation.
- Stop / send-now on a turn that is paused on an approval card does not reach the
  helper (the queued prompt waits for the pending-tool deadline, 30 minutes by
  default). A fix is in progress; see `ARCHITECTURE.md`.
- Reopen a conversation's helper session when the profile/model/key/cwd changes.
- Make `session-map.json` writes atomic (or lock them).
- Stopping an already-running command from the agent (as opposed to declining a
  pending one or cancelling inference) is not wired in v1; the user keeps
  Warp's own block controls.

## M5 — Package the validated target, evidence 🚧 partial

Done:
- Runnable development build (`target/debug/warpi`) verified to launch and
  render natively under Xvfb + Mesa lavapipe; evidence committed in
  `standalone/evidence/`, including the native GUI agent round trip, model
  picker, per-model toggle, and Test connection.
- `standalone/BUILDING.md` documents prerequisites, helper build, and config.
- `packaging/package-warpi.sh` assembles a no-sudo development bundle.

Remaining:
- Installer/release packaging; no signed bundle for any platform yet.
- Platform evidence is Linux x86_64 only in this environment.

## Deferred: ClikCode review follow-ups (2026-09-23)

Identified from a third-party review of the MIT-licensed
[Jgracier/ClikCode](https://github.com/Jgracier/ClikCode), recorded in
`standalone/research/clikcode-review.md`. These are **design-level borrows**:
ideas and structures can be reimplemented freely, but any copied code needs the
MIT copyright/permission notice recorded in `LICENSE-NOTES.md` (with a
`DONOR-ATTRIBUTION.md`-style entry) before it lands.

**Deferred** in favour of finishing and shipping the current tracks — UI and
observability, the durable queue/event log, subagents, and release consolidation;
revisit these afterwards. Nothing below is implemented; where part of an item
already exists, the item says so.

1. **Provider-error classification → retry (M).** A provider or transport
   failure collapses to a terminal failure today: `session.open` sends
   `retry.enabled: false`, the helper's `turn.failed` carries a `retryable` flag,
   and the bridge forwards it (`BridgeEvent::RunFailed`) but nothing acts on it —
   `warp_events` maps the code and drops the flag, and the app reports a generic
   failure. Classify by status code / declared kind only (401/403 auth, 402/429
   quota with `retryAfter`, transport timeouts), never by model-authored text,
   and retry the recoverable class with bounded backoff. Keep the retries bounded
   and integrated with the stall/cancel watchdog and the exchange lifecycle; do
   not resurrect the SDK retry wholesale. Areas:
   `crates/standalone_agent/src/bridge.rs` (classification,
   `BridgeEvent::RunFailed`), `standalone/pi-helper/src/runtime.ts`
   (`turn.failed` emission), `crates/standalone_agent/src/protocol.rs` if a
   classified kind is added to the wire, and
   `crates/standalone_agent/src/warp_events.rs` (`failure_reason`).

2. **Steering / `steer` delivery mode (M).** Queue-by-default and
   cancel-and-send exist (`ARCHITECTURE.md` §Prompt queueing and cancellation),
   but a running turn cannot be steered. Add a steering path that appends a
   mid-turn prompt before Pi's next model step; the delivery-mode shape is
   already designed in `standalone/research/event-log-delivery-spec.md`. Areas:
   `standalone/pi-helper/src/runtime.ts`, `standalone/pi-helper/src/protocol.ts`,
   `crates/standalone_agent/src/bridge.rs`, `app/src/ai/standalone/mod.rs`.
   Risk: depends on whether the Pi SDK accepts a user message mid-prompt;
   interacts with the parked-turn cancel caveat.

3. **Two-stage compaction + authoritative token estimate (M–L).** The bridge now
   derives compaction settings (`derive_compaction_settings`), forwards
   compaction events with a context reading, and the usage ledger exists; what is
   missing is the strategy and the estimator. Reference behavior: elide old tool
   results to head+tail before any summarization, never split a tool call from
   its result, and prefer the endpoint-reported input-token count over local
   `chars/N` estimates. Pi still owns the compactor, so the valuable parts now
   are the estimator feeding the context meter/ledger and the boundary rule if
   warpi ever owns a cut. Areas: `standalone/pi-helper/src/runtime.ts`,
   `crates/standalone_agent/src/usage_ledger.rs`,
   `crates/standalone_agent/src/warp_events.rs` (`finished()` still emits
   `token_usage: Vec::new()`).

4. **Per-model context/output defaults (S–M).** A longest-prefix lookup table
   (model-id prefix → context window, output cap) so preset and custom profiles
   start with sensible limits. Ship as overridable defaults, never as validation:
   the data is endpoint-dependent and goes stale. Areas:
   `crates/standalone_agent/src/provider.rs`,
   `app/src/settings_view/local_provider_page.rs`.

5. **Permission-policy vocabulary (M, design-level).** Reusable wording and
   structures for approval decisions: hard denies that survive bypass,
   symlink-aware path confinement, compound-command analysis (a rule for
   `git status` must not vouch for `git status && rm -rf x`), and suggesting the
   narrowest rule that would have allowed a call. Warp keeps the approval
   authority; treat this as policy/UX input for the approval surface and the
   bridge's argument validation, to confirm with the app owners first. Areas:
   Warp's approval policy under `app/src/ai/**`
   (`blocklist/permissions.rs`, the action-model cancel/reject paths) and the
   bridge's argument validation.

6. **Node-helper packaging safety (S).** Build-time assertions (every runtime
   import is a declared dependency; the bundle contains no stray `node_modules`)
   plus a pack → install → run test for the helper tarball in a temp directory.
   Areas: `standalone/pi-helper/` build/test scripts, `packaging/`.

## Working agreements

- One writer per shared interface: protocol frames, tool names, and event
  semantics are frozen in `ARCHITECTURE.md` before parallel work.
- Every milestone ends with: relevant `cargo test` run, a local commit, and an
  updated status line in this file.
