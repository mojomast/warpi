# Provider compatibility (warpi)

What v1 supports, what it explicitly rejects, and what remains unverified.
Nothing in this table is inferred from a model name.

## Wire protocol

| Protocol | Status | Notes |
| --- | --- | --- |
| OpenAI Chat Completions (`/chat/completions`, streamed, tool calls) | **Supported (v1)** | The only accepted wire protocol. `WireProtocol::parse` rejects everything else. |
| OpenAI Responses API | **Rejected** | Different protocol; not treated as a synonym. |
| Anthropic Messages | **Rejected** | Different protocol; not treated as a synonym. |

Base URL handling (unit-tested in `crates/standalone_agent/src/provider.rs`):

| User input | Normalised to | Resulting request path |
| --- | --- | --- |
| `https://host` | `https://host` | `/chat/completions` |
| `https://host/v1` | `https://host/v1` | `/v1/chat/completions` |
| `https://host/v1/` | `https://host/v1` | `/v1/chat/completions` |
| `https://host/v1/chat/completions` | `https://host/v1` | `/v1/chat/completions` (no duplication) |

Rejected: non-http(s) schemes, embedded credentials, query strings,
fragments.

## Explicit capability flags

Each flag is opt-in; the default is the conservative behaviour. Unknown values
are never assumed.

| Capability | Default | Flag |
| --- | --- | --- |
| `developer` role instead of `system` | off | `compat.supports_developer_role` |
| `reasoning_effort` parameter | off | `compat.supports_reasoning_effort` |
| Usage in the streamed response | on (SDK default) | `compat.supports_usage_in_streaming` |
| `tool_choice` support | off | `compat.tool_choice` |
| Token limit parameter | `max_tokens` | `compat.max_completion_tokens_field` → `max_completion_tokens` |
| Image input | **off** | `supports_image_input` |
| Reasoning display | **off** | `reasoning` is recorded but thinking tokens are not shown in v1 |
| Context/output limits | required, user-entered | no provider catalog is consulted |

## Tool surface offered to the model

Seven brokered tools; every one maps to a typed Warp action executed by the
native client:

| Model tool | Canonical call | Warp action | Approval |
| --- | --- | --- | --- |
| `bash` | `workspace.shell` | `RunShellCommand` (`risk_category = NONTRIVIAL_LOCAL_CHANGE`, never `is_read_only`) | Warp's command approval; see the `is_risky` note below |
| `bash_output` | `workspace.read_shell_command_output` | `ReadShellCommandOutput` with a 1–120 s wait (default 30) | read permissions; it can only wait, never write to or stop the command |
| `read` | `workspace.read_file` | `ReadFiles` (optional line range) | read permissions |
| `write` | `workspace.write_file` | `ApplyFileDiffs` → `new_files` (overwrite allowed) | diff review |
| `edit` | `workspace.edit_file` | `ApplyFileDiffs` → exact-match `FileDiff` | diff review |
| `glob` | `workspace.glob` | `FileGlobV2` | read permissions |
| `grep` | `workspace.grep` | `Grep` (`ignore_case` becomes `(?i)`; the `glob` filter is rejected) | read permissions |

Rejected rather than approximated: unknown tool names, `edit` with identical
old/new text, `grep` with a `glob` filter, any tool-argument payload over
512 KiB. A rejected tool call becomes a failed tool result for the model, never
a partial execution.

A `bash` call that is still running when Warp stops waiting is **not** a success:
the model gets an error result with the partial output, the command id, and the
non-interactive guidance, and it can call `bash_output` to keep waiting in bounded
steps. Interactive and forever-running commands are not blocked by the harness; it
steers the model away from them with the tool descriptions and a system-prompt
addendum, because it cannot interrupt a command that already blocks the user's
terminal.

**`is_risky` status (2026-09-23)**: `translate_tool_call` currently emits
`is_risky: false`, which makes Warp's `AgentDecides` path auto-execute the command
before the redirection/allowlist gate runs. That is a gap against the intended
model (Pi cannot classify risk, so it must not claim the not-risky shortcut); the
intended value is `is_risky: true`, which routes every Pi shell call through the
same denylist, redirection, allowlist, and read-only checks as a native call the
model marked risky. A fix is in the working tree but not committed as of
2026-09-23; until it lands, treat the redirection gate as not enforced for Pi
shell calls (`SECURITY.md` has the same caveat).

Deferred (not offered in v1): file deletion, writing to or stopping a running
command (`bash_write`/`bash_cancel`), background command management beyond
`bash_output` polling, MCP tools and resources, subagents, computer use, web
search, documents, skills, artifacts.

## "Test connection" semantics

The settings page probes `GET {base}/models` with the profile's authentication
mode and an 8-second timeout.

| Outcome | Reported |
| --- | --- |
| 2xx | reachable; if the body lists models, the count is shown |
| 404 | reachable; `/models` is optional and the model id is configured manually |
| 401/403/5xx | failure with the status code |
| connection/DNS/timeout | failure with the transport error |

## Failure classification

| Bridge failure | Warp finish reason |
| --- | --- |
| `invalid_api_key` / `auth` | `InvalidApiKey` |
| `max_output_tokens`, context overflow | `ReachedMaxTokenLimit` |
| `provider_error`, `timeout`, LLM unavailable | `LlmUnavailable` |
| anything else (protocol, internal) | `InternalError` |

Retries are currently **off**: `session.open` sends `retry.enabled: false` and
`max_retries: 0`, so the backend never multiplies a provider retry and a
non-retryable error is terminal for the run. The helper's `SettingsManager.retry`
path exists but is not exercised by the standalone session; mapping
`RunFailed.retryable` onto a real retry layer is planned, not implemented.

## Compatibility that is **NOT VERIFIED**

- **Adapter level**: the round trip is verified against a real hosted provider
  (DeepSeek `deepseek-flash` and `deepseek-v4-pro`) through
  `crates/standalone_agent/tests/real_provider.rs`. Every other provider, and the
  same DeepSeek flow driven through the GUI, is **NOT RUN**. The Kimi (Moonshot)
  preset only prefills an endpoint and suggested model ids; it is not a
  compatibility claim. See `VALIDATION.md`.
- Redirect behaviour with credentials, proxy environments, and corporate
  middleboxes that rewrite streaming responses.
- Non-UTF8 or unusually fragmented SSE frames beyond the fixture's synthetic
  fragmentation.
- Providers that require `max_completion_tokens`, reject `stream_options`, or
  reject tool schemas containing `additionalProperties` — the flags exist, but
  each provider must be verified by the user.
