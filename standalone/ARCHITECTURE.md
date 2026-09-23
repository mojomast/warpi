# Architecture (warpi)

warpi: the native Warp agent experience without a Warp account or Warp servers,
driving inference directly to a user-configured OpenAI-compatible endpoint.

```
┌──────────────────────────────────────────────────────────────────────┐
│ Warp native UI + controller + history model + action model           │
│   - approvals, tool cards, diffs, persistence, conversation state    │
│   - executes every workspace side effect                             │
└──────────────▲──────────────────────────────────────┬───────────────┘
               │ warp_multi_agent_api::ResponseEvent   │ ai::Request
               │ (Init / ClientActions / Finished)     │ (task context + inputs)
┌──────────────┴──────────────────────────────────────▼───────────────┐
│ crates/standalone_agent  (thin Rust backend, M1)                    │
│   provider.rs  local profile registry (no secrets)                  │
│   secrets.rs   OS secret store -> in-memory SecretString            │
│   helper.rs    supervised stdio process, bounded frames, stderr     │
│   bridge.rs    session/turn/exchange state machine, tool correlation │
│   warp_events.rs  bridge events -> native Warp protobuf events      │
└──────────────▲──────────────────────────────────────┬───────────────┘
               │ NDJSON frames (v1)                    │
┌──────────────┴──────────────────────────────────────▼───────────────┐
│ standalone/pi-helper  (private Node helper, no listening port)      │
│   Pi coding-agent SDK owns the model loop and the transcript        │
│   every workspace tool is a custom tool that suspends on a promise  │
└──────────────────────────┬──────────────────────────────────────────┘
                           │ HTTPS (Chat Completions, streamed)
                    user-configured endpoint
```

## Ownership split

| Concern | Owner |
| --- | --- |
| Model loop, prompt/tools, transcript, compaction | Pi (helper process) |
| Retries | **None today.** `session.open` disables the SDK retry layer (`retry.enabled: false`, `max_retries: 0`), so a provider failure is terminal for the run. Native-style retry classification is a known gap (`PROVIDER_COMPATIBILITY.md`). |
| Approvals, permissions, workspace execution, diffs, history, persistence | Warp (native controller/action model) |
| Transport, protocol validation, correlation, Warp event shaping | `standalone_agent` |
| Endpoint, model id, limits, credentials | Local standalone configuration |

Neither side duplicates the other's authority: the helper never executes a
workspace side effect, and Warp never decides what the model sees.

## Exchange vs run

This distinction is the core of the integration:

- **Exchange** = one Warp request/response stream (`ResponseStream` in the
  controller). It is opened by `ResponseEvent::Init`, carries any number of
  `ClientActions`, and is closed by `ResponseEvent::Finished`.
- **Run** = one Pi prompt. It spans exchanges. A run is opened by
  `turn.start`, may be suspended any number of times on tool calls, and is
  settled by `turn.completed` / `turn.failed` / `turn.cancelled`.

When Pi reaches a tool call, the helper emits `tool.calls` followed by
`turn.awaiting_tools`. The Rust bridge translates the batch into Warp
`ToolCall` messages and closes the exchange with `StreamFinished(Done)`.
Warp's history model keeps the conversation `InProgress` because the exchange
has actions pending, so the controller does **not** wait on Pi. When Warp has
executed (or rejected) the actions it sends the results in the next request;
the bridge validates them against the suspended batch and sends `turn.resume`,
which resolves the same promises so the same Pi prompt continues.

```
Warp request (user query)  -> exchange E1 -> turn.start   -> Pi prompt P
Pi requests tools          -> tool.calls / turn.awaiting_tools -> E1: Finished(Done), run paused
Warp request (results)     -> exchange E2 -> turn.resume  -> P continues
Pi emits final text        -> deltas/message -> E2: Finished(Done), run settled
```

