# Pi SDK ground truth — `@earendil-works/pi-coding-agent@0.84.2`

Verified against the **installed** package in `<pi-helper checkout>`,
not against older `@mariozechner/*` docs. Date: 2026-09-23, Node `v22.19.0`.

Paths below are relative to
`<pi-helper checkout>/node_modules/@earendil-works/pi-coding-agent/`
unless stated otherwise. Dependency versions: `pi-helper/DEPENDENCY-TREE.md`.

A working smoke test lives at `pi-helper/smoke.mjs` (run with
`node smoke.mjs`); its verified output is pasted in §10.

---

## 0. Entry points and package layout

`package.json`:

```json
"main": "./dist/index.js",
"types": "./dist/index.d.ts",
"bin": { "pi": "dist/cli.js" },
"exports": {
  ".":          { "types": "./dist/index.d.ts", "import": "./dist/index.js" },
  "./rpc-entry":{ "import": "./dist/rpc-entry.js" },
  "./client":   { "types": "./dist/client/index.d.ts", "import": "./dist/client/index.js" }
},
"engines": { "node": ">=22.19.0" }
```

- Prebuilt ESM JS + `.d.ts` ship in `dist/` — usable directly from Node, no build step.
- No global install needed. The CLI (`dist/cli.js`, `bin: pi`) is separate from the SDK.
- `dist/index.d.ts` re-exports the SDK surface, e.g.:
  `createAgentSession`, `createAgentSessionRuntime`, `AgentSessionRuntime`,
  `SessionManager`, `SettingsManager`, `ModelRuntime`, `DefaultResourceLoader`,
  `defineTool`, `createBashTool`/`createReadTool`/…, and the type unions
  `AgentSessionEvent`, `ToolDefinition`, `CreateAgentSessionOptions`, etc.

## 1. Creating a session / `AgentSession`

### 1.1 `createAgentSession()` — the SDK entrypoint

`dist/core/sdk.d.ts:10-56`:

```ts
export interface CreateAgentSessionOptions {
  cwd?: string;                 // default process.cwd()
  agentDir?: string;            // default ~/.pi/agent
  modelRuntime?: ModelRuntime;  // default: runtime using agentDir/auth.json + models.json
  model?: Model<any>;           // default: settings, else first available
  thinkingLevel?: ThinkingLevel;// "off"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max"
  scopedModels?: Array<{ model: Model<any>; thinkingLevel?: ThinkingLevel }>;
  noTools?: "all" | "builtin";
  tools?: string[];             // allowlist
  excludeTools?: string[];      // denylist applied after tools
  customTools?: ToolDefinition[];
  resourceLoader?: ResourceLoader;       // default DefaultResourceLoader
  sessionManager?: SessionManager;       // default SessionManager.create(cwd)
  settingsManager?: SettingsManager;     // default SettingsManager.create(cwd, agentDir)
  sessionStartEvent?: SessionStartEvent;
}
export declare function createAgentSession(
  options?: CreateAgentSessionOptions
): Promise<CreateAgentSessionResult>;
```

`CreateAgentSessionResult` = `{ session: AgentSession; extensionsResult: LoadExtensionsResult; modelFallbackMessage?: string }`.

Only a model runtime is really needed; everything else has defaults
(`dist/core/sdk.js:62-77`). If `model` is omitted the code restores it from the
session, then from settings, then scans available models and returns
`modelFallbackMessage` when none is usable.

### 1.2 `AgentSession` public API

`dist/core/agent-session.d.ts:192+`:

```ts
export declare class AgentSession {
  readonly agent: Agent;                       // @earendil-works/pi-agent-core
  readonly sessionManager: SessionManager;
  readonly settingsManager: SettingsManager;
  subscribe(listener: AgentSessionEventListener): () => void;
  dispose(): void;
  get state(): AgentState;
  get model(): Model<any> | undefined;
  get thinkingLevel(): ThinkingLevel;
  get isStreaming(): boolean;
  get isIdle(): boolean;
  get systemPrompt(): string;
  prompt(text: string, options?: PromptOptions): Promise<void>;   // resolves after the run finishes
  steer(text: string, images?: ImageContent[]): Promise<void>;
  followUp(text: string, images?: ImageContent[]): Promise<void>;
  sendUserMessage(content, options?): Promise<void>;
  clearQueue(): { steering: string[]; followUp: string[] };
  abort(): Promise<void>;        // abort current op, then waits for idle
  waitForIdle(): Promise<void>;
  setModel(model: Model<any>): Promise<void>;
  compact(customInstructions?: string): Promise<CompactionResult>;
  abortCompaction(): void;
  setAutoCompactionEnabled(enabled: boolean): void;
  bindExtensions(bindings: ExtensionBindings): Promise<void>;
  reload(options?): Promise<void>;
}
```

