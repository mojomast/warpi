# Build and install (development build)

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
npm test                 # 13 tests

# Build the app
cd ../..
cargo build -p warp --bin warp-oss --features gui
```

The helper's `dist/` directory must exist before the app is started; at runtime
the app finds it at `standalone/pi-helper/dist/main.js` relative to the
executable (or via `WARPOS_PI_HELPER_ENTRY`).

## Configure standalone mode

Write `<Warp data dir>/standalone/config.json` (or point
`WARPOS_STANDALONE_CONFIG` at a file). Example with an API key stored in the
OS secret store under the key `warposs/local`:

```json
{
  "enabled": true,
  "profile": {
    "id": "local",
    "display_name": "Local llama.cpp",
    "base_url": "http://127.0.0.1:8080/v1",
    "wire": "open_ai_chat_completions",
    "model_id": "qwen3-coder-30b",
    "credential": { "secret_store": { "key": "warposs/local" } },
    "context_limit": 131072,
    "output_limit": 8192,
    "compat": { "supports_developer_role": false },
    "reasoning": false,
    "supports_image_input": false
  },
  "load_context_files": true,
  "max_context_file_bytes": 65536
}
```

For an endpoint that requires no authentication:

```json
"credential": "none"
```

`wire` only accepts `open_ai_chat_completions`. Other protocols are rejected.

Store the secret with the platform tool of your choice (macOS Keychain
`security add-generic-password`, `secret-tool store` on Linux, Windows
Credential Manager). The value is read at first use and kept only in memory.

## Verify

```bash
cargo test -p standalone_agent        # 20 tests
cd standalone/pi-helper && npm test   # 13 tests
```

## Known limitations

- No settings UI yet: the config file is hand-written.
- No packaged release; the app runs from `target/debug/warp-oss`.
- `auth=none` is the only mode that works without any credential; everything
  else needs a secret-store entry.