A run that never requests tools settles inside E1. A rejected tool is a
`turn.resume` with `status: rejected`; Pi receives it as a tool error and
continues (or the user's next message supersedes the run). Prompts submitted while
a run is live are queued rather than interrupting it (see "Prompt queueing and
cancellation" below).

## Turn lifecycle guarantees

Three invariants close the turn states that used to park a Pi run forever
(implemented in `crates/standalone_agent`; the suites are
`tests/tool_result_loss.rs`, `tests/untranslatable_calls.rs`, and
`tests/cancel_settlement.rs`):

1. **Exactly one result per forwarded tool call.** `plan_resume` classifies every
   incoming result as accepted, already-delivered, or unknown. Accepted results
   are sent, and when a resume accepts at least one result, every call still
   pending that the app did not answer is answered with a synthesized cancelled
   result merged into the same `turn.resume` frame, so a dropped or rejected
   action cannot leave Pi waiting on a promise nobody will resolve. Duplicates are
   no-ops, and a batch with no matching results fails the exchange without
   consuming pending state (the caller can retry with the right batch).
2. **Untranslatable calls are answered in place.** `translate_tool_call` failures
   (unknown tool, invalid argument, unsupported filter) become model-visible error
   results. A batch where nothing translates is resumed by the bridge inside the
   same exchange, so the Pi run continues; `Tool::Server` is never emitted.
3. **Cancellation always settles.** `turn.cancel` names the exchange it targets, so
   a queued prompt can be cancelled without touching the running turn and a stale
   cancel is a no-op. `RunCancelled` renders `Finished(Done)` (the proto has no
   cancelled finish reason). If the helper does not terminalize the turn within
   `WARPI_CANCEL_DEADLINE_SECS` (default 5), the watchdog synthesizes the terminal
   event.

The watchdog bounds the other ways a turn can go quiet:

| Condition | Deadline | Env override |
| --- | --- | --- |
| No helper event while the provider works | 120 s | `WARPI_TURN_STALL_TIMEOUT_SECS` |
| Turn suspended on tool calls | 1800 s | `WARPI_PENDING_TOOL_TIMEOUT_SECS` (`0` = no deadline) |
| Helper has not acknowledged `turn.cancel` | 5 s | `WARPI_CANCEL_DEADLINE_SECS` |

Expiry settles the attached exchange (`ProtocolError{timeout}`,
`RunFailed{tool_timeout, retryable: true}`, or `RunCancelled`), drops the turn, and
starts the next queued prompt. `WARPI_PENDING_TOOL_TIMEOUT_SECS=0` restores
native Warp's unbounded approval wait, but it also removes the only automatic
release for a paused standalone turn (see the queueing caveat below).

## Data flow inside `crates/standalone_agent`

1. `provider::ProviderProfile` validates the endpoint: http(s) only, no
   credentials/query/fragment in URLs, full `/chat/completions` suffixes are
   stripped so `/v1` is never duplicated, model ids that look like config
   UUIDs are rejected, limits are checked.
2. `secrets` resolves the profile's credential reference at session-open time.
   `auth = none` resolves nothing and must not put anything on the wire.
3. `helper` spawns `node <helper>/dist/main.js` with an explicit argv, a
   controlled working directory, and an allowlisted environment (`PATH`,
   `LANG`, `LC_ALL`, plus `HOME`/`TMPDIR`/`WARPI_PI_SCRATCH_DIR`/`PI_OFFLINE`
   set to fork-private values). Stdout is parsed as framed protocol; stderr is
   drained independently into a bounded tail.
4. `bridge` keeps per-conversation state: session generation, the current Pi
   turn, the suspended tool calls, and already-delivered tool call ids. Every
   helper event is checked against session + turn + exchange before it can
   reach the UI.
5. `warp_events` renders `ResponseEvent`s exactly as the Warp server would:
   `Init` first, `CreateTask` for a task the client does not yet treat as
   server-backed, `AddMessagesToTask` for the first text delta, then
   `AppendToMessageContent` with the `agent_output.text` field mask, tool calls
   as `RunShellCommand` / `ReadShellCommandOutput` / `ReadFiles` / `FileGlobV2` /
   `Grep` / `ApplyFileDiffs`, and a terminal `StreamFinished`.

## Standalone mode in the Warp app

### Provider configuration and model display

- **Settings page** `app/src/settings_view/local_provider_page.rs` (Settings →
  Agents → **Local Pi provider**): edits the active profile (display name, base
  URL, model id, additional model ids, context/output limits), toggles standalone
  mode, switches authentication between `none` and an API key (written to the OS
  secret store under `warpi/profile/<id>`), creates/deletes profiles, and runs a
  **Test connection** probe against `{base}/models`. Provider presets prefill the
  endpoint and a suggested model for common OpenAI-compatible services (DeepSeek,
  Kimi (Moonshot), OpenAI, OpenRouter, Groq, Mistral, xAI, Together, Fireworks,
  and local Ollama/LM Studio/llama.cpp/vLLM servers); every field stays editable.
  A missing `/models` endpoint reports success-with-note, because v1 supports
  manual model ids.
- **Model picker**: when standalone mode is enabled, `LLMPreferences` carries
  one synthetic `LLMInfo` (`standalone:<profile-id>|<model-id>`) per *enabled*
  model of every configured profile, so the native picker lists all models from
  all profiles. Selecting a standalone model switches the active profile and the
  model id used when a helper session is next opened, and the list refreshes
  immediately after a save or a toggle, without a restart. (The committed picker
  also lists Warp's own model entries next to the standalone ones; a pending
  change hides every cloud-only entry — models and settings sections — while
  standalone mode is enabled. It is uncommitted as of 2026-09-23, so the final
  picker contents are in flux; the GUI screenshot above shows the committed
  behavior.)
- **Open conversations keep their session** (known gap, 2026-09-23):
  `run_exchange` opens the helper session only once per conversation and never
  re-reads the profile while it is open, so a conversation that is already
  running keeps the endpoint, model, key, and working directory it opened with —
  for later turns too, not just the turn in flight. The model chip follows the
  new selection while inference uses the old one. Reopening on a changed
  (profile, model, key, cwd) fingerprint is planned; not implemented.
- **Per-model enable/disable**: the settings page lists every model of the
  active profile with a toggle. Disabled models are persisted in the profile's
  `disabled_models` and never appear in the picker. The model currently in use
  cannot be disabled (the page asks the user to select another first).
- **Conversation identity**: `app/src/ai/standalone/mod.rs` generates a Warp
  task id for brand-new conversations (matching the real server's first
  `CreateTask`) and reuses it for every exchange; `CreateTask` is sent only when
  the client does not already have a server-backed task.

### Prompt queueing and cancellation

Standalone turns cannot be steered, so a prompt submitted while one is running is
queued instead of interrupting it:

- **App side**: standalone conversations default to queue mode
  (`QueuedQueryModel::is_queue_next_prompt_toggle_enabled` returns true when
  standalone mode is enabled, unless the per-conversation toggle overrides it).
  Queued prompts are FIFO and drain through Warp's existing queued-prompt path.
- **Bridge side**: `start_turn` for a conversation that already has a live turn
  pushes a `QueuedTurn` onto that session's `VecDeque`; `finish_turn` immediately
  starts the head of the queue, so acceptance order is execution order. The old
  `SessionBusy` rejection path no longer exists.
- **Send now**: `Ctrl+Alt+Shift+Enter` (`Cmd+Alt+Shift+Enter` on macOS) cancels
  the running turn and submits the head queued prompt. The input's queue hint and
  the queued-prompt panel header advertise the combination; the binding is enabled
  only in standalone mode and only while a sendable head row exists.

**Known caveat (fix in progress, uncommitted as of 2026-09-23)**: when the turn is
paused on an approval card or a command snapshot, the exchange that owns the
cancel task has already closed, so stop / send-now cannot reach the helper. The
queued prompt waits behind the parked turn until the pending-tool deadline expires
it (30 minutes by default; indefinitely with `WARPI_PENDING_TOOL_TIMEOUT_SECS=0`)
or the user answers the card. The bridge-side groundwork for a cancel signal and
an orphan-event channel is in the working tree but is not committed, and the app
does not wire it yet; treat this as pending.

`app/src/ai/standalone/mod.rs`:

- Loads `standalone/config.json` from the fork's private data directory (or
  `WARPI_STANDALONE_CONFIG`), validates the profile, and caches it.
- `RequestParams::new` attaches a `StandaloneRequestConfig` to the request when
  standalone mode is enabled.
- `app/src/ai/agent/api/impl.rs::generate_multi_agent_output` branches to the
  local backend *before* any cloud call. There is no fallback in either
  direction: standalone requests never reach Warp servers, and a failing local
  endpoint is reported as a local error.
- One supervised helper session per conversation; the conversation → Pi session
  file mapping is persisted in `<data dir>/standalone/session-map.json`, and a
  restart resumes the recorded session file. The map is written with an unguarded
  read-modify-write, so two conversations opening their first session at the same
  moment can lose an entry and silently restart on a fresh transcript (known gap,
  2026-09-23).
- `AISettings::is_any_ai_enabled` returns true when standalone mode is enabled,
  so a fresh profile can use agent mode without an account. Every other AI
  surface keeps its cloud gating, and signed-out cloud calls fail at the local
  auth boundary before any HTTP request is attempted.

## Protocol (v1) summary

Newline-delimited JSON, one frame per line, bounded at 4 MiB. Only frames on
stdout; diagnostics on stderr.

Backend → helper: `hello`, `session.open`, `turn.start`, `turn.resume`,
`turn.cancel`, `session.compact`, `shutdown`.

Helper → backend: `hello.ok`, `session.opened`, `turn.started`,
`assistant.delta`, `assistant.message`, `assistant.reasoning`, `tool.calls`,
`turn.awaiting_tools`, `turn.completed`, `turn.cancelling`, `turn.cancelled`,
`turn.failed`, `compaction.started`, `compaction.finished`, `diagnostic`,
`error`, `shutdown.ack`.

Identity: `session_id` (Warp conversation), `generation` (session epoch),
`turn_id` (one Pi prompt), `exchange_id` (one Warp request), `seq` (monotonic
per direction), tool call ids. Frames that open or terminate an exchange carry
a new `exchange_id`; events emitted for an exchange must match the exchange the
backend currently reads. `turn.cancel` carries the exchange id the app wants
cancelled: a queued prompt or the running turn, never a turn that merely replaced
the one the caller was watching. Version mismatches, oversized frames, regressions
in `seq`, duplicate tool call ids, unknown tool names, foreign/stale/duplicate
tool results, and cross-session replies are rejected (see `SECURITY.md`).

## Explicit non-goals (v1)

- No ACP, no plugin marketplace, no service-composition framework.
- No Responses/Anthropic/Messages protocol support (rejected, not emulated).
- No image input claims, no reasoning display, no cost claims.
- No parallel subagents; one active run per conversation, with additional prompts
  queued (FIFO) behind it.
- No background shell control: `bash_write`/`bash_cancel` are not offered.
  `bash_output` only polls a command that is already running; it cannot write to
  it or stop it.
- No SSH/remote workspace execution: unsupported remote actions fail
  explicitly.
