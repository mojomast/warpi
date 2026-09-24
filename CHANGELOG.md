# Changelog

warpi is a standalone fork of Warp. Its versions are independent of upstream Warp's
date-based tags (`v0.YYYY.MM.DD.HH.MM.channel_NN`). Bump these together:

- `standalone/VERSION` — canonical, used by packaging and docs.
- `app/src/standalone_ui.rs` (`WARPI_VERSION`) — shown in About when no release tag is compiled in.
- `app/Cargo.toml` and `standalone/pi-helper/package.json` — crate/package metadata.

A release build may also set `GIT_RELEASE_TAG` at compile time; when present it takes
precedence over `WARPI_VERSION` in About.

## 0.1.2 — 2026-09-24

- **macOS packaging fixed.** `packaging/package-warpi-macos.sh` now creates the
  CLI tarball before checksumming it and uses `arm64` artifact names that match
  the workflow, so the macOS `.dmg` and `.tar.gz` are produced. Verified: the
  `macos-aarch64` CI job built, packaged, attested, uploaded, and passed the
  adapter tests.
- **Onboarding: local-backend welcome slide.** On the warpi channel (or when
  standalone mode is configured) the first-run experience shows a warpi welcome
  that explains how to add a provider key — *Open Settings → Agents → Local Pi
  provider, pick a preset, paste the key, Test connection* — with a button that
  opens that settings page. The flow no longer routes a fresh warpi install
  through a Warp login.

## 0.1.1 — 2026-09-24

Windows hardening and cross-platform packaging.

- **Bundled Node runtime on every platform.** Releases ship a pinned,
  SHA-256-verified Node.js 22.19.0 next to the executable, and the app prefers it
  over `PATH` (`helper_executable` still overrides). New
  `script/fetch-node-runtime.sh` (Linux/macOS) and the existing PowerShell script
  (Windows) stage it; the installer and bundles include it.
- **Windows startup crash fixed.** The helper's sanitized environment now passes
  `SystemRoot`/`windir`/`TEMP`/`TMP` (and CPU topology), which Node 24's
  `ncrypto::CSPRNG` startup self-check needs; previously a clean host could abort
  on every prompt.
- **Windows fixture tests un-scoped.** The harness now hands Node a plain absolute
  helper path (Node 24 rejects the `\\?\` form `canonicalize()` returns) and the
  TS harness supplies the Windows OS variables, so every fixture suite runs on
  Windows. Verified end to end on a real Windows host.
- **macOS packaging.** New `packaging/package-warpi-macos.sh` (CLI tarball plus a
  drag-to-Applications `.app`/`.dmg`) and a `macos-aarch64` CI job. **Unverified.**
- **Command-line installs.** `install.sh` installs the binary, helper, and bundled
  runtime under `~/.local/opt/warpi` on Linux and macOS.
- **Helper system-prompt guidance.** The model is told the shell is PowerShell on
  Windows (use `$env:VAR` not `%VAR%`, `;` not `&`) and not to batch a slow
  command with other commands.

## 0.1.0 — 2026-09-24

First public release. CI produces installable artifacts for **Linux x86_64 and
Windows x86_64** — the Linux `warpi-linux-x86_64.tar.gz` (with a `.sha256`) and
the Windows `WarpiSetup.exe` — each accompanied by a GitHub **build-provenance
attestation** rather than code signing; there is no Authenticode certificate and
no GPG key. In both jobs the bundle assembly, installer build, attestation, and
artifact upload run **before** the adapter-test step, so a failing test never
costs us the artifacts (the job still ends red). The fixture-based Windows tests
are **scoped** by a single `fixture_test!` macro: any test that spawns the
cross-process Node fixture is `#[ignore]`d on Windows with a tracked reason
("cross-process fixture provider is unreliable on Windows; root cause unverified;
tracked for 0.1.1"), while every unit test and every non-fixture test still runs
and enforces. The fixture's root cause remains **unverified**, and the Windows
installer has not been compiled or run locally. See the README's
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
