# License notes (warpi)

This file explains which license applies to which part of the repository, and
what a redistributor must preserve. It is a summary, not a substitute for the
license texts themselves.

## Component map

| Component / path | License | Copyright |
| --- | --- | --- |
| Upstream Warp client code (everything except the two crates below), as modified by this fork: `app/`, `crates/*` other than `crates/warpui` and `crates/warpui_core`, `script/`, build files | **AGPL-3.0-only** — `LICENSE-AGPL` | Copyright (C) 2020-2026 Denver Technologies, Inc., and contributors |
| `crates/warpui/`, `crates/warpui_core/` (the UI framework crates) | **MIT** — `LICENSE-MIT` | Copyright (C) 2020-2026 Denver Technologies, Inc. |
| New fork code: `crates/standalone_agent/`, `standalone/**` (including `standalone/pi-helper/`), the `warpi` binary/entrypoint and app wiring added by this fork, `packaging/**`, `.github/workflows/warpi-build.yml` | **AGPL-3.0-only** (same terms as the app; see `Cargo.toml` `[workspace.package] license = "AGPL-3.0-only"` and `standalone/pi-helper/package.json` `"license": "AGPL-3.0-only"`) | Fork contributors; upstream notices retained |
| Behaviour ported from **sasuke39/openwarp**, commit `5045d30` (NDJSON tool-brokering protocol ideas and Warp client-action event shapes) | **MIT** — notice reproduced in `standalone/DONOR-ATTRIBUTION.md` | Copyright (c) 2026 sasuke39 |
| npm runtime dependencies of the helper: `@earendil-works/pi-coding-agent@0.84.2`, `typebox@1.3.7` (and their transitive dependencies) | Their own licenses — **MIT** for the two pinned packages per their package metadata | Their respective authors; license files/package metadata travel inside `node_modules` when bundled |
| The names "Warp" and "warpi", logos, and other brand assets | Not granted by any of the above | Their respective owners |

No donor source file was copied into this repository. The donor's separate
`openwarp-client` repository is AGPL-3.0 and was not used; see
`standalone/REUSE.md` for the exact list of ported behaviours and deliberate
deviations.

## What a redistributor must preserve

The fork combines an AGPL-3.0-only work (the application), an MIT-licensed part
of that work (the `warpui` crates), MIT-licensed donor attribution, and bundled
npm dependencies. A binary or source redistribution must:

1. **Keep the license texts.** Ship `LICENSE-AGPL` and `LICENSE-MIT` unchanged
   with both source and binary distributions. The development bundle produced by
   `packaging/package-warpi.sh` copies both into the package and the installed
   directory for this reason.
2. **Honour the MIT notices.** The MIT copyright and permission notice for
   `crates/warpui`/`crates/warpui_core` must be included in copies and
   substantial portions of the software — including compiled binaries that
   embed those crates. The donor MIT notice (sasuke39/openwarp, 2026) must be
   preserved for the ported behaviour; keep `standalone/DONOR-ATTRIBUTION.md`
   and `standalone/REUSE.md` in source distributions.
3. **Honour the AGPL.** Provide the Corresponding Source for the version you
   distribute, mark modified files as changed (the fork's changes are additive:
   see `standalone/REUSE.md` "Files added by this fork" and "Upstream files
   changed"), and do not add restrictions beyond the AGPL. If you let users
   interact with a modified version over a network, AGPL section 13 requires
   offering them the Corresponding Source.
4. **Preserve dependency notices.** If you redistribute the bundled
   `node_modules`, keep each package's `license` file and `package.json`
   metadata (for example `typebox`'s `license` file and the Pi SDK's MIT
   declaration). Do not strip them when pruning the bundle.
5. **Do not imply affiliation.** Do not use Warp's or the donor's names or marks
   to suggest endorsement, and keep the disclaimer in `README.md` with any
   redistribution.
6. **Keep the traceability files.** `standalone/REUSE.md` (source pins) and
   `standalone/DONOR-ATTRIBUTION.md` (donor commit and MIT text) are part of the
   compliance story, not optional documentation.

## Files added by this fork

For reference, the complete list of additively changed paths is maintained in
`standalone/REUSE.md`:

```
crates/standalone_agent/            new Rust crate (AGPL-3.0-only)
app/src/ai/standalone/mod.rs        standalone wiring + local config
app/src/bin/warpi.rs                warpi entry point and channel identity
app/src/settings_view/local_provider_page.rs   provider settings page
app/src/ai/mod.rs, agent/api.rs, agent/api/impl.rs, settings/ai.rs   additive wiring
app/Cargo.toml                      + standalone_agent dependency and warpi bin
standalone/pi-helper/               private Node helper embedding the Pi SDK
standalone/*.md                     documentation
standalone/dev-env.sh               audit-machine dev shims (not shipped)
packaging/                          development packaging scripts
```

There is no separate per-file license header on the new files; their license is
the repository default above.