`PromptOptions` (`:153-166`): `expandPromptTemplates?`, `images?`,
`streamingBehavior?: "steer" | "followUp"`, `source?`, `preflightResult?`.
`prompt()` **throws** if called while streaming without `streamingBehavior`.

Session replacement (`/new`, `/resume`, `/fork`, import) is **not** on
`AgentSession`; it lives on `AgentSessionRuntime`
(`dist/core/agent-session-runtime.d.ts`), created via
`createAgentSessionRuntime(factory, { cwd, agentDir, sessionManager })` with
`createAgentSessionServices()` + `createAgentSessionFromServices()`.

## 2. Custom tools — registration, execute, external await

### 2.1 Definition shape

`dist/core/extensions/types.d.ts:344-376`:

```ts
export interface ToolDefinition<TParams extends TSchema = TSchema, TDetails = unknown, TState = any> {
  name: string;                 // used in LLM tool calls
  label: string;                // UI label
  description: string;          // LLM description
  promptSnippet?: string;       // one-line entry in default system prompt
  promptGuidelines?: string[];
  parameters: TParams;          // TypeBox schema
  prepareArguments?: (args: unknown) => Static<TParams>;   // compat shim before validation
  executionMode?: "sequential" | "parallel";
  execute(
    toolCallId: string,
    params: Static<TParams>,
    signal: AbortSignal | undefined,
    onUpdate: AgentToolUpdateCallback<TDetails> | undefined,
    ctx: ExtensionContext
  ): Promise<AgentToolResult<TDetails>>;
  // optional TUI renderers: renderCall / renderResult
}
export declare function defineTool<...>(tool): ToolDefinition<...> & AnyToolDefinition; // :386
```

Return value, `pi-agent-core/dist/types.d.ts:316-334`:

```ts
export interface AgentToolResult<T> {
  content: (TextContent | ImageContent)[];  // went to the model
  details: T;
  usage?: Usage;
  addedToolNames?: string[];
  terminate?: boolean;
}
```

Register through `createAgentSession({ customTools: [tool] })`
(`AgentSessionConfig.customTools`, `dist/core/agent-session.d.ts:121-122`).
Tool names must be listed in `tools` (or not excluded by `excludeTools`) to be
active when an allowlist is used: `_refreshToolRegistry` in
`dist/core/agent-session.js:1940-2010` first builds builtins, then
`definitionRegistry.set(custom.name, …)` and `toolRegistry.set(tool.name, …)`
for custom tools — **custom tools with the same name shadow built-ins**.
(OpenWarp relies on this: it re-registers `bash`, `read`, `write`, `edit`,
`grep`, `find`, `ls` as broker-routed custom tools.)

### 2.2 Awaiting an external result, then resuming the same call

The `execute` promise may stay pending arbitrarily long. Nothing needs to be
registered separately: the session suspends while the tool promise is pending,
and resolving it feeds the result back into the same assistant turn as a
`role: "tool"` message. In-process model:

```ts
execute: async (_toolCallId, params, signal) => {
  const reply = await someExternalBroker.request(params); // Go/Rust side answers later
  return { content: [{ type: "text", text: reply }], details: {} };
}
```

This is exactly what `pi-helper/smoke.mjs` verifies: the first fake-model
response emits a tool call; the tool awaits a promise the test resolves; the
second request to the model contains the tool result, and the assistant
continues in the same run. No re-prompt is required.

If a tool throws (or the external reply is an error), the tool result is
recorded as an error for the model instead of failing the whole session.

## 3. SessionManager — persistence and data directory

`dist/core/session-manager.d.ts` (class at `:184`):

