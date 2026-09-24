# standalone/ — the warpi agent backend

Everything this fork ("warpi") adds for account-free, server-free agent inference.

```
crates/standalone_agent/   Rust backend (protocol, provider profiles, secrets,
                           helper supervision, bridge, Warp event translation)
standalone/pi-helper/      Private Node helper embedding the Pi coding-agent SDK
standalone/*.md            Architecture, reuse, security, network, compatibility,
                           validation, plan, build instructions, Windows probe
standalone/evidence/       Test logs and GUI screenshots backing VALIDATION.md
standalone/dev-env.sh      Dev-only environment shims for this audit machine
```

Read in this order:

1. `ARCHITECTURE.md` — ownership split, exchange vs run, turn-lifecycle guarantees, queueing, protocol summary, provider UI.
2. `BUILDING.md` — how to build and configure a local endpoint.
3. `PROVIDER_COMPATIBILITY.md` — what is supported, rejected, and unverified.
4. `SECURITY.md` — credentials, validation, approvals, honest boundaries.
5. `NETWORK_DEPENDENCIES.md` — every application-managed egress path.
6. `VALIDATION.md` — exact evidence, PASS/FAIL/NOT RUN, risks.
7. `REUSE.md` — pins, donor commits, licence/attribution.
8. `PLAN.md` — milestones, current status, what remains.
9. `WINDOWS.md` — the Windows cross-check probe and what a real runner needs.
10. `DONOR-ATTRIBUTION.md` — donor commit and MIT licence text.

Quick check:

```bash
cargo test -p standalone_agent
(cd standalone/pi-helper && npm ci && npm test)
```

Runtime: the Windows installer bundles a pinned Node.js 22.19.0 and the app
prefers it over `PATH`; an explicit `helper_executable` still overrides.
**Node.js >= 22.19.0 is required**. See `WINDOWS.md` section 8 for the runtime
selection rule, the `ncrypto::CSPRNG` startup failure it closes, and the
diagnostics.
