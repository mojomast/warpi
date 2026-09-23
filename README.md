# warpi

<img src="docs/assets/warpi-wordmark.png" alt="warpi wordmark" width="320">

warpi is an independent fork of [Warp](https://github.com/warpdotdev/warp) that
keeps the native Warp UI and replaces the cloud agent backend with a local one.
Agent inference runs against a user-configured OpenAI-compatible endpoint — a
hosted API or a server you run yourself — through the
[Pi coding-agent SDK](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)
(`0.84.2`, pinned exactly). A private Node helper owns the model loop and the
tool protocol; the Rust adapter in `crates/standalone_agent` supervises it, and
Warp's own approval, tool-card, diff, and execution paths stay in charge of every
workspace side effect. **No Warp account is required, and no Warp server sits in
the agent path.**

Current version: **0.1.0** (release notes in [`CHANGELOG.md`](CHANGELOG.md); the
app shows the same string in **Settings → About**). This is an early,
deliberately honest release: every capability below carries an evidence pointer,
and anything that is not verified says so.

History: this repository imports upstream Warp at
`71088ba18d27114ccfb358901c66b54220cf30d0` and replays the fork commits from
branch `standalone-pi-backend` on top (0.1.0 corresponds to `4cda907`). The fork
uses its own identity (`dev.warpi.warpi`, data directory `.warpi`/`warpi`,
binary `warpi`, `WARPI_*` environment variables), so stock Warp and warpi can
coexist on one machine.

## Why warpi exists

Warp's agent is excellent, but it runs through Warp's servers and the model
selection that comes with them. warpi keeps the parts of Warp that are hard to
replace — the terminal, the block UI, the approval and diff surfaces — and moves
the agent's brain to an endpoint **you** choose:

- **Bring your own model.** Any OpenAI-compatible Chat Completions endpoint,
  local (Ollama, LM Studio, llama.cpp, vLLM) or hosted (DeepSeek, Kimi/Moonshot,
  OpenAI, OpenRouter, Groq, Mistral, xAI, Together, Fireworks, or a custom URL).
- **No account, no cloud agent path.** Sign-in, credits, and Warp's cloud agent
  surfaces are hidden while standalone mode is on; requests go from your machine
  to your endpoint and nowhere else in the agent path.
- **Warp still owns side effects.** The model proposes; Warp's native approval
  card, deny/allow rules, allowlist, redirection checks, and read-only checks
  decide and execute.

It is for people who are comfortable building from source and running an early
development build, and who would rather see an honest status table than a
marketing page. It is **not** a signed product, it is not sandboxed, and it is
not (yet) verified on Windows or macOS GUI (see
[Limitations](#limitations-and-not-yet-verified)).

## Quick start (Linux x86_64)

The verified configuration is a **Linux x86_64 development build**. Prerequisites
are the same as upstream Warp: Rust `1.92.0` from `rust-toolchain.toml`,
`protoc` >= 3.15, `cmake`, `pkg-config`, a C/C++ toolchain, ALSA development
headers for the GUI feature (`libasound2-dev` on Debian/Ubuntu), and **Node >=
22.19** for the helper. (The audit machine has no `sudo`; `standalone/dev-env.sh`
provides protoc/cmake/ALSA shims and is a development convenience, not part of
the product.)

```bash
# 1. Build the private Pi helper
cd standalone/pi-helper
npm ci
npm run build          # tsc -> dist/

# 2. Build and run warpi
cd ../..
cargo build -p warp --bin warpi --features gui
target/debug/warpi
```

The app finds the helper at `standalone/pi-helper/dist/main.js` relative to the
executable (or via `WARPI_PI_HELPER_ENTRY`). If the helper is missing, agent
requests fail with an explicit local error.

### Add a provider and a key

Open **Settings → Agents → Local Pi provider** (screenshot:
[`docs/assets/provider-settings.png`](docs/assets/provider-settings.png)).

1. Pick a **provider preset** — `Custom endpoint…`, DeepSeek, Kimi (Moonshot),
   OpenAI, OpenRouter, Groq, Mistral, xAI (Grok), Together AI, Fireworks AI,
   Ollama (local), LM Studio (local), llama.cpp server (local), or vLLM (local).
   A preset prefills a base URL and, where it is stable, suggested model ids and
   context/output limits; **every field stays editable**.
2. Set the **model id** (sent verbatim to the provider) and, optionally,
   **additional models** (comma-separated) that should also appear in the picker.
3. Set **context** and **output** limits. They are required; no provider model
   catalog is consulted and no catalog request is made.
4. Choose **authentication**: `none`, or an **API key**. The key is written to
   the OS secret store (macOS Keychain, Windows Credential Manager/DPAPI, Linux
   Secret Service); on Linux without a Secret Service it lands in a
   `0600`, AES-256-GCM-encrypted fallback file under the app state directory —
   never in the config file.
5. **Test connection** probes `GET {base}/models` with the profile's auth mode
   and an 8-second timeout. `2xx` is reachable; `404` is "reachable, `/models`
   is optional"; `401`/`403`/`5xx` and transport errors are reported as failures.
6. **Save profile**.

### Pick a model

Open the native model picker. Every enabled model of every configured profile
appears as `standalone:<profile>|<model>` (for example
`standalone:deepseek|deepseek-v4-pro`) and shows as `DeepSeek · deepseek-flash`
(screenshot:
[`docs/assets/model-picker.png`](docs/assets/model-picker.png)). Selecting one
switches the active profile and the model id used when the next helper session
opens. The same page lists the active profile's models with an enable/disable
toggle; disabled models are persisted per profile as `disabled_models`, never
appear in the picker, and the model currently in use cannot be disabled
(screenshot:
[`standalone/evidence/gui-model-toggle.png`](standalone/evidence/gui-model-toggle.png)).

A conversation whose profile, model, base URL, or key changed reopens on its
next fresh prompt. A turn that is already live keeps the endpoint it started on,
so the model chip can briefly disagree with the model serving that turn.

### Files, config, and the optional bundle

- **Config file (Linux default):**
  `~/.local/share/warpi/standalone/config.json` (`$XDG_DATA_HOME/warpi/...` when
  `XDG_DATA_HOME` is set), or any path via `WARPI_STANDALONE_CONFIG`.
- **Conversation and usage state:** under the same data directory
  (`session-map.json`, the usage ledger, and the helper scratch dir).
- **Packaged development bundle (optional).** Instead of running from
  `target/debug/`:

  ```bash
  cargo build -p warp --bin warpi --features gui
  bash packaging/package-warpi.sh
  ./dist/warpi-linux-x86_64/install.sh   # ~/.local/opt/warpi, launcher in ~/.local/bin/warpi
  ```

  `packaging/package-warpi.sh` never uses `sudo` or the network and refuses to
  run unless the binary and built helper are present. The bundle carries the
  helper's production `node_modules`, so **Node >= 22.19 must be on `PATH`** at
  runtime. No signed installer ships (see
  [Limitations](#limitations-and-not-yet-verified)).
- **Optional extra: DpQuake fonts.** Two decorative Quake fonts ship in
  `standalone/fonts/DpQuake/` but are never selected by default.
  `standalone/scripts/install-quake-fonts.sh` installs them into the user font
  cache and teaches fontconfig to treat `DpQuake` as monospace. `dpquake.txt`
  **must always ship alongside them** — see `standalone/fonts/DpQuake/README.md`.

## How it works

```
+---------------------------------------------------------------+
| Warp native UI + controller + history + action model          |
|   approvals . tool cards . diffs . persistence . task state   |
|   executes every workspace side effect                        |
+---------------------------------+-----------------------------+
                                  | ResponseEvent          | ai::Request
                                  | (Init / ClientActions  | (task context +
                                  |  / Finished)           | user input)
+---------------------------------------------------------------+
| crates/standalone_agent   (Rust adapter)                      |
|   provider.rs   local profile registry (no secrets)           |
|   secrets.rs    OS secret store -> in-memory SecretString     |
|   helper.rs     supervised stdio process, bounded frames      |
|   bridge.rs     session/turn/exchange state, tool correlation |
|   usage_ledger.rs  durable token/cost ledger (cost unused)    |
|   warp_events.rs  bridge events -> native Warp protobuf       |
+---------------------------------+-----------------------------+
                                  | NDJSON frames (v1)     | NDJSON frames (v1)
                                  | helper stdout          | helper stdin
+---------------------------------------------------------------+
| standalone/pi-helper (private Node helper; stdio, no port)    |
|   Pi SDK owns the model loop and the transcript               |
|   every workspace tool suspends on a promise until Warp       |
|   delivers the result                                         |
+---------------------------------+-----------------------------+
                                  | HTTPS: OpenAI Chat
                                  | Completions, streamed
                             +----v-----------------------------+
                             | user-configured endpoint         |
                             | (local or remote; no Warp        |
                             |  servers involved)               |
                             +----------------------------------+
```

Ownership is split so neither side duplicates the other:

- **Pi (helper)** owns the model loop, the prompt and tools, the transcript, and
  compaction. The SDK retry layer is disabled at session open
  (`retry.enabled: false`), so a provider failure is terminal for the run right
  now. The helper never executes a workspace side effect.
- **`standalone_agent`** owns transport, protocol validation, correlation, the
  turn/queue/usage bookkeeping, and turning helper events into the same native
  `ResponseEvent`s the Warp server would send.
- **Warp** owns approvals, permissions, execution, diffs, history, and
  persistence. A model-provided `is_read_only` or risk value is never treated as
  authorization.

A **run** is one Pi prompt and may span several Warp request/response
**exchanges**: when Pi calls a tool, the flow pauses, Warp runs the approved
action, and the result resumes the same Pi prompt. The model is offered exactly
seven brokered tools — `bash`, `bash_output`, `read`, `write`, `edit`, `glob`,
`grep` — and Pi's own built-in tools are disabled. Approvals are Warp's: a shell
call raises the native approval card (screenshot:
[`standalone/evidence/gui-tool-approval.png`](standalone/evidence/gui-tool-approval.png)),
and the completed round trip — command block, output, and final assistant text —
is in
[`standalone/evidence/gui-round-trip-complete.png`](standalone/evidence/gui-round-trip-complete.png).
Details are in `standalone/ARCHITECTURE.md`.

## What works today

`Verified` means reproduced with a test, a log, or a screenshot in this
repository. `Not verified` and `Deferred` are used deliberately.

| Capability | Status | Evidence |
| --- | --- | --- |
| Native Warp GUI on Linux x86_64 (development build) | **Verified** | window renders under Xvfb + Mesa lavapipe: `standalone/evidence/gui-native-window.png` |
| **Hosted provider driven through the GUI**: prompt → streamed answer, with the profile and model shown in the model chip | **Verified** — DeepSeek `deepseek-flash`; prompt `Reply with the single word pong.` answered `pong.` | `docs/assets/usage-footer.png`; model chip and context meter in `docs/assets/context-meter.png` |
| End-to-end agent round trip (deterministic loopback fixture): prompt → local provider → native tool approval card → Run → command output → result back to Pi → second provider request → final text | **Verified** | `standalone/evidence/gui-tool-approval.png`, `gui-round-trip-complete.png` |
| Provider settings page (Settings → Agents → Local Pi provider), presets, and **Test connection** (`GET {base}/models`) | **Verified** | `docs/assets/provider-settings.png`, `standalone/evidence/gui-test-connection.png` |
| Per-response **usage footer**: input/output/cache/total tokens, tokens/second, and duration under each standalone response (cost omitted — see Roadmap) | **Verified** | `docs/assets/usage-footer.png`; `app/src/ai/standalone/usage_model.rs` |
| **Context-window meter** in the input footer: amber at ≥ 75 %, red at ≥ 90 %, a "next turn may trigger compaction" note at ≥ 95 %, and a deliberate **no fake `0%`** when the provider reports no tokens ("Context usage unavailable") | **Verified** | `docs/assets/context-meter.png`; `context_meter_level`/`context_meter_label` in `usage_model.rs` |
| Two toggles on the provider page, both **default Shown**: `Per-response usage footer` and `Context-window meter` (`standalone_response_usage`, `standalone_context_meter`; never cloud-synced) | **Verified** | `docs/assets/provider-settings.png`; `app/src/settings/ai.rs` |
| Native model picker: every enabled model of every profile as `standalone:<profile>\|<model>`; selecting one switches the active profile/model for the next helper session; a changed profile/model/key reopens the conversation on its next fresh prompt | **Verified** | `docs/assets/model-picker.png`, `standalone/evidence/gui-model-picker-all-providers.png` (predates cloud-entry hiding) |
| Per-model enable/disable, persisted per profile as `disabled_models` | **Verified** | `standalone/evidence/gui-model-toggle.png` |
| Prompt queueing: a prompt submitted while a turn runs is queued FIFO at the app and bridge layers instead of failing; `Ctrl+Alt+Shift+Enter` (`Cmd+Alt+Shift+Enter` on macOS) cancels the running turn and sends the head queued prompt immediately. The **hint and queued-row rendering** (keycaps + "cancel the running turn and send now") are verified; the keypress itself has **not** been driven | **Verified (rendering) / keypress not exercised** | `docs/assets/queue-hint.png`; automated app input test, bridge queue tests, vertical-slice FIFO ordering |
| Turn lifecycle: exactly one result per forwarded tool call (dropped results synthesized in the same resume), untranslatable calls answered in place, cancel deadline settling as `Finished(Done)`, and stall / pending-tool / compaction deadlines that free the session | **Verified** (automated stub-helper suites) | `standalone/ARCHITECTURE.md` §Turn lifecycle guarantees |
| Long-running commands: a still-running command returns partial output plus non-interactive guidance; `bash_output` polls it with a bounded wait (1–120 s, default 30 s) and cannot write to or stop it | **Verified** (automated); not driven in the GUI | `standalone/PROVIDER_COMPATIBILITY.md` |
| `auth = none` sends no `Authorization` header and never leaks the helper's internal placeholder | **Verified** (automated) | `auth_none_never_sends_an_authorization_header` in `crates/standalone_agent/tests/vertical_slice.rs` |
| Real hosted provider at the **adapter** level (DeepSeek `deepseek-flash`, `deepseek-v4-pro`): prompt → tool call → brokered workspace call → resume → second request → final marker | **Verified (PASS)**, rerun on demand | `WARPI_REAL_PROVIDER_KEY=... cargo test -p standalone_agent --test real_provider`; `standalone/VALIDATION.md` |
| Warp's account/billing/cloud surfaces hidden while standalone mode is on: the model picker's cloud entries and the Account, Billing, Teams, Referrals, Warp Drive, Shared Blocks, Code Indexing, Cloud Environments, and Cloud Agent API Keys settings sections are hidden behind a local fallback; voice input and the SSH warpification extension are hidden (plain `ssh` still works) | **Verified** (source); the committed picker screenshot predates the hiding | `app/src/standalone_ui.rs`, `app/src/ai/llms.rs`, `app/src/settings_view/mod.rs` |
| About page: warpi wordmark, `v0.1.0`, repository link, and fork-first copyright | **Verified** | `docs/assets/about-0.1.0.png` |
| Diff review / file-edit (`write`/`edit`) flow driven in the GUI | **Not verified** — wired to `ApplyFileDiffs` and tested at the event level | `standalone/VALIDATION.md` |
| Conversation restart / idle restore driven in the GUI | **Not verified** — the conversation → Pi session mapping is durable | `standalone/VALIDATION.md` |
| Windows / macOS native build and GUI | **Not verified** — see [Limitations](#limitations-and-not-yet-verified) | `standalone/WINDOWS.md`, `.github/workflows/warpi-build.yml` |
| Subagents (`task` tool) | **Deferred** — the Rust adapter and helper code are merged but **inert by default**: a session only gets the tool when the config sets `subagents.enabled` **and** the helper advertises the `subagents` capability (and the process default `WARPI_SUBAGENTS` is an escape hatch). Not part of the v1 feature set | `crates/standalone_agent/tests/subagents_forwarding.rs`, `standalone/pi-helper/test/subagents.test.ts` |
| Durable prompt queue and event-log delivery | **Deferred to 0.1.1** — `journal.rs`/`event_log.rs` are merged but have **no callers**; the 0.1.0 prompt queue is in-memory | `standalone/PLAN.md`, Roadmap below |
| Cost / pricing display | **Deferred** — no preset prices ship, so the footer omits USD | `app/src/ai/standalone/usage_model.rs` |
| TUI wired to the standalone backend | **Deferred** — the upstream headless TUI builds as `warpi-tui` (`script/run-tui`) but is not connected to the local backend | `script/run-tui` |
| MCP tools, `bash_write`/`bash_cancel`, file deletion | **Deferred** by design; not advertised to the model | `standalone/PROVIDER_COMPATIBILITY.md` |
| Release packaging / signed installer | **Not done** — a development packaging script ships; the upstream `script/` bundlers still target the old `oss` channel | `standalone/VALIDATION.md`, `packaging/package-warpi.sh` |

**Test inventory (0.1.0 source).** `cargo test -p standalone_agent -- --list`
reports **156 test functions** — 117 unit (`usage_ledger` 26, `bridge` 24,
`journal` 19, `event_log` 15, `warp_events` 12, `protocol` 11, `provider` 7,
`secrets` 2, `helper` 1) and 39 integration across 15 binaries (vertical slice
11; durable queue recovery 5; event-log delivery 4; untranslatable calls,
subagents forwarding 3 each; tool-result loss, session reopen,
duplicate-and-reattach 2 each; user rejection, transcript repair, session
isolation, real provider, paused-turn cancel, compaction watchdog, cancel
settlement 1 each). The Pi helper has **37 tests** (subagents 14, broker 6,
protocol 4, runtime smoke 4, usage telemetry 4, transcript repair 3, usage live
2). The last recorded full runs predate the newest suites, so
`standalone/VALIDATION.md` marks those **NOT RE-RUN** rather than PASS. The
`journal`/`event_log` suites exercise modules that the app does not call yet.

## Configuration reference

### Profile fields

```json
{
  "enabled": true,
  "profiles": [
    {
      "id": "local",
      "display_name": "Local llama.cpp",
      "base_url": "http://127.0.0.1:8080/v1",
      "wire": "open_ai_chat_completions",
      "model_id": "qwen3-coder-30b",
      "models": [],
      "disabled_models": [],
      "credential": "none",
      "context_limit": 131072,
      "output_limit": 8192,
      "compat": { "supports_developer_role": false },
      "reasoning": false,
      "supports_image_input": false,
      "headers": {},
      "pricing": {}
    }
  ],
  "active_profile": "local",
  "load_context_files": true,
  "max_context_file_bytes": 65536
}
```

- `wire` accepts only `open_ai_chat_completions`; the older single-`profile`
  form is still read.
- `credential` is `"none"` or a `secret_store` reference; the secret itself
  never lives in this file.
- `models` lists extra selectable ids for the same endpoint (the UI takes them
  comma-separated); `disabled_models` records the ids switched off; the picker
  offers `model_id` plus every `models` entry that is not disabled.
- `compat` flags are opt-in and conservative: `supports_developer_role`,
  `supports_reasoning_effort`, `supports_usage_in_streaming`, `tool_choice`,
  and `max_completion_tokens_field`. Unknown values are never assumed.
- `reasoning` and `supports_image_input` are off in v1; `headers` are extra
  non-secret headers (an `Authorization` header is rejected — use `credential`).
- `pricing` is a per-model USD/1M-token table the ledger can price against; it
  is never sent to the helper, **no preset prices ship**, and a missing row
  means "cost unknown", not free.

### Provider presets

`PROVIDER_PRESETS` in `app/src/ai/standalone/mod.rs`: `Custom endpoint…`,
**DeepSeek** (`deepseek-flash`, +`deepseek-v4-pro`), **Kimi (Moonshot)**
(`kimi-k2-0905-preview`, +`kimi-k2-turbo-preview`), **OpenAI**, **OpenRouter**,
**Groq**, **Mistral**, **xAI (Grok)**, **Together AI**, **Fireworks AI**,
**Ollama (local)**, **LM Studio (local)**, **llama.cpp server (local)**,
**vLLM (local)**. DeepSeek is the one verified end-to-end (both adapter and
GUI); the others beyond a preset are unverified.

### Environment variables

| Variable | Effect |
| --- | --- |
| `WARPI_STANDALONE_CONFIG` | Path to the standalone config JSON (overrides the default). |
| `WARPI_PI_HELPER_ENTRY` | Explicit path to the helper entry (`dist/main.js`); development/testing override. |
| `WARPI_TURN_STALL_TIMEOUT_SECS` | Seconds with no helper event before the turn is settled (default `120`). |
| `WARPI_PENDING_TOOL_TIMEOUT_SECS` | Seconds a turn may wait on tool calls (default `1800`; `0` disables the deadline). |
| `WARPI_CANCEL_DEADLINE_SECS` | Seconds to wait for a helper to acknowledge `turn.cancel` (default `5`). |
| `WARPI_COMPACTION_TIMEOUT_SECS` | Seconds a compaction may run inside a turn (default `600`). |
| `WARPI_LEDGER_MAX_BYTES` | Byte budget for the usage ledger before oldest-first pruning. |
| `WARPI_SUBAGENTS` | Helper process default that enables the opt-in `task` subagent; normally unset. |

(`WARPI_EVENT_LOG_*` variables exist in the merged-but-unwired event-log module
and have no effect in 0.1.0. The `WARPI_REAL_PROVIDER_*` variables are read only
by the opt-in `real_provider` test.)

## Security and privacy

Short version; the honest full statement is `standalone/SECURITY.md`.

- **Approvals stay in Warp.** The adapter only emits typed actions. A shell tool
  call is always labelled `NontrivialLocalChange`, never `is_read_only`, and is
  marked `is_risky: true`; model-supplied risk values are not authorization.
  Pi cannot classify a command's risk, so marking every shell call risky keeps
  Warp's denylist, redirection, allowlist, and read-only checks in the path
  instead of the `AgentDecides` auto-execute shortcut.
- **Rejections return a definite result.** A rejected shell call is answered
  with a converted `cancelled` error result, so the Pi run resumes with an error
  instead of the old "run the command again" text; the bridge also keeps a deny
  registry that answers a still-pending denied call with `status: rejected`.
  Remaining caveats: in the app-driven flow the converted error result takes
  precedence, so the typed `rejected` status is not guaranteed end-to-end; a
  lone rejection is delivered on the next request rather than pushed; and the
  diff-review Reject path is not wired yet, so a rejected `write`/`edit` still
  surfaces as a cancelled error.
- **One origin per profile.** TLS is required for public hosts; plain `http` is
  allowed only for loopback/private endpoints the user typed. Certificate
  validation is never disabled globally.
- **The helper is supervised, not sandboxed.** Explicit executable and argv, a
  controlled working directory, an allowlisted environment, no listening socket,
  no global Pi install, no `~/.pi`, no ambient extensions. It runs with the same
  user privileges as Warp.
- **Model output is untrusted.** Tool names must be in the active set, arguments
  are schema-validated before the broker sees them, and unknown names or
  arguments fail closed.
- **Protocol bounds.** Newline-delimited JSON frames are capped at 4 MiB;
  protocol version, `seq`, session/turn/exchange/generation identity, duplicate
  tool-call ids, and foreign/stale tool results are validated. A result that
  matches no pending call fails the exchange without consuming state, duplicates
  are ignored, and calls the app drops are closed with a synthesized cancelled
  error so the Pi run never waits on a promise nobody resolves. Per-result
  content and the aggregate resume frame are bounded (an oversized batch is
  truncated, with a marker, and every pending call still gets a result), and a
  malformed helper frame is scoped to its own session (that path has no
  dedicated test yet).
- **Credentials.** Stored in the OS secret store by reference; on Linux without
  a Secret Service, Warp's secure-storage fallback keeps the key in an
  AES-256-GCM-encrypted, owner-only (`0600`) file under the app state directory.
  It is read into an in-memory `SecretString` that redacts itself in
  `Debug`/`Display` and zeroizes on drop, and it is not persisted in the
  helper's session files. The key is handed to the helper only inside the
  private `session.open` frame.
- **No Warp account, no Warp server in the agent path.** Sign-in and cloud
  surfaces are hidden while standalone mode is on; `standalone/NETWORK_DEPENDENCIES.md`
  enumerates every application-managed egress path.
- **Unknown outcomes are surfaced, not repaired.** A crash after an effect but
  before its result is recorded leaves the outcome unknown; non-idempotent
  commands are never retried automatically. No exactly-once guarantee is
  claimed.

## Limitations and not-yet-verified

- **Windows: build green, tests not green.** CI run `35879868946` passed the
  Linux job end to end (release build, Rust adapter tests,
  `packaging/package-warpi.sh`, artifact upload). The Windows job passed the
  checkout, pinned `protoc` + cmake, helper build/tests, and the release
  `warpi.exe` build (steps 1–8), then failed **step 9, "Test the Rust adapter"**:
  the helper's first request to the cross-process Node fixture in
  `session_isolation.rs` reports `provider_error: "Connection error."` The same
  suite passes on Linux and the helper's own tests pass on the same runner. The
  fixture was hardened (listener readiness probe, stdout/unhandled-error
  guards) and the step now runs `--no-fail-fast` with `continue-on-error`, but
  the root cause is **not reproducible off Windows**, so the fix is
  **UNVERIFIED**. Windows bundle staging/upload and Windows **packaging** have
  never been exercised at all. See `standalone/WINDOWS.md`.
- **macOS: entirely unverified.** There is no macOS host in this work. The
  bundle identity, icon (`app/channels/warpi/icon/icon.icns`), and
  menu/metadata wiring are present **by construction only**.
- **Packaging and signing.** There is no signed installer or release bundle for
  any platform; `packaging/package-warpi.sh` is a no-sudo, no-network development
  convenience.
- **GUI flows not driven.** The diff-review (`write`/`edit`) flow, restart/idle
  restore, and the send-now keypress are not exercised in the GUI. The last
  recorded full test runs predate the newest suites (see
  `standalone/VALIDATION.md`).
- **Hosted providers beyond DeepSeek** are unverified; the Kimi (Moonshot) preset
  only prefills an endpoint and suggested model ids.
- **Robustness gaps.** Provider-error retry is off (the SDK retry layer is
  disabled at session open, so a transient failure is terminal), the
  crash-injection matrix and a forced-compaction integration test are missing,
  helper sessions are not idle-evicted, request interleaving is not per-session,
  a compaction longer than its 600-second deadline is still cancelled, and
  helper shutdown on app teardown is best-effort. See `standalone/PLAN.md`.

## Roadmap

0.1.1 is where the merged-but-unwired work lands. None of it is claimed today.

- **Wire durable queueing and event-log delivery.** `journal.rs`/`event_log.rs`
  exist with generation fencing and a per-conversation event log, but have no
  callers; until they are wired, a restart does not recover queued prompts or
  undelivered events (only the conversation → Pi session mapping survives).
- **Enable cost/pricing display.** The ledger already records tokens and can
  price a model, but no preset prices ship, so the footer omits USD.
- **Turn on subagents.** The `task` adapter is present and inert by default;
  enabling it needs `subagents.enabled` plus the helper capability, and a
  product decision on the approval UX.
- **Explore the TUI and remote work.** The upstream headless TUI builds as
  `warpi-tui` but is not wired to the local backend. Plain `ssh` in the terminal
  works; Warp's remote-server / SSH-warpification surfaces are hidden and the
  adapter refuses remote actions, so remote sessions are an open exploration,
  not a committed feature.

### Parked ideas (deferred, from a third-party review)

These are parked, not scheduled; the current focus is finishing and shipping the
tracks above. They come from an internal review of the MIT-licensed
[Jgracier/ClikCode](https://github.com/Jgracier/ClikCode) — design ideas only,
with no code copied, so `LICENSE-NOTES.md` records the donor row as empty. Any
future borrow must reproduce the donor's MIT notice first.

- **Retry recoverable provider errors.** Classify endpoint failures by kind or
  status and retry the recoverable ones with bounded backoff, without fighting
  the existing inactivity and cancellation timeouts.
- **Steer a running turn.** Let a new message join the turn already running
  (today it queues, or send-now cancels and replaces it).
- **Two-stage compaction with an authoritative token count.** Elide old tool
  output before summarizing and prefer the endpoint-reported input-token count
  over local estimates.
- **Per-model context/output defaults.** A small longest-prefix table so presets
  start with sensible limits the user can still override.
- **Clearer permission-policy wording.** Reusable allow/deny phrasing
  (narrowest-rule suggestions, compound-command awareness); Warp remains the
  approval authority.
- **Safer helper packaging.** Build-time assertions and a pack → install → run
  test for the helper bundle.

## License and attribution

- Upstream Warp code is **AGPL-3.0-only** ([`LICENSE-AGPL`](LICENSE-AGPL)),
  except `crates/warpui` and `crates/warpui_core`, which are **MIT**
  ([`LICENSE-MIT`](LICENSE-MIT)).
- All new fork code (`crates/standalone_agent`, `standalone/**`, the app wiring,
  the packaging scripts, the `warpi` binary) is **AGPL-3.0-only**.
- Copyright is fork-first in the app and installers: **© 2026 warpi
  contributors. Warp is © 2020–2026 Denver Technologies, Inc.** (screenshot:
  [`docs/assets/about-0.1.0.png`](docs/assets/about-0.1.0.png)).
- Behaviour ported from **sasuke39/openwarp** at commit `5045d30` (MIT) was
  rewritten, not copied; the required notice is reproduced in
  `standalone/DONOR-ATTRIBUTION.md`, and `standalone/REUSE.md` lists exactly what
  was ported and every deliberate deviation. The donor register in
  [`LICENSE-NOTES.md`](LICENSE-NOTES.md) records the ported behaviour and leaves
  the ClikCode row empty because no code was taken.
- The Pi coding-agent SDK (`0.84.2`) and typebox (`1.3.7`) are npm dependencies
  under their own licenses (MIT per the pinned packages' metadata).
- The DpQuake/QUAKE2 fonts in `standalone/fonts/DpQuake/` are free to
  redistribute under the author's terms, but `dpquake.txt` **must always ship
  with them** (the author also notes the stylized logos are id Software
  copyright). See `standalone/fonts/DpQuake/README.md`.
- [`THIRD_PARTY_LICENSES.txt`](THIRD_PARTY_LICENSES.txt) is the runtime
  dependency license census (the Rust normal-edge runtime closure and the bundled
  npm production tree); the authoritative per-crate texts are generated by
  `cargo about generate` at release time. Redistribution obligations are
  summarised in `LICENSE-NOTES.md`.

## Not affiliated

warpi is an independent fork and is **not affiliated with, endorsed by, or
sponsored by Warp or Denver Technologies, Inc.** "Warp" is a trademark of its
owner; this fork keeps its own app identity and does not ship Warp branding as
its own.
