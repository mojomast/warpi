# warpi

<img src="docs/assets/warpi-wordmark.png" alt="warpi wordmark" width="320">

warpi is a fork of [Warp](https://github.com/warpdotdev/warp) (pinned at
`71088ba18d27114ccfb358901c66b54220cf30d0`) that keeps the native Warp UI and
replaces the cloud agent backend with a local one. Agent inference runs against a
user-configured OpenAI-compatible endpoint through the
[Pi coding-agent SDK](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)
(`0.84.2`, pinned exactly); the Rust adapter in `crates/standalone_agent`
supervises a private Node helper, and Warp's own approval, tool-card, diff, and
execution paths stay in charge of every workspace side effect. No Warp account is
required, and no Warp server sits in the agent path.

Status: early development, validated as a **Linux x86_64 development build**.
The multi-profile model picker and per-model enable/disable are verified in the
native GUI, and the agent adapter is validated end-to-end against a real hosted
provider (DeepSeek). Prompt queueing, the seven-tool loop (`bash`, `bash_output`,
`read`, `write`, `edit`, `glob`, `grep`), and the turn-lifecycle guarantees are
implemented and covered by automated tests. Driving a hosted provider through the
GUI is **pending** and is not claimed. There is no signed installer or release
bundle yet. Every claim below traces to a test, a screenshot, or an explicit
**not verified** / **not run** marker.

Version: **0.1.0**. Release notes are in [`CHANGELOG.md`](CHANGELOG.md); the app
renders the version in **Settings → About**.

History: this repository imports upstream Warp at
`71088ba18d27114ccfb358901c66b54220cf30d0` and replays the fork commits from
branch `standalone-pi-backend` on top. The fork uses its own identity
(`dev.warpi.warpi`, data directory `.warpi`/`warpi`, binary `warpi`, `WARPI_*`
environment variables), so stock Warp and warpi can coexist on one machine.

## What works today