```ts
static create(cwd: string, sessionDir?: string, options?: NewSessionOptions): SessionManager;   // :318
static open(path: string, sessionDir?: string, cwdOverride?: string): SessionManager;           // :325
static continueRecent(cwd: string, sessionDir?: string): SessionManager;                        // :331
static inMemory(cwd?: string, options?: NewSessionOptions): SessionManager;                     // :333
static forkFrom(sourcePath, targetCwd, sessionDir?, options?): SessionManager;                   // :341
static list(cwd, sessionDir?, onProgress?): Promise<SessionInfo[]>;
static listAll(...): Promise<SessionInfo[]>;
```

- Storage: append-only JSONL trees (`id`/`parentId`) with a header; format version
  `CURRENT_SESSION_VERSION = 3` (`:4`). Sessions saved with `sessionFile`,
  `sessionId`, `getSessionDir()` accessors.
- Default dir: `getDefaultSessionDir(cwd, agentDir)` = `<agentDir>/sessions/<encoded-cwd>`
  (`dist/core/session-manager.js:240-254`), i.e. `~/.pi/agent/sessions/...`
  unless `agentDir` is overridden.
- Custom data dir:
  - `SessionManager.create(cwd, "/custom/sessions")`
  - `SessionManager.continueRecent(cwd, "/custom/sessions")` — when an explicit
    dir is passed, `findMostRecentSession` filters by `cwd`
    (`dist/core/session-manager.js:1212-1222`), which is what OpenWarp uses.
  - CLI/env: `PI_CODING_AGENT_SESSION_DIR`, `--session-dir`, and the
    `sessionDir` setting.
  - Global config dir: `getAgentDir()` honours `PI_CODING_AGENT_DIR`, else
    `~/.pi/agent` (`dist/config.js:412-418`; `ENV_AGENT_DIR` constant).

In-memory sessions (`SessionManager.inMemory()`) never touch disk —
`session.sessionFile === undefined` (verified).

## 4. Provider/model config for OpenAI-compatible endpoints

### 4.1 `ModelRuntime`

`dist/core/model-runtime.d.ts:3-19,65,82,97`:

```ts
export interface CreateModelRuntimeOptions {
  credentials?: CredentialStore;   authPath?: string;
  modelsPath?: string | null;      // null = no models.json, in-memory model store
  modelsStore?: ModelsStore;       modelsStorePath?: string;
  allowModelNetwork?: boolean;     // default false
  modelRefreshTimeoutMs?: number;
  catalogBaseUrl?: string;
  signal?: AbortSignal;
  refreshOnCreate?: boolean;       // false = skip initial catalog/availability refresh
}
export declare class ModelRuntime implements Models {
  static create(options?: CreateModelRuntimeOptions): Promise<ModelRuntime>;
  getModel(providerId: string, modelId: string): Model<Api> | undefined;
  getAvailable(providerId?): Promise<readonly Model<Api>[]>;
  hasConfiguredAuth(providerId: string): boolean;
  setRuntimeApiKey(providerId: string, apiKey: string, options?): Promise<void>;
  registerProvider(providerId: string, config: ProviderConfigInput): void;
  refresh(options?: ModelsRefreshOptions): Promise<ModelsRefreshResult>;
}
```

### 4.2 Custom OpenAI-compatible provider

`dist/core/provider-composer.d.ts:13-36`:

```ts
export interface ProviderConfigInput {
  name?: string;
  baseUrl?: string;
  apiKey?: string;              // literal, or "$ENV_VAR", or "!command" (models.json value syntax)
  api?: Api;                    // "openai-completions" is a KnownApi
  streamSimple?: (model, context, options?) => AssistantMessageEventStream;
  headers?: Record<string, string>;
  authHeader?: boolean;
  oauth?: ExtensionOAuthConfig;
  models?: Array<{
    id: string; name: string; api?: Api; baseUrl?: string;
    reasoning: boolean;
    thinkingLevelMap?: Model<Api>["thinkingLevelMap"];
    input: ("text" | "image")[];
    cost: Model<Api>["cost"];
    contextWindow: number;
    maxTokens: number;
    samplingParams?: Record<string, unknown>;
    headers?: Record<string, string>;
    compat?: Model<Api>["compat"];
  }>;
  refreshModels?(context): Promise<NonNullable<ProviderConfigInput["models"]>>;
}
```

