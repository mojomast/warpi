# Build and install (warpi development build)

The validated target in this audit is **Linux x86_64, development build**. No
installer or release bundle is produced yet (M5).

## Prerequisites

Same as upstream Warp (see `AGENTS.md` / `script/linux/install_build_deps`):
Rust 1.92.0 (from `rust-toolchain.toml`), `protoc` >= 3.15, `cmake`,
`pkg-config`, a C/C++ toolchain, and the ALSA development headers for the GUI
feature on Linux. Then:

```bash
# Build the private Pi helper (Node >= 22.19)
cd standalone/pi-helper
npm ci
npm run build            # tsc -> dist/
npm test                 # 14 tests

# Build the app
cd ../..
cargo build -p warp --bin warpi --features gui
```

The helper's `dist/` directory must exist before the app is started; at runtime
the app finds it at `standalone/pi-helper/dist/main.js` relative to the
executable (or via `WARPI_PI_HELPER_ENTRY`).

## Runtime prerequisite (end users)

**Node.js >= 22.19.0 is required.** The Windows installer bundles a pinned
Node.js 22.19.0 next to the executable (`standalone\node\node.exe`) and the app
prefers it; an explicit `helper_executable` still overrides, and a development
tree without a bundled runtime falls back to `node` on `PATH`. See `WINDOWS.md`
section 8 for the `ncrypto::CSPRNG` failure mode this closes, the runtime
selection rule, and the diagnostics for a hand-configured runtime.

## Configure standalone mode

The normal way to configure warpi is the in-app settings page
(**Settings → Agents → Local Pi provider**): endpoint, model id, additional model
ids, limits, authentication mode, "Test connection", and the API key (stored in
the OS secret store). Provider presets prefill common endpoints (including Kimi
(Moonshot)); every field stays editable. This file is the same configuration and
can also be written by hand at `<warpi data dir>/standalone/config.json`, or
pointed at with `WARPI_STANDALONE_CONFIG`. Example with an API key stored in the
OS secret store under the key `warpi/profile/local`:

```json
{
  "enabled": true,
  "profiles": [
    {
      "id": "local",
      "display_name": "Local llama.cpp",
      "base_url": "http://127.0.0.1:8080/v1",
      "wire": "open_ai_chat_completions",
      "model_id": "qwen3-coder-30b",
      "models": [],
      "disabled_models": [],
      "credential": { "secret_store": { "key": "warpi/profile/local" } },
      "context_limit": 131072,
      "output_limit": 8192,
      "compat": { "supports_developer_role": false },
      "reasoning": false,
      "supports_image_input": false
    }
  ],
  "active_profile": "local",
  "load_context_files": true,
  "max_context_file_bytes": 65536
}
```

The older single-`profile` form is still read (it is folded into `profiles`).
For an endpoint that requires no authentication:

```json
"credential": "none"
```

`wire` only accepts `open_ai_chat_completions`. Other protocols are rejected.

On a Linux box with no Secret Service (no `gnome-keyring`/KWallet running),
Warp falls back to an AES-256-GCM-encrypted file under the application state
directory, mode `0600`; the key never enters the config file or the helper's
session.

Credentials are written by the settings page. If you prefer the platform tool,
the key format is `warpi/profile/<profile-id>` (macOS Keychain
`security add-generic-password`, `secret-tool store` on Linux, Windows
Credential Manager). The value is read at first use and kept only in memory.

## Verify

```bash
cargo test -p standalone_agent          # source inventory at 2026-09-23: 56 test functions
cd standalone/pi-helper && npm test     # 14 tests
```

The last recorded full runs are older and smaller than the current suites; see
`VALIDATION.md` for exactly what has been re-run.

## Known limitations

- Configuration is normally done in the settings page (see above); a hand-written
  config file is still supported but no longer required.
- No packaged release; the app runs from `target/debug/warpi`. The no-sudo
  development bundle in `packaging/package-warpi.sh` is not a signed installer.
- `auth=none` is the only mode that works without any credential; everything
  else needs a secret-store entry or the Linux file fallback.
- A profile/model/credential change does not apply to a conversation whose
  helper session is already open (see `ARCHITECTURE.md`).