| Capability | Status | Evidence |
| --- | --- | --- |
| Native Warp GUI on Linux x86_64 (development build) | **Verified** | window renders under Xvfb + Mesa lavapipe: `standalone/evidence/gui-native-window.png` |
| End-to-end agent round trip: prompt → local provider → native tool approval card → Run → command output → result back to Pi → second provider request → final text | **Verified** (deterministic loopback fixture) | `standalone/evidence/gui-tool-approval.png`, `gui-round-trip-complete.png`; fixture request captures |
| Provider settings page (Settings → Agents → Local Pi provider) and **Test connection** (`GET {base}/models`) | **Verified** | `standalone/evidence/gui-provider-settings.png`, `gui-test-connection.png` |
| Native model picker: every enabled model of every configured profile appears as `standalone:<profile>\|<model>`; selecting one switches the active profile and the model id for the next helper session, and a changed profile/model/key reopens a conversation on its next fresh prompt (see the roadmap) | **Verified** (GUI, deterministic fixture plus a configured DeepSeek profile) | `standalone/evidence/gui-model-picker-all-providers.png` |
| Per-model enable/disable in the settings page, persisted per profile as `disabled_models`: disabled models never appear in the picker, and the model in use cannot be disabled | **Verified** (GUI) | `standalone/evidence/gui-model-toggle.png` |
| Prompt queueing: a prompt submitted while a turn is running is queued FIFO at both the app and bridge layers instead of failing; `Ctrl+Alt+Shift+Enter` (`Cmd+Alt+Shift+Enter` on macOS) cancels the running turn and sends the head queued prompt now | **Verified** (automated: app input test, bridge queue tests, vertical-slice FIFO ordering); the keybinding and hints are not yet driven in the GUI | `standalone/ARCHITECTURE.md` §Prompt queueing and cancellation |
| Turn lifecycle: exactly one result per forwarded tool call (dropped results are synthesized in the same resume), untranslatable calls answered in place, cancel deadline with `RunCancelled` settling as `Finished(Done)`, stall and pending-tool deadlines that free the session | **Verified** (automated stub-helper suites: dropped results, untranslatable calls, cancel settlement) | `standalone/ARCHITECTURE.md` §Turn lifecycle guarantees |
| Long-running commands: a still-running command is returned as an error with partial output and non-interactive guidance; `bash_output` polls it with a 1–120 s bounded wait (default 30 s) and cannot write to or stop the command | **Verified** (automated: Rust event tests, helper broker tests); not yet driven in the GUI | `standalone/PROVIDER_COMPATIBILITY.md` |
| `auth = none` sends no `Authorization` header and never leaks the helper's internal placeholder | **Verified** (automated) | `auth_none_never_sends_an_authorization_header` in `crates/standalone_agent/tests/vertical_slice.rs` |
| Rust adapter suites: unit (`provider`, `secrets`, `protocol`, `bridge`, `helper`, `warp_events`), session isolation, vertical slice (round trip, `auth = none`, FIFO queueing, cancellation, provider failures), dropped-result, untranslatable-call, and cancel-settlement suites, plus one opt-in real-provider test that reports `NOT RUN` and passes without credentials | 56 test functions at the time of writing; the last recorded full run predates the newest suites, so those are **not re-run** yet | `cargo test -p standalone_agent`; `standalone/VALIDATION.md` for the exact PASS/NOT RUN state |
| Pi helper | 14 tests in the current source (protocol, runtime smoke, broker); the recorded log covers 13, so the `bash_output`/guardrail tests are **not re-run** yet | `cd standalone/pi-helper && npm test`; `standalone/evidence/helper-tests.log` |
| Real (hosted) provider — **adapter level**, DeepSeek `deepseek-flash` and `deepseek-v4-pro` | **Verified (PASS)**: prompt → provider tool call → brokered workspace call → resume with the real tool output → second provider request → final answer containing the marker | `WARPI_REAL_PROVIDER_KEY=... cargo test -p standalone_agent --test real_provider` (`crates/standalone_agent/tests/real_provider.rs`) |
| Real (hosted) provider — **driven through the GUI** | **Pending — not claimed.** The adapter-level pass is in the row above; a full hosted-provider round trip has not been driven through the GUI yet | — |
| Windows / macOS native build and GUI | **Not verified.** The backend crate passes `cargo check --target x86_64-pc-windows-gnu`, but the GUI cross-build is blocked by native C/asm dependencies (`aws-lc-sys`, SQLite, tree-sitter, …) with no cross C toolchain; a real Windows runner is the path. The GitHub Actions job has not run yet | `standalone/WINDOWS.md`, `.github/workflows/warpi-build.yml`, `standalone/ci/warpi-windows-job.md` |
| Diff review / file-edit (`write`/`edit`) flow driven in the GUI | **Not yet driven in the GUI** (wired to `ApplyFileDiffs` and tested at the event level) | `standalone/VALIDATION.md` |
| Conversation restart / idle restore driven in the GUI | **Not yet driven in the GUI** (durable conversation → Pi session mapping exists) | `standalone/VALIDATION.md` |
| MCP tools, subagents, `bash_write`/`bash_cancel` (writing to or stopping a running command), file deletion | **Not in v1** (deferred by design) | `standalone/PROVIDER_COMPATIBILITY.md` |
| Release packaging / signed installer | **Not done** — a development packaging script is included; the upstream `script/` bundlers still target the old `oss` channel | `standalone/VALIDATION.md`, `packaging/package-warpi.sh` |

## Architecture

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
|   warp_events.rs  bridge events -> native Warp protobuf       |
+---------------------------------+-----------------------------+
                                  | NDJSON frames (v1)     | NDJSON frames (v1)
                                  | helper stdout          | helper stdin
+---------------------------------------------------------------+
| standalone/pi-helper (private Node helper; no listening port) |
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

- **Pi (helper)** owns the model loop, prompt and tools, transcript, and
  compaction. The SDK retry layer is disabled at session open
  (`retry.enabled: false`), so a provider failure is terminal for the run right
  now; mapping retryable failures onto a real retry layer is a known gap. The
  helper never executes a workspace side effect.
- **`standalone_agent`** owns transport, protocol validation, correlation, and
  turning helper events into the same native `ResponseEvent`s the Warp server
  would send.
- **Warp** owns approvals, permissions, execution, diffs, history, and
  persistence. A model-provided `is_read_only` or risk value is never treated as
  authorization.

A **run** is one Pi prompt and may span several Warp request/response
**exchanges**: when Pi calls a tool, the flow pauses, Warp runs the approved
action, and the result resumes the same Pi prompt. Details are in
`standalone/ARCHITECTURE.md`.

## Quick start (Linux, development build)