Verified working registration (model + auth + limits) from `smoke.mjs`:

```ts
const modelRuntime = await ModelRuntime.create({ modelsPath: null, refreshOnCreate: false });
modelRuntime.registerProvider("warp-smoke", {
  name: "Smoke OpenAI-compatible",
  baseUrl: `http://127.0.0.1:${port}/v1`,
  apiKey: "sk-smoke",
  api: "openai-completions",
  models: [{
    id: "smoke-model", name: "smoke-model", reasoning: false,
    input: ["text", "image"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: 128000, maxTokens: 16384,
    compat: { supportsDeveloperRole: false, supportsReasoningEffort: false },
  }],
});
const model = modelRuntime.getModel("warp-smoke", "smoke-model");
```

`Model` fields (`pi-ai/dist/types.d.ts:670-691`): `id, name, api, provider,
baseUrl, reasoning, thinkingLevelMap?, input, cost, contextWindow, maxTokens,
samplingParams?, headers?, compat?`. `KnownApi` includes
`"openai-completions" | "openai-responses" | "anthropic-messages" | …`
(`pi-ai/dist/types.d.ts:15`). `compat` for `openai-completions` is
`OpenAICompletionsCompat` (`pi-ai/dist/types.d.ts:448-468`:
`supportsStore`, `supportsDeveloperRole`, `supportsReasoningEffort`,
`supportsUsageInStreaming`, `supportsFinishReason`, `maxTokensField`,
`requiresToolResultName`, …).

- `maxTokens` maps into the provider request's max-token field;
  `contextWindow` drives compaction thresholds.
- Per-model `baseUrl`/`headers` override the provider-level ones.
- API keys also resolve through `auth.json`, env vars
  (`OPENAI_API_KEY`, …), `models.json` (`apiKey`, `$ENV`, `!command`), and
  `setRuntimeApiKey()` (not persisted). Precedence is documented in `docs/sdk.md`.
- Alternative to code: `models.json` with a provider entry
  (`{ "providers": { "my": { "baseUrl": …, "api": "openai-completions",
  "apiKey": "$MY_KEY", "models": [...] } } }`); `ModelRuntime.create({ modelsPath })`
  honours it. `docs/custom-provider.md` covers the extension
  `pi.registerProvider()` route.
- Note: in `pi-ai@0.84.2` the old static `import { getModel } from
  "@earendil-works/pi-ai"` is **not exported at top level** anymore (verified in
  Node: `typeof getModel === "undefined"`); it exists at
  `@earendil-works/pi-ai/compat` and is deprecated. Prefer
  `modelRuntime.getModel(provider, id)`.

## 5. Settings and resource loading; disabling extras

`dist/core/resource-loader.d.ts:67-119`:

```ts
export interface DefaultResourceLoaderOptions {
  cwd: string; agentDir: string;
  settingsManager?: SettingsManager; eventBus?: EventBus;
  additionalExtensionPaths?: string[];
  additionalSkillPaths?: string[]; additionalPromptTemplatePaths?: string[];
  additionalThemePaths?: string[];
  extensionFactories?: InlineExtension[];
  noExtensions?: boolean; noSkills?: boolean; noPromptTemplates?: boolean;
  noThemes?: boolean;
  noContextFiles?: boolean;          // disables AGENTS.md discovery
  systemPrompt?: string; appendSystemPrompt?: string[];
  agentsFilesOverride?: (...) => ...; systemPromptOverride?: (base) => string | undefined;
  // skillsOverride / promptsOverride / themesOverride / extensionsOverride
}
```

- **AGENTS.md**: loaded by `DefaultResourceLoader` from cwd upward plus
  `agentDir/AGENTS.md`; set `noContextFiles: true` to disable. OpenWarp does this
  and then passes `appendSystemPrompt: [request.system_prompt]`.
- **Extensions**: `noExtensions: true` drops `.pi/extensions`, `agentDir/extensions`
  and settings paths. Inline factories (`extensionFactories`) still run unless
  omitted. Project trust (`reload({ resolveProjectTrust })`) gates project-local
  extensions/packages; OpenWarp resolves trust from `PI_ENABLE_EXTENSIONS`.
- **Packages**: `Settings.packages: PackageSource[]`
  (`settings-manager.d.ts:58-65,90`). Caution: `DefaultResourceLoader.reload()`
  always calls `packageManager.resolve()`
  (`dist/core/resource-loader.js:275`), and with no `onMissing` callback the
  package manager **installs missing npm/git packages** unless
  `PI_OFFLINE` is set (`dist/core/package-manager.js:981-996`). Keep
  `packages: []` (or `SettingsManager.inMemory`) and/or set `PI_OFFLINE=1`
  for a hermetic sidecar.
- **Auto-update**: only the CLI has update logic (`pi update …`,
  `package-manager-cli.js:416-428` hits `https://pi.dev/api/latest-version`).
  Nothing in `createAgentSession`/`ModelRuntime.create` performs version checks
  or self-updates. `PI_OFFLINE`/`--offline` also sets `PI_SKIP_VERSION_CHECK`.
