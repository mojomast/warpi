# Network dependencies

Inventory of application-managed network paths, what the standalone fork does
with each, and how it is tested. "Disabled" means the code path is not
reachable in standalone mode; "unreachable without an account" means the
request fails at the local auth boundary before any socket is opened.

## Matrix

| # | Path | Source | Standalone behaviour | Evidence |
| --- | --- | --- | --- | --- |
| 1 | Agent inference (`/ai/multi-agent`) | `app/src/ai/agent/api/impl.rs`, `crates/warp_multi_agent_client` | **Replaced.** Branch to `standalone_agent` happens before any cloud call. No fallback. | `crates/standalone_agent/tests/vertical_slice.rs` (two provider requests, none to Warp); `app/src/ai/standalone/mod.rs` |
| 2 | Passive suggestions (`/ai/passive-suggestions`) | `app/src/ai/blocklist/passive_suggestions/maa.rs` | Not routed locally. Reachable only if the user enables passive AI; requires an access token, which a signed-out profile does not have. | `AuthSession::get_or_refresh_access_token` bails locally when credentials are `None` (`crates/warp_server_client/src/auth/session.rs:97`) |
| 3 | Auth: anonymous user, Firebase tokens, device flow | `crates/warp_server_client/src/auth/*`, `app/src/auth/auth_manager.rs` | Not used in standalone mode; no account is created or required. | `AISettings::is_any_ai_enabled` standalone exception; no login UI needed |
| 4 | GraphQL API (workspaces, teams, settings sync) | `crates/graphql/src/client.rs` | Unreachable without an account; standalone configuration is a local file. | local config path in `app/src/ai/standalone/mod.rs` |
| 5 | Billing / credits | `app/src/server/server_api/workspace.rs`, `billing_workspace_settings.rs` | Unreachable without an account; no purchase/credit UI is required to run. | n/a (no account) |
| 6 | Conversations / Drive / sharing | `crates/warp_graphql` queries, `ServerApi::get_warp_drive_updates` | Unreachable without an account. Conversation history is local (`crates/persistence`, SQLite). | existing persistence tests; no standalone change |
| 7 | Codebase indexing / embeddings | `crates/ai/src/index/full_source_code_embedding`, `ServerApi::generate_code_embeddings` | Feature-flagged off by default; requires an account if enabled. | `FeatureFlag::FullSourceCodeEmbedding` not in OSS release flags |
| 8 | Telemetry (RudderStack) | `app/src/server/telemetry/mod.rs` | Disabled by channel configuration (`telemetry_config: None` for OSS). | `crates/warp_core/src/channel/state.rs::is_telemetry_available` |
| 9 | Crash reporting (Sentry) | `app/src/crash_reporting/mod.rs` | Disabled by channel configuration (`crash_reporting_config: None`). | `is_crash_reporting_available` |
| 10 | Update checks / downloads | `app/src/autoupdate/*` | Disabled for OSS (`autoupdate_config: None`); no release download URL is set for the fork. | `is_autoupdate_available` / OSS `unreachable!` guards |
| 11 | Model catalog | `ServerApi::get_feature_model_choices` / `freeAvailableModels` | Not required: the model id is typed by the user; the helper never fetches a catalog (`modelsPath: null`, `allowModelNetwork: false`). | `standalone/pi-helper/src/runtime.ts`; `ModelRuntime.create` options |
| 12 | MCP servers (user-configured) | `crates/mcp/src/runtime.rs` | Out of scope for v1. MCP is not advertised to the model in standalone mode. | `get_supported_tools` list to be narrowed in M2 (see status) |
| 13 | Remote server download (`/download/cli`) | `crates/remote_server/src/setup.rs` | Unreachable: SSH workspaces are unsupported and refused. | `SECURITY.md` |
| 14 | LSP binary installs (GitHub) | `crates/lsp/src/install.rs` | Only on demand; unrelated to agent inference. | n/a |
| 15 | Node/npm downloads | `crates/node_runtime` | Not used by the standalone helper; the helper is bundled and launched with the system/runtime `node` chosen at install time. | `standalone/pi-helper` ships `dist/` |

## Standalone helper egress

| Destination | When | Notes |
| --- | --- | --- |
| Configured provider base URL | Every turn | The only inference destination. Origin is derived from the profile; the header-stripping policy is keyed on that origin. |
| Nothing else | – | `PI_OFFLINE=1` blocks the SDK's package/catalog manager; `noExtensions`, `noSkills`, `noPromptTemplates`, `noThemes`, `noContextFiles` (unless explicitly enabled) prevent resource discovery; `enableInstallTelemetry: false`, `enableAnalytics: false`. |

## Test coverage of egress

- The vertical-slice test runs the real helper against a **loopback fixture
  server** and asserts the number and order of requests, the request bodies,
  and the absence of an `Authorization` header for `auth = none`.
- The Rust test suite never contacts a non-loopback host.
- The helper never listens on a port; there is no HTTP server in either
  component (verified by protocol design and by the absence of any bind call in
  the helper source).
- **NOT RUN**: a full packet-capture/egress audit of the GUI binary under
  Xvfb. The matrix above is a source-level inventory, and cloud-only paths are
  additionally gated by the local auth boundary.