Prerequisites (same as upstream Warp): Rust `1.92.0` from `rust-toolchain.toml`,
`protoc` >= 3.15, `cmake`, `pkg-config`, a C/C++ toolchain, ALSA development
headers for the GUI feature (`libasound2-dev` on Debian/Ubuntu), and **Node >=
22.19** for the helper. On the audit machine (no `sudo`) the protoc/cmake/ALSA
shims come from `standalone/dev-env.sh`; they are not part of the product.

```bash
# 1. Build the private Pi helper
cd standalone/pi-helper
npm ci
npm run build          # tsc -> dist/
npm test               # 14 tests

# 2. Build and run warpi
cd ../..
cargo build -p warp --bin warpi --features gui
target/debug/warpi
```

The app finds the helper at `standalone/pi-helper/dist/main.js` relative to the
executable (or via `WARPI_PI_HELPER_ENTRY`). If the helper is missing, agent
requests fail with an explicit local error.

**Configure through the UI (normal path).** Open
**Settings → Agents → Local Pi provider** and fill in:

- display name, base URL (for example `http://127.0.0.1:8080/v1`), model id,
  additional model ids (comma-separated), context and output limits. Provider
  presets prefill common endpoints and suggested models — DeepSeek,
  Kimi (Moonshot), OpenAI, OpenRouter, Groq, Mistral, xAI, Together, Fireworks,
  and local Ollama/LM Studio/llama.cpp/vLLM servers — and every field stays
  editable;
- authentication: `none`, or an API key that is written to the OS secret store
  (macOS Keychain, Windows Credential Manager/DPAPI, Linux Secret Service). On
  Linux systems without a Secret Service provider (headless boxes, bare X
  sessions) Warp's built-in fallback keeps the key in an AES-256-GCM-encrypted
  file under the application state directory, mode `0600` — never in the config
  file;
- **Test connection**, which probes `GET {base}/models` with the profile's
  authentication mode and an 8-second timeout. A `404` is reported as
  reachable-with-note because v1 supports manual model ids.

Every enabled model of every configured profile appears in the native model
picker as `standalone:<profile>|<model>` (for example
`standalone:deepseek|deepseek-v4-pro`). Picking one switches the active profile
and the model id for the next helper session; a conversation whose profile,
model, base URL, or key changed reopens on its next fresh prompt (landed 2026-09-23). A
turn that is already live keeps the endpoint it started on, so the model chip can
briefly disagree with the model serving that turn — see the roadmap.

The same page lists the active profile's models with an enable/disable toggle.
Disabled models are persisted per profile as `disabled_models`, never appear in
the picker, and the model currently in use cannot be disabled (pick another
model first). Saving a profile or toggling a model refreshes the picker without
a restart. The standalone on/off toggle lives on the same page.