- **Telemetry**: `Settings.enableInstallTelemetry` defaults to `true`
  (`dist/core/settings-manager.js:655`); in this package it only gates
  provider-attribution headers for known third-party hosts
  (`dist/core/provider-attribution.js`: OpenRouter `HTTP-Referer: https://pi.dev`,
  NVIDIA NIM `X-BILLING-INVOKE-ORIGIN: Pi`, Cloudflare `User-Agent:
  pi-coding-agent`, OpenCode session headers). No install-telemetry network
  sender was found in the installed `dist/`. Third-party hosts only — a custom
  `baseUrl` gets no attribution headers. `PI_TELEMETRY=0` disables the flag.
- `SettingsManager` (`settings-manager.d.ts:140-160`):
  `create(cwd, agentDir?, options?)`, `inMemory(settings?, options?)`,
  `fromStorage(storage)`, `applyOverrides()`, `flush()`, `drainErrors()`.
  Persistence: `~/.pi/agent/settings.json` + `<cwd>/.pi/settings.json`.
  Relevant settings: `compaction.enabled/reserveTokens/keepRecentTokens`,
  `retry.enabled/maxRetries/baseDelayMs`, `defaultProvider`, `defaultModel`,
  `defaultThinkingLevel`, `defaultTools`, `sessionDir`, `packages`, `extensions`.

## 6. Streaming events

`AgentSessionEvent` (`dist/core/agent-session.d.ts:40-107`) = the agent-core
`AgentEvent` union minus the old `agent_end` shape, plus session events:

- `agent_start`, `agent_end { messages, willRetry }`, `agent_settled`
- `turn_start`, `turn_end { message, toolResults }`
- `message_start { message }`, `message_update { message, assistantMessageEvent }`,
  `message_end { message }`
- `tool_execution_start { toolCallId, toolName, args }`,
  `tool_execution_update { …, partialResult }`,
  `tool_execution_end { …, result, isError }`
- `queue_update { steering, followUp }`,
  `compaction_start { reason }`, `compaction_end { reason, result, aborted, willRetry, errorMessage? }`
- `auto_retry_start/end`, `summarization_retry_*`
- `entry_appended`, `session_info_changed`, `thinking_level_changed`,
  `bash_execution_update`

`assistantMessageEvent` is the pi-ai `AssistantMessageEvent`; text deltas:

```ts
session.subscribe((event) => {
  if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
    process.stdout.write(event.assistantMessageEvent.delta);
  }
});
```

Usage/cost are not separate events — read them from the final assistant
`message_end`/`turn_end` message (`usage.{input,output,cacheRead,cacheWrite,reasoning,totalTokens,cost}`
and `stopReason`). Verified in `smoke.mjs`:

```json
"usage":{"input":20,"output":3,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":23,
         "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
"stopReason":"stop"
```

`subscribe()` returns an unsubscribe function. `session.agent` exposes the raw
`Agent` (`subscribe`, `state`, `signal`, `abort()`, `waitForIdle()`).

## 7. Cancellation / abort

- `session.abort(): Promise<void>` — aborts the current run and waits for idle.
- `session.waitForIdle()` — resolves after the run and awaited listeners settle.
- The agent-core `Agent.abort()` calls the run's `AbortController.abort()`
  (`pi-agent-core/dist/agent.js:202-204`); the same `AbortSignal` is passed as
  the 3rd argument of every tool `execute`, so a pending external await can be
  rejected by listening to `signal`.
