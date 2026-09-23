# Milestone plan and current status

Branch: `standalone-pi-backend` in the fork checkout. Upstream baseline
`71088ba18d27114ccfb358901c66b54220cf30d0`.

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
- Evidence: 22 Rust tests (14 unit + 8 integration) and 13 helper tests,
  including the full round trip against a loopback fixture with the real
  helper, `auth=none` wire assertions, two-session isolation, and a clean
  `cargo check`/`cargo build` of the native GUI binary.

## M2 — Standalone onboarding/settings, credentials, model picker 🚧 partial

Done:
- Local-only config (`<data dir>/standalone/config.json` or
  `WARPOS_STANDALONE_CONFIG`), validation, and a validated single-profile
  registry with credential references.
- Credential resolution through `SecureStorage` into `SecretString`; `auth=none`
  path proven on the wire.
- Native control flow wiring: `RequestParams.standalone`,
  `generate_multi_agent_output` branch, `is_any_ai_enabled` exception,
  per-conversation helper sessions, durable conversation → Pi session file map.

Remaining before this milestone can be called complete:
- In-app settings page for endpoint/profile/credential entry (today the config
  file must be written by hand).
- Native model picker entry for standalone profiles (today the request carries
  the synthetic default model id; the profile's model id is what Pi uses).
- First-run UX for standalone mode (no account, no cloud onboarding).

## M3 — Core tools, approvals, native UI, durable mapping 🚧 partial

Done:
- Six brokered tools mapped to native actions; exact active tool set asserted at
  session open; built-in Pi tools disabled.
- Approvals, diffs, and execution reuse Warp's own path unchanged.
- Durable session mapping (`session-map.json`); restart resumes the exact Pi
  session file. Idle restart is exercised manually (see `VALIDATION.md`).

Remaining:
- MCP via Warp's existing integration (not in v1 by design).
- Reviewed `write`/`edit` UX polish (diffs render through the native path but
  have not been exercised in a real GUI session here).
- Long-running command handoff (`bash_output`/`bash_write`/`bash_cancel`).

## M4 — Cancellation, retries, crashes, compaction, isolation, egress 🚧 partial

Done:
- Cancellation while streaming and while awaiting tools; the helper settles the
  run before accepting the next message (tested).
- Duplicate/foreign/stale result rejection; unknown outcomes surfaced.
- Two independent sessions with different profiles/directories, isolated by
  construction and verified by `two_sessions_with_different_profiles_stay_isolated`.
- Network dependency matrix (`NETWORK_DEPENDENCIES.md`).

Remaining:
- Crash-injection matrix (kill the helper before/after tool execution and
  assert no automatic re-execution).
- Compaction exercised end-to-end (settings derived and unit-tested; a
  forced-compaction integration test is missing).
- Bounded retry exhaustion test at the bridge level.
- Full egress audit of the GUI process.
- Idle eviction: a conversation's helper session stays alive until app exit;
  add an idle timeout/close policy before shipping long-running builds.

## M5 — Package the validated target, evidence 🚧 partial

Done:
- Runnable development build (`target/debug/warp-oss`) verified to launch and
  render natively under Xvfb + Mesa lavapipe; evidence committed in
  `standalone/evidence/`.
- `standalone/BUILDING.md` documents prerequisites, helper build, and config.

Remaining:
- Installer/release packaging; no signed bundle for any platform yet.
- Native GUI agent flow not exercised (input-automation gap); see
  `VALIDATION.md`.
- Platform evidence is Linux x86_64 only in this environment.

## Working agreements

- One writer per shared interface: protocol frames, tool names, and event
  semantics are frozen in `ARCHITECTURE.md` before parallel work.
- Every milestone ends with: relevant `cargo test` run, a local commit, and an
  updated status line in this file.