**Or write the config file by hand.** Linux default:
`~/.local/share/warpi/standalone/config.json` (`$XDG_DATA_HOME/warpi/...` when
`XDG_DATA_HOME` is set), or any path via `WARPI_STANDALONE_CONFIG`. Example with
no authentication (see `standalone/BUILDING.md` for the API-key form):

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
      "supports_image_input": false
    }
  ],
  "active_profile": "local",
  "load_context_files": true,
  "max_context_file_bytes": 65536
}
```

`wire` only accepts `open_ai_chat_completions`; the older single-`profile`
form is still read. `models` lists further selectable ids for the same endpoint
(the UI takes them comma-separated), and `disabled_models` records the ids
switched off in the settings page; the picker offers `model_id` plus every
`models` entry that is not disabled. Credentials can also be written with the
platform tool under the key `warpi/profile/<profile-id>`; the value is read at
first use and kept in memory only.

**Packaged development bundle (optional).** Instead of running from
`target/debug/`, assemble a self-contained directory and install it without
`sudo`:

```bash
cargo build -p warp --bin warpi --features gui
bash packaging/package-warpi.sh
./dist/warpi-linux-x86_64/install.sh     # installs to ~/.local/opt/warpi, launcher in ~/.local/bin/warpi
```

`packaging/package-warpi.sh` never uses `sudo` or the network and refuses to run
unless the binary and built helper are already present. The bundle includes the
helper's production `node_modules` because the compiled helper imports the Pi
SDK at runtime; **Node >= 22.19 must be on `PATH`** when you run warpi.

**Optional bundled extra: DpQuake fonts.** Two decorative Quake fonts
(`DpQuake`, `QUAKE2`) ship in `standalone/fonts/DpQuake/` but are never selected
by default. `standalone/scripts/install-quake-fonts.sh` copies them into the
user font cache and teaches fontconfig to treat `DpQuake` as monospace, after
which they appear in **Settings → Appearance → Terminal font family**. They are
free to redistribute under the author's terms, with one hard condition:
`dpquake.txt` (the author's readme/licence, plus the id Software logo notice)
**must always ship alongside them** — see `standalone/fonts/DpQuake/README.md`.

## Provider compatibility (v1)

| Topic | v1 behaviour |
| --- | --- |
| Wire protocol | OpenAI **Chat Completions only** (`/chat/completions`, streamed, tool calls). The OpenAI Responses API and Anthropic Messages are rejected, not emulated. |
| Base URL | `https://host`, `https://host/v1`, `https://host/v1/`, and a full `/v1/chat/completions` are all accepted and normalised without duplicating `/v1`. Non-`http(s)` schemes, embedded credentials, query strings, and fragments are rejected. |
| Authentication | `none` (no `Authorization` header at all — asserted by an automated test) or an API key stored in the OS secret store and handed to the helper only inside the private `session.open` frame. |
| Capability flags | Explicit and conservative. `developer` role, `reasoning_effort`, `tool_choice`, and `max_completion_tokens` are opt-in; usage in the streamed response follows the SDK default (on). Image input and reasoning display are off in v1. Unknown values are never assumed. |
| Context/output limits | Required, user-entered. No provider model catalog is consulted and no catalog network request is made. |
| Test connection | `2xx` = reachable; `404` = reachable with a note (`/models` is optional); `401`/`403`/`5xx` = failure with status; DNS/connection/timeout = failure with the transport error. |
| Fallback | None in either direction. Standalone requests never reach Warp servers, and a failing local endpoint is reported as a local error. |
| Verified against a real endpoint | **Adapter level: YES** (DeepSeek `deepseek-flash` and `deepseek-v4-pro`, tool-call round trip; see the top table). GUI real-provider validation is **pending** and not claimed. Other providers are unverified; the compatibility tests themselves still use the deterministic loopback fixture. |

Full tables, including the seven tools offered to the model, the queueing
behavior, and the exact reject-vs-approximate rules, are in
`standalone/PROVIDER_COMPATIBILITY.md` and `standalone/ARCHITECTURE.md`.

## Security and trust model

Short version; the honest full statement is `standalone/SECURITY.md`.

- **Approvals stay in Warp.** The adapter only emits typed actions. A shell tool
  call is always labelled `NontrivialLocalChange`, never `is_read_only`, and is
  marked `is_risky: true`; model-supplied risk values are not authorization.
  Pi cannot classify a command's risk, so marking every shell call risky keeps
  Warp's denylist, redirection, allowlist, and read-only checks in the path
  instead of the `AgentDecides` auto-execute shortcut.
- **Rejections return a definite result.** A rejected shell call is answered
  with a converted `cancelled` error result (landed 2026-09-23), so the Pi run
  resumes with an error and the old "run the command again" text is gone; the
  bridge additionally has a typed `rejected` path for a call that is still
  pending. Still open: the app-sent error result takes precedence over the typed
  status, so `rejected` is not guaranteed end-to-end; a lone rejection is not
  pushed on its own (it is delivered on the next request), and the
  denial-vs-follow-up ordering is untested. The diff-review Reject path is not
  wired yet, so a rejected `write`/`edit` still surfaces as a cancelled error.
- **One origin per profile.** TLS is required for public hosts; plain `http` is
  allowed only for loopback/private endpoints the user typed. Certificate
  validation is never disabled globally.
- **The helper is supervised, not sandboxed.** Explicit executable and argv,
  controlled working directory, allowlisted environment, no listening socket,
  no global Pi install, no `~/.pi`, no ambient extensions. It runs with the same
  user privileges as Warp.
- **Model output is untrusted.** Tool names must be in the active set, arguments
  are schema-validated before the broker sees them, and unknown names or
  arguments fail closed.