- Verified: while a prompt was blocked on a hanging stream, `session.abort()`
  made `prompt()` **resolve** (not reject) and `session.isStreaming` became
  `false`. The aborted assistant message carries `stopReason: "aborted"` in
  general (OpenWarp checks `stopReason === "error" | "aborted" | "length"`).
- `session.abortCompaction()` cancels compaction separately.

## 8. SDK vs CLI / RPC-only APIs

- Documented SDK entrypoint: `createAgentSession()` (plus
  `createAgentSessionServices` / `createAgentSessionFromServices` /
  `createAgentSessionRuntime` for cwd/session replacement). `docs/sdk.md` exists
  in the package and matches the `.d.ts` (modulo the stale `getModel` import).
- The high-level modes are exported but CLI/TUI-oriented: `main()` (dist/main.js),
  `InteractiveMode`, `runPrintMode`, `runRpcMode`, `RpcClient`.
- `runRpcMode` + `dist/rpc-entry.js` implement a JSON-RPC-over-stdio protocol
  (`docs/rpc.md`) with commands `prompt`, `steer`, `follow_up`, `abort`,
  `new_session`, `get_state`, `get_messages`, `set_model`, `compact`,
  `set_auto_compaction`, `switch_session`/`fork`/`clone`, `get_entries`/`get_tree`,
  etc. Events mirror the agent stream. This is the language-agnostic path if we
  do not want an in-process SDK.
- The CLI can be embedded as a subprocess: `pi --mode rpc --no-session`
  (strict JSONL; `\n` only — Node `readline` is explicitly not protocol compliant).

## 8b. `@earendil-works/pi-client` / `pi-protocol` (what they actually are)

- `pi-client@0.84.2` is a **transport-neutral client for remote pi sessions**
  (`.../node_modules/@earendil-works/pi-client/dist/index.d.ts`):

  ```ts
  export { PiClient } from "./client.ts";
  export type { AcquireSessionOptions, PiSessionHandle, SessionLease, SessionLeaseMode } from "./session-handle.ts";
  ```

  `PiClient` needs a caller-supplied `ByteTransportFactory`
  (`PiClientOptions = { transportFactory; maxFrameLength?; onListenerError? }`,
  from `pi-client/dist/types.d.ts`) and exchanges
  length-prefixed CBOR messages. It exposes
  `connect/reconnect/disconnect/listSessions/createSession/attachSession/acquireSession/subscribe/onEvent`.
  A `SessionLease` (`PiSessionHandle`) has `prompt/steer/abort/setModel/setThinking`,
  snapshots and `AsyncDisposable`.

- `pi-protocol@0.84.2` defines that wire protocol with TypeBox schemas
  (`.../pi-protocol/dist/schemas.d.ts`): `PROTOCOL_VERSION = 1`,
  commands `list | create | attach | detach | prompt | steer | abort |
  set_model | set_thinking` (`CommandSchema`, `:1779-1819`), server events
  `server_snapshot | session_snapshot | …` with revisions/transcript
  (`ServerEventSchema`, `:6174+`). Message shapes include ModelMetadata with
  `provider/id/api/contextWindow/maxTokens/authenticated`, and
  `SessionPhase = idle|turn|compaction|branch_summary|retry`.

- Caveat: the coding-agent package's `./client` export is **not** `PiClient`; it
  re-exports a UI-facing `RemoteSession` wrapper
  (`dist/client/index.d.ts`, `dist/client/remote-session.d.ts`) that builds on
  `PiClient`. The coding-agent `dist/` ships **no server
  implementation** for the CBOR transport (only `dist/rpc-entry.js`, i.e.
  `pi --mode rpc` over stdio JSON-RPC, and `dist/server/create-harness.js`).
  So `pi-client` is only useful if we also run/implement the matching pi server;
  for embedding, the in-process SDK or `runRpcMode` are the practical paths.

## 9. Prebuilt JS / global install / runtime network

