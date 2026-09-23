# warpi channel icon

Application icon for the `warpi` channel, derived from the user's chosen mark.

- **Provenance**: AI-generated for warpi with `gpt-image-1-mini` (via the OpenRouter image API on 2026-09-23).
- **Source**: `evidence/logos/warpi-icon-pi-caret.png` in the audit workspace (generation record: `evidence/logos/README.md`, `evidence/logos/generation-results.json`).
- **Mark**: the user's chosen app icon — a mint `❯`/`π` glyph on a transparent canvas.
- **Processing**: the full 1024x1024 canvas was downscaled with Lanczos resampling to the platform sizes below; the artwork is untouched (no recolour, no crop, no filters).

## Files

- `no-padding/16x16.png` … `no-padding/512x512.png` — Linux hicolor sizes and the PNG used for macOS bundling.
- `no-padding/icon.ico` — Windows resource icon (16/32/48/64/128/256, multi-resolution).
- `icon.icns` — macOS icon (multi-resolution), used as the bundle icon via `app/Cargo.toml`.
- `AppIcon.icon/` — macOS Sequoia adaptive-icon bundle: `icon.json` plus `Assets/warpi.png`, the 1024x1024 master. `script/compile_icon` compiles it with `actool` during macOS bundling.