- **Protocol bounds.** Newline-delimited JSON frames capped at 4 MiB; protocol
  version, `seq`, session/turn/exchange/generation identity, duplicate tool call
  ids, and foreign/stale tool results are all validated. A result that matches no
  pending call fails the exchange without consuming state, duplicates are
  ignored, and calls the app drops are closed with a synthesized cancelled error
  so the Pi run never waits on a promise nobody resolves. Per-result content and
  the aggregate resume frame are bounded (landed 2026-09-23): an oversized batch is
  truncated, with a marker, and every pending call still gets a result. A
  malformed helper frame is scoped to its own session (landed 2026-09-23); that path has
  no dedicated test yet.
- **Credentials.** Stored in the OS secret store by reference (or, on Linux
  without a Secret Service, in Warp's encrypted `0600` fallback file under the
  state directory); read into an in-memory `SecretString` that redacts itself in
  `Debug`/`Display` and zeroizes on drop. The key is not persisted in the helper's session files.
- **Unknown outcomes are surfaced, not repaired.** A crash after an effect but
  before its result is recorded leaves the outcome unknown; non-idempotent
  commands are never retried automatically. No exactly-once guarantee is
  claimed.

## Repository layout

```
.
├── app/                                    # GUI app (upstream crate)
│   └── src/
│       ├── bin/warpi.rs                    # warpi entry point and channel identity
│       ├── ai/standalone/mod.rs            # standalone config, session mapping, request wiring
│       └── settings_view/local_provider_page.rs   # Settings → Agents → Local Pi provider
├── crates/
│   ├── standalone_agent/                   # Rust adapter (new in this fork)
│   │   ├── src/{provider,secrets,helper,bridge,warp_events,protocol}.rs
│   │   └── tests/{vertical_slice,session_isolation,real_provider}.rs
│   ├── warpui/  crates/warpui_core/        # MIT-licensed UI crates (upstream)
│   └── ...                                 # the rest of the upstream workspace
├── standalone/
│   ├── pi-helper/                          # private Node helper (Pi SDK 0.84.2, Node >= 22.19)
│   │   ├── src/{main,protocol,runtime,workspace-tools}.ts
│   │   ├── test/
│   │   └── package.json  package-lock.json
│   ├── ARCHITECTURE.md                     # ownership, exchange vs run, protocol, provider UI
│   ├── BUILDING.md                         # prerequisites, helper build, config reference
│   ├── SECURITY.md                         # trust boundaries and explicit non-protections
│   ├── PROVIDER_COMPATIBILITY.md           # supported / rejected / unverified
│   ├── NETWORK_DEPENDENCIES.md             # every application-managed egress path
│   ├── VALIDATION.md                       # PASS / FAIL / NOT RUN evidence and risks
│   ├── PLAN.md                             # milestones and remaining work
│   ├── WINDOWS.md                          # Windows status: Linux cross-check PASS for the adapter, GUI needs a real runner
│   ├── REUSE.md  DONOR-ATTRIBUTION.md      # source pins, ported logic, MIT donor notice
│   ├── ci/warpi-windows-job.md             # proposed Windows runner job (not run)
│   ├── fonts/DpQuake/                      # optional bundled fonts + author's licence (dpquake.txt)
│   ├── scripts/install-quake-fonts.sh      # installs the fonts into the user font cache
│   └── evidence/                           # test logs and GUI screenshots
├── packaging/
│   ├── package-warpi.sh                    # no-sudo, no-network dev packaging
│   ├── install.sh                          # installed by the bundle; writes ~/.local/bin/warpi
│   └── config.example.json
├── .github/workflows/warpi-build.yml       # Linux/Windows artifacts (unverified; see the file)
├── script/                                 # upstream build/bundle scripts (not updated for warpi)
├── CONTRIBUTING.md
├── LICENSE-NOTES.md
├── LICENSE-AGPL  LICENSE-MIT               # upstream license texts
├── THIRD_PARTY_LICENSES.txt                # runtime dependency license census
├── Cargo.toml  Cargo.lock  rust-toolchain.toml
└── README.md
```

## Testing

```bash
cargo test -p standalone_agent                  # unit + session-isolation + vertical-slice + stub-helper suites (56 test functions in the current source; see VALIDATION.md for which have been re-run)
cd standalone/pi-helper && npm test             # 14 tests in the current source (13 in the recorded log)
cargo check -p warp --bin warpi --features gui  # app compile
cargo check -p warp --tests --features gui      # app test targets compile

# Opt-in real-provider adapter test (skips without a key; never runs in CI)
WARPI_REAL_PROVIDER_KEY=... cargo test -p standalone_agent --test real_provider
```