- Ships prebuilt `dist/*.js` (ESM) + `.d.ts`; runs on Node ≥ 22.19 with no build.
- No global install required; a local `npm install` is enough.
- Network at runtime is off by default **if** you avoid the triggers:
  `ModelRuntime.create({ allowModelNetwork: false })` (default) and
  `refreshOnCreate: false` skip catalog refreshes; static/registered providers
  work offline. `models-store.json` caching is skipped entirely with
  `modelsPath: null` (in-memory store).
- Residual network triggers to know about: version check and self-update (CLI
  only), package auto-install from configured `packages`
  (SDK too — see §5), provider-attribution headers for known third-party hosts,
  and obviously the model endpoint itself. `PI_OFFLINE=1` shuts all of the
  Pi-side ones off.

## 10. Verified smoke-test output (evidence)

`node smoke.mjs` (source in `pi-helper/smoke.mjs`), fake OpenAI-compatible SSE
server, custom tool `probe` awaiting an external promise:

```
MODEL: {"provider":"warp-smoke","api":"openai-completions","baseUrl":"http://127.0.0.1:37271/v1"}
MODEL_FALLBACK: undefined
SESSION_FILE(inMemory): undefined
EVENTS_AFTER_TURN1: agent_start,turn_start,message_start,message_end,message_start,message_update:toolcall_start,message_update:toolcall_delta,message_update:toolcall_end,message_end,tool_execution_start,tool_execution_update,tool_execution_end,message_start,message_end,turn_end,turn_start,message_start,message_update:text_start,message_update:text_delta,message_update:text_end,message_end,turn_end,agent_end,agent_settled
TOOL_EXEC_COUNT: 1 SIGNAL: true
TOOL_RESULT_CONTENT: [{"type":"text","text":"external:broker-reply:external:ctx=string"}]
LAST_ASSISTANT: {"stopReason":"stop","usage":{"input":20,"output":3,...},"api":"openai-completions","provider":"warp-smoke","model":"smoke-model"}
SECOND_REQUEST_TAIL: [{"role":"assistant","content":null,"tool_calls":[{"id":"call-1",...}]},{"role":"tool","content":"external:broker-reply:external:ctx=string","tool_call_id":"call-1"}]
ABORT_OUTCOME: undefined
IS_STREAMING_AFTER_ABORT: false
SMOKE_OK
```

## Open questions

1. **Stability of `ProviderConfigInput`**: `registerProvider()` is typed
   `(providerId: string, config: ProviderConfigInput) => void`; the docs also
   advertise `pi.registerProvider(createProvider(...))` inside extensions. Which
   form is the long-term canonical one for an embedding app is unclear; the
   `ProviderConfigInput` form works and is what OpenWarp pins.
2. **API key env fallback for custom providers**: `apiKey` accepts `$ENV`
   references (resolved via `resolve-config-value`), but it is not documented
   whether an unregistered provider falls back to a generic env var. We pass the
   literal value.
3. **`agent_settled` vs `agent_end` semantics**: `agent_settled` is emitted after
   awaited listeners settle, but it is not clear whether `session.prompt()`
   resolves before or after `agent_settled` in all cases; OpenWarp relies only on
   `runToken`/`waitForIdle` instead.
4. **Abort error surface**: in the smoke test `prompt()` resolved after
   `abort()`. Whether it can reject for some providers/phases (network error vs
   abort) is not specified; callers should treat both as terminal.
5. **Custom tool `ctx` type**: `ExtensionContext` is passed by the session; for
   pure SDK tools only `cwd` is obviously meaningful. No dedicated documented
   "SDK tool context" type exists.
6. **Package auto-install**: `DefaultResourceLoader.reload()` can install
   configured packages even in SDK usage when `PI_OFFLINE` is unset. The only
   reliable switch is avoiding `packages`/`extensions` config or `PI_OFFLINE`.
7. **`sessionDir` collision**: `SessionManager.continueRecent(cwd, dir)` filters
   by cwd only when `dir` differs from the default dir; OpenWarp gives each
   conversation its own hashed dir, so "continue" starts fresh when no file
   exists. Fine for its design, but the semantics should be re-checked if reused.
8. **pi-ai top-level exports changed**: docs still show
   `import { getModel } from "@earendil-works/pi-ai"`; in 0.84.2 that export
   moved to `/compat` and is deprecated. Any saved OpenWarp-adjacent snippets
   using it will break.
