# Changelog

warpi is a standalone fork of Warp. Its versions are independent of upstream Warp's
date-based tags (`v0.YYYY.MM.DD.HH.MM.channel_NN`). Bump these together:

- `standalone/VERSION` — canonical, used by packaging and docs.
- `app/src/standalone_ui.rs` (`WARPI_VERSION`) — shown in About when no release tag is compiled in.
- `app/Cargo.toml` and `standalone/pi-helper/package.json` — crate/package metadata.

A release build may also set `GIT_RELEASE_TAG` at compile time; when present it takes
precedence over `WARPI_VERSION` in About.

## 0.1.0 — unreleased

First public release. Linux and Windows builds.

- Local agent backend against user-configured OpenAI-compatible endpoints, with no Warp
  account and no Warp servers required.
- Provider presets (DeepSeek, Kimi/Moonshot, and other OpenAI-compatible endpoints) plus
  custom endpoints, with per-model enable/disable and a native model picker.
- Brokered tool execution with native Warp approval prompts, and queue-by-default
  mid-turn prompts with cancel-and-send.
- Local session persistence, context compaction, and a token/usage ledger.
- Warp's paid and cloud-only surfaces hidden while standalone mode is active.
