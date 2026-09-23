# Security model (warpi)

Honest statement of what is and is not protected. The Pi helper is **not** a
sandbox, and nothing here claims otherwise.

## Trust boundaries

1. **Warp UI/user → adapter.** The user approves actions; the model cannot.
   A model-provided `is_read_only`/`is_risky`/`risk_category` value is never
   treated as authorization: the adapter always labels shell tool calls
   `NontrivialLocalChange` and never sets `is_read_only`. **Current gap
   (2026-09-23):** the adapter emits `is_risky: false`, and that value
   short-circuits Warp's `AgentDecides` path to `Allowed(AgentDecided)` before
   the redirection/allowlist gate runs. The intended value is `is_risky: true`
   (Pi cannot classify risk, so it must not claim the not-risky shortcut), which
   routes every Pi shell call through the denylist, redirection, allowlist, and
   read-only checks. A fix is in the working tree but not committed; until it
   lands, the redirection gate is not enforced for Pi shell calls.
2. **Adapter → provider endpoint.** One configured origin per profile. TLS is
   required by the URL rules for public hosts (http is allowed only for
   loopback/private endpoints the user typed explicitly); certificate
   validation is never disabled globally.
3. **Helper process.** Launched with an explicit executable and argv, a
   controlled working directory, and an allowlisted environment. It has no
   listening socket, no global Pi installation, no ambient extensions, and no
   access to the user's `~/.pi`.
4. **Model output.** Treated as untrusted input: tool names must be in the
   active set, arguments are schema-validated by the SDK before the broker
   sees them, unknown names/arguments fail closed, and streamed JSON is never
   executed partially.

## Credentials

- References are stored in configuration (`CredentialRef::SecretStore { key }`);
  the secret itself lives in the OS secret store (macOS Keychain, Windows
  DPAPI, Linux Secret Service). On Linux without a Secret Service provider
  (headless boxes, bare X sessions), Warp's file fallback keeps it in an
  AES-256-GCM-encrypted file under the application state directory with mode
  `0600`; a nested credential key such as `warpi/profile/<id>` creates the
  intermediate directory and stays owner-only (regression-tested). The secret is
  read into an in-memory `SecretString` that redacts itself in `Debug`/`Display`
  and zeroizes on drop, and it never enters the config file.
- The key is written by the settings page (to the OS secret store, or to the
  owner-only file fallback on Linux without a Secret Service) and travels to the
  helper only inside the private stdio `session.open` frame. That frame is never
  persisted; the helper's session files contain the model transcript and
  provider/model ids, not the key.
- `auth = none` sends no `Authorization` header. Pi's SDK requires a configured
  credential before it will run a turn, so the helper registers a non-secret
  placeholder and strips `Authorization`/`cf-aig-authorization` at the HTTP
  boundary for that profile's origin. The integration test asserts both halves:
  no header reaches the fixture server, and the placeholder never appears on
  the wire.
- Redirects: the OpenAI client used by Pi follows redirects; this fork does not
  add credentials to requests it constructs, and `auth = none` profiles have
  none to forward. Cross-origin credential forwarding for `api_key` profiles is
  therefore a property of the vendored client, not of this adapter; a redirect
  test is **NOT RUN**.
- Profiles and keys are snapshots taken when a session opens, and a session
  stays open for the life of its conversation: changing the profile or rotating
  the key does not re-key an already-open conversation (see the known gap in
  `ARCHITECTURE.md`). Each session gets its own `ModelRuntime` instance, so two
  sessions with different endpoints/keys cannot share a client.

## Protocol validation

Rejected, with a typed error, before any state changes:

- protocol version mismatch, non-JSON frames, non-object frames;
- frames over 4 MiB, identifiers over 256 bytes;
- `seq` regressions in either direction;
- session/turn/exchange/generation mismatches (stale events are dropped, not
  delivered);
- duplicate tool call ids in a batch;
- tool results for unknown, already-delivered, or foreign tool calls (surfaced
  as `unknown_tool_result`, never guessed);
- empty `turn.awaiting_tools` batches;
- unknown tool names and unsupported arguments (fail closed);
- tool argument payloads over 512 KiB, tool result content over 4 MiB, event
  text over 1 MiB (truncated with an explicit marker). Per-result truncation is
  enforced; the aggregate `turn.resume` frame for a batch of large results can
  still exceed the 4 MiB frame bound and fail to send (a batch of maximal reads
  is the trigger). A fix that bounds the whole batch and truncates with a marker
  is in the working tree but not committed as of 2026-09-23.

## Approval, execution, and side effects

- Approvals are Warp's own: the adapter only emits typed actions. Unknown or
  invalid actions never reach the executor.
- Allow / reject / cancel / error stay distinct at the executor, but the
  distinction is not yet delivered to the model for user rejections
  (2026-09-23): clicking Reject cancels the pending action locally
  (`ManuallyCancelled`), and the bridge currently synthesizes a generic error
  result that even suggests running the command again. A `status: rejected`
  result is produced only for a **denylisted** command (the
  `PermissionDenied` path); the glue that would record a UI rejection and
  answer it with `Rejected` is in the working tree but not committed. Until it
  lands, treat the model-visible rejection status as unreliable. Rejecting never
  deadlocks the run: the user's next message supersedes it.
- One owner of side effects: Pi's built-in `read`/`bash`/`write`/`edit` tools
  are disabled (`noTools: "all"` + an explicit allowlist); only the seven
  brokered custom tools are active (`bash`, `bash_output`, `read`, `write`,
  `edit`, `glob`, `grep`), and the helper asserts the exact active set at session
  open. `bash_output` only waits for an already-running command; it cannot write
  to it or stop it.
- A crash after an effect but before the result is recorded leaves the outcome
  **unknown**. The protocol reports the failure and never automatically retries
  a non-idempotent command. No exactly-once guarantee is claimed.
- Unknown outcomes are surfaced, never repaired by fabricating a success: a call
  the app did not answer is closed with a synthesized cancelled error (so Pi is
  never left waiting on a promise nobody resolves), and the turn's unresolved
  ids are logged when it settles.

## What this design does not do

- It does not sandbox the helper. A malicious model can still ask Warp to run a
  command; Warp's approval UX decides. The helper process has the same user
  privileges as Warp.
- It does not prevent the model from attempting prompt injection through file
  contents; repository instructions cannot change endpoints, executables,
  authentication, or trust policy because those live in local configuration
  that is only read at startup.
- It does not encrypt the helper's session files. They live in the fork's
  private data directory with owner-only permissions and contain the model
  transcript. Keep the data directory on an encrypted volume if the transcript
  is sensitive.
- Remote/SSH workspaces are explicitly unsupported: the adapter refuses to run
  a remote session's requested actions locally and reports the failure.
