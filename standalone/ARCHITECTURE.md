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
| Model loop, prompt/tools, transcript, compaction, retries | Pi (helper process) |
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
continues (or the user's next message supersedes the run).

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
   as `RunShellCommand` / `ReadFiles` / `FileGlobV2` / `Grep` / `ApplyFileDiffs`,
   and a terminal `StreamFinished`.

## Standalone mode in the Warp app

### Provider configuration and model display

- **Settings page** `app/src/settings_view/local_provider_page.rs` (Settings →
  Agents → **Local Pi provider**): edits the active profile (display name, base
  URL, model id, context/output limits), toggles standalone mode, switches
  authentication between `none` and an API key (written to the OS secret store
  under `warpi/profile/<id>`), creates/deletes profiles, and runs a
  **Test connection** probe against `{base}/models`. A missing `/models`
  endpoint reports success-with-note, because v1 supports manual model ids.
- **Model picker**: when standalone mode is enabled, `LLMPreferences` carries a
  synthetic `LLMInfo` for the active profile (`standalone:<profile-id>`), so the
  native model chip and picker show the local model instead of the cloud
  default. Selecting another model changes only the label: standalone routing
  always uses the configured profile. The entry refreshes immediately after a
  save, without a restart.
- **Conversation identity**: `app/src/ai/standalone/mod.rs` generates a Warp
  task id for brand-new conversations (matching the real server's first
  `CreateTask`) and reuses it for every exchange; `CreateTask` is sent only when
  the client does not already have a server-backed task.

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
  file mapping is persisted in
  `<data dir>/standalone/session-map.json`.
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
backend currently reads. Version mismatches, oversized frames, regressions in
`seq`, duplicate tool call ids, unknown tool names, foreign/stale/duplicate
tool results, and cross-session replies are rejected (see `SECURITY.md`).

## Explicit non-goals (v1)

- No ACP, no plugin marketplace, no service-composition framework.
- No Responses/Anthropic/Messages protocol support (rejected, not emulated).
- No image input claims, no reasoning display, no cost claims.
- No parallel subagents; one active run per conversation.
- No SSH/remote workspace execution: unsupported remote actions fail
  explicitly.
