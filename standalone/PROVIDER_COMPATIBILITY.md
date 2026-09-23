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

Six brokered tools; every one maps to a typed Warp action executed by the
native client:

| Model tool | Canonical call | Warp action | Approval |
| --- | --- | --- | --- |
| `bash` | `workspace.shell` | `RunShellCommand` (`risk_category = NONTRIVIAL_LOCAL_CHANGE`, never `is_read_only`) | Warp's command approval |
| `read` | `workspace.read_file` | `ReadFiles` (optional line range) | read permissions |
| `write` | `workspace.write_file` | `ApplyFileDiffs` → `new_files` (overwrite allowed) | diff review |
| `edit` | `workspace.edit_file` | `ApplyFileDiffs` → exact-match `FileDiff` | diff review |
| `glob` | `workspace.glob` | `FileGlobV2` | read permissions |
| `grep` | `workspace.grep` | `Grep` (`ignore_case` becomes `(?i)`; the `glob` filter is rejected) | read permissions |

Rejected rather than approximated: unknown tool names, `edit` with identical
old/new text, `grep` with a `glob` filter, any tool-argument payload over
512 KiB. A rejected tool call becomes a failed tool result for the model, never
a partial execution.

Deferred (not offered in v1): file deletion, background/long-running command
control, MCP tools and resources, subagents, computer use, web search,
documents, skills, artifacts.

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

Retries are configured once (helper-side `SettingsManager.retry`); the backend
never multiplies them. A non-retryable error is terminal for the exchange and
the run.

## Compatibility that is **NOT VERIFIED**

- No real provider has been exercised in this audit environment. All provider
  tests use the deterministic loopback fixture; treat real-provider behaviour
  as **NOT RUN**. See `VALIDATION.md`.
- Redirect behaviour with credentials, proxy environments, and corporate
  middleboxes that rewrite streaming responses.
- Non-UTF8 or unusually fragmented SSE frames beyond the fixture's synthetic
  fragmentation.
- Providers that require `max_completion_tokens`, reject `stream_options`, or
  reject tool schemas containing `additionalProperties` — the flags exist, but
  each provider must be verified by the user.
