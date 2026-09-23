# Changelog

warpi is a standalone fork of Warp. Its versions are independent of upstream Warp's
date-based tags (`v0.YYYY.MM.DD.HH.MM.channel_NN`). Bump these together:

- `standalone/VERSION` — canonical, used by packaging and docs.
- `app/src/standalone_ui.rs` (`WARPI_VERSION`) — shown in About when no release tag is compiled in.
- `app/Cargo.toml` and `standalone/pi-helper/package.json` — crate/package metadata.

A release build may also set `GIT_RELEASE_TAG` at compile time; when present it takes
precedence over `WARPI_VERSION` in About.

## 0.1.0 — 2026-09-23

First public release. CI produces installable artifacts for **Linux x86_64 and
Windows x86_64** — the Linux `warpi-linux-x86_64.tar.gz` (with a `.sha256`) and
the Windows `WarpiSetup.exe` — each accompanied by a GitHub **build-provenance
attestation** rather than code signing; there is no Authenticode certificate and
no GPG key. The Windows adapter test step is **scoped**: the known-failing
`session_isolation` suite is ignored on Windows while every other adapter failure
still turns the job red. The fixture's root cause remains **unverified**, and the
Windows installer has not been compiled or run locally. See the README's
"Limitations and not-yet-verified".

- Local agent backend against user-configured OpenAI-compatible endpoints, with no Warp
  account and no Warp servers required.
- Provider presets (DeepSeek, Kimi/Moonshot, and other OpenAI-compatible endpoints) plus
  custom endpoints, with per-model enable/disable and a native model picker.
- Brokered tool execution with native Warp approval prompts, and queue-by-default
  mid-turn prompts with cancel-and-send.
- Local session persistence and context compaction, plus a per-response token-usage
  footer and a context-window meter backed by a durable token-usage ledger. Cost and
  pricing display is deferred — no prices ship yet.
- Warp's paid and cloud-only surfaces hidden while standalone mode is active.

Not in 0.1.0: durable prompt queueing and event-log delivery (the merged
`journal`/`event_log` modules have no callers yet; deferred to 0.1.1), cost
estimates, and subagents (present but inert by default).
