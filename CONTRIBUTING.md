# Contributing to warpi

warpi is a fork of Warp, pinned to upstream commit
`71088ba18d27114ccfb358901c66b54220cf30d0` on branch `standalone-pi-backend`.
This document covers the fork-specific workflow. Upstream's `AGENTS.md`,
`CONTRIBUTING.md`, and `CODE_OF_CONDUCT.md` still apply where they do not
conflict with the fork's scope.

## Scope

Fork changes are additive and live in:

- `crates/standalone_agent/` — the Rust adapter (protocol, provider profiles,
  secrets, helper supervision, bridge, Warp event shaping);
- `app/src/ai/standalone/mod.rs` and the provider settings page — the app-side
  wiring and configuration;
- `standalone/pi-helper/` — the private Node helper embedding the Pi SDK;
- `standalone/*.md` — design, security, compatibility, and evidence documents.

Do not rewrite or delete upstream files when an additive change will do. Keep
standalone routing free of any cloud fallback: agent traffic either goes to the
configured local endpoint or fails locally.

## Build and test

```bash
# Private helper (Node >= 22.19)
cd standalone/pi-helper
npm ci
npm run build
npm test                                            # 13 tests

# Rust adapter
cargo test -p standalone_agent                      # 24 tests
cargo check -p warp --bin warpi --features gui
cargo check -p warp --tests --features gui

# Development build and run
cargo build -p warp --bin warpi --features gui
target/debug/warpi
```

Prerequisites, configuration, and packaging are documented in
`standalone/BUILDING.md` and `README.md`. On the audit machine (no `sudo`),
`source standalone/dev-env.sh` provides the protoc/cmake/ALSA shims; they are
development-only and must not be relied on by the product.

Any change that touches runtime behaviour should come with a test in the
relevant suite, and any change to evidence-backed claims should update
`standalone/VALIDATION.md` with the exact command and result.

## One writer per shared interface

The protocol is a shared interface between `crates/standalone_agent` and
`standalone/pi-helper`, plus the native event shapes consumed by the Warp UI.
**Freeze a shared interface before parallel work, and keep exactly one writer on
it at a time.**

Before changing any of these, read `standalone/ARCHITECTURE.md`:

- protocol frames and their fields (NDJSON, v1), identity fields
  (`session_id`/`generation`/`turn_id`/`exchange_id`/`seq`), bounds, and error
  semantics;
- the six canonical workspace tool names and their argument/result schemas;
- `ResponseEvent` shaping and the exchange-vs-run lifecycle;
- credential-handling rules and the allowlisted helper environment.

A protocol change is not complete until: the frame/field semantics are recorded
in `standalone/ARCHITECTURE.md`, both sides (`protocol.rs` and `protocol.ts`)
agree, and the affected Rust and helper tests pass. If two people need to work
on the same interface, one writes and the other reviews; do not land competing
interpretations.

## Security

Follow the disclosure process in `SECURITY.md`; do not open a public issue for a
suspected vulnerability. The trust boundaries the fork relies on are stated in
`standalone/SECURITY.md` — changes that weaken one (for example, treating model
output as authorization or adding a cloud fallback) need an explicit design
discussion first.

## Licensing

Contributions are accepted under the license of the area they touch:
AGPL-3.0-only for the fork and app code, MIT for `crates/warpui` and
`crates/warpui_core`. Ported third-party behaviour requires attribution with the
exact commit, as done for sasuke39/openwarp in `standalone/REUSE.md` and
`standalone/DONOR-ATTRIBUTION.md`. See `LICENSE-NOTES.md`.