The vertical-slice suite runs the real helper against a loopback fixture and
asserts the number and order of provider requests, the request bodies, the
absence of an `Authorization` header for `auth = none`, two-session isolation,
FIFO queueing, cancellation, and provider-failure classification. The
stub-helper suites cover dropped tool results, untranslatable calls, and cancel
settlement. The helper suite covers protocol validation, tool brokering,
compaction settings, `bash_output`, and four end-to-end provider scenarios. The
Rust tests never contact a non-loopback host, except the opt-in `real_provider`
test, which only runs when `WARPI_REAL_PROVIDER_KEY` is set and defaults to
DeepSeek's OpenAI-compatible endpoint (`WARPI_REAL_PROVIDER_BASE_URL`,
`WARPI_REAL_PROVIDER_MODEL` override it). It proves the adapter round trip
end-to-end: prompt → provider tool call → brokered workspace call → resume with
the real tool output → second provider request → final answer containing a
marker that exists only in the test. The tool is executed by the test (the
equivalent of Warp running the approved command).

## Roadmap and known gaps

- **Durable queueing and event-log delivery are not in 0.1.0.** The prompt queue
  is in-memory and lives with the running session, so a restart does not recover
  queued prompts or undelivered events.
  `crates/standalone_agent/src/journal.rs` and `event_log.rs` are merged but have
  no callers yet, and the feature is deferred to 0.1.1. Nothing in 0.1.0 survives
  a restart beyond the conversation → Pi session mapping.
- **Cost and pricing display are not in 0.1.0.** No preset prices ship, so the
  usage footer and context meter show token counts only; estimated USD cost is
  omitted.
- **Real-provider validation**: the adapter-level round trip against DeepSeek
  (`deepseek-flash`, `deepseek-v4-pro`) is **PASS** (top table; rerun with
  `WARPI_REAL_PROVIDER_KEY=... cargo test -p standalone_agent --test real_provider`).
  The same provider flow driven through the GUI is **pending** and not claimed,
  and no coding-performance claim is made. Other hosted providers are
  unverified; the Kimi (Moonshot) preset only prefills an endpoint and suggested
  model ids. `standalone/VALIDATION.md` records the adapter-level PASS and the
  not-run items separately.
- **Diff review**: `write`/`edit` map to `ApplyFileDiffs` and are unit-tested at
  the event level, but the approval/diff flow has not been driven in the GUI.
- **Restart**: the conversation → Pi session mapping is durable; restart/idle
  restore has not been exercised end-to-end in the GUI.
- **Cancellation** is tested in Rust and the helper, including a helper that
  never acknowledges `turn.cancel` (the cancel deadline settles the turn); it has
  not been driven through the GUI. Stopping an already-running command from the
  agent is not wired in v1 (Warp's own block controls still work; `bash_output`
  can only wait).
- **Queueing**: a prompt submitted while a turn is running is queued FIFO and
  runs when the turn settles; the send-now keybinding cancels the running turn —
  including one paused on an approval card (landed 2026-09-23) — and sends the queued
  prompt. If the helper never acknowledges the cancel, the cancel deadline
  settles the turn. The keybinding and hints are implemented but not yet driven
  in the GUI.
- **Profile/model/key changes reopen the session on the next fresh prompt**
  (landed 2026-09-23). A turn or queued prompt that is already live keeps the endpoint it
  started on, so the model chip can still show the new selection while that turn
  finishes. The conversation → Pi session map is written under a lock and through
  a temp-file rename.
- **MCP** is out of scope for v1 and is not advertised to the model.
- **Long-running commands**: `bash_output` bounded polling is implemented
  (`workspace.read_shell_command_output`, 1–120 s, default 30 s, cannot write to
  or stop the command); `bash_write`/`bash_cancel` and file deletion are deferred.
- **Subagents, images, reasoning display, web search, documents, skills,
  artifacts** are not offered in v1.
- **Packaging**: no signed installer or release bundle; the release bundlers
  under `script/` still carry the old `oss` channel case. The included
  `packaging/package-warpi.sh` is a development convenience.
- **Platforms**: Linux x86_64 only. `cargo check -p standalone_agent --target
  x86_64-pc-windows-gnu` **passes**, but the GUI cross-build is blocked by
  native C/asm dependencies (`aws-lc-sys`, SQLite, tree-sitter, …); the Windows
  path is the GitHub Actions job (`.github/workflows/warpi-build.yml`,
  `standalone/ci/warpi-windows-job.md`), which has not been run. macOS is
  entirely unverified. `standalone/WINDOWS.md` has the full probe log.
- **Robustness gaps**: retry handling (the SDK retry layer is disabled at
  session open, so a transient provider failure is terminal), the crash-injection
  matrix, a forced-compaction integration test, idle eviction of helper sessions,
  per-session actors for request interleaving, and a full egress audit of the GUI
  process are all still open. A compaction longer than its 600-second deadline is
  still cancelled, and helper shutdown on app teardown is best-effort (a session
  whose lock is held is skipped; the helper also exits on stdin EOF). See
  `standalone/PLAN.md`.

### Parked ideas (deferred, from a third-party review)

These are parked, not scheduled. The current focus is finishing and shipping the
existing tracks — local UI and observability, subagents, and release
consolidation; durable queueing and event-log delivery are the next track (0.1.1).
This list is a backlog to revisit afterwards. They
come from an internal review of the MIT-licensed
[Jgracier/ClikCode](https://github.com/Jgracier/ClikCode) — recorded as
`standalone/research/clikcode-review.md` in the development tree, which is not
part of this repository. Design inspiration is free to reuse; any copied code
needs the MIT copyright and permission notice recorded in `LICENSE-NOTES.md`
first.

- **Retry recoverable provider errors (deferred).** Today an endpoint error (rate
  limit, busy server, dropped connection) ends the turn as a generic failure. The
  idea is to classify errors by kind or status and retry the recoverable ones
  with bounded backoff, without fighting the existing inactivity and cancellation
  timeouts.
- **Steer a running turn (deferred).** Prompts submitted mid-turn are queued
  today, and the send-now keybinding cancels the running turn and sends
  immediately. The idea is to let a new message join the turn already running,
  taking effect at its next step.
- **Two-stage compaction with an authoritative token count (deferred).** Elide
  old tool output before summarizing, never split a tool call from its result,
  and prefer the endpoint-reported input-token count over local estimates for the
  context and usage display.
- **Per-model context/output defaults (deferred).** A small longest-prefix table
  so new provider presets start with sensible limits the user can still override.
- **Clearer permission-policy wording (deferred).** Reusable ways to phrase and
  structure allow/deny decisions (narrowest-rule suggestions, compound-command
  awareness) as subagent approvals are designed; Warp remains the approval
  authority.
- **Safer helper packaging (deferred).** Build-time assertions and an
  install-and-run test for the Node helper bundle.

## Licensing and attribution

- Upstream Warp code is **AGPL-3.0-only** (`LICENSE-AGPL`), except
  `crates/warpui` and `crates/warpui_core`, which are **MIT** (`LICENSE-MIT`).
- All new fork code (`crates/standalone_agent`, `standalone/**`, the app wiring,
  the packaging scripts) is **AGPL-3.0-only**.
- Behaviour ported from **sasuke39/openwarp** at commit `5045d30` (MIT) was
  rewritten, not copied; the required copyright notice is reproduced in
  `standalone/DONOR-ATTRIBUTION.md`, and `standalone/REUSE.md` lists exactly what
  was ported and every deliberate deviation.
- The Pi coding-agent SDK (`0.84.2`) and typebox (`1.3.7`) are npm dependencies
  under their own licenses (MIT per the pinned packages' metadata).
- The DpQuake/QUAKE2 fonts in `standalone/fonts/DpQuake/` are free to
  redistribute under the author's terms, but `dpquake.txt` **must always ship
  with them** (the author also notes the stylized logos are id Software
  copyright). See `standalone/fonts/DpQuake/README.md`.
- The runtime dependency license census (the Rust normal-edge runtime closure and
  the bundled npm production tree) is `THIRD_PARTY_LICENSES.txt`; the
  authoritative per-crate texts are generated by `cargo about generate` at
  release time.
- Donor attribution is tracked in a register in `LICENSE-NOTES.md`; no donor code
  beyond the openwarp behaviour above has been copied.
- Redistribution obligations are summarised in `LICENSE-NOTES.md`.

## No affiliation

warpi is an independent fork and is **not affiliated with, endorsed by, or
sponsored by Warp or Denver Technologies, Inc.** "Warp" is a trademark of its
owner; this fork keeps its own app identity and does not ship Warp branding as
its own.
