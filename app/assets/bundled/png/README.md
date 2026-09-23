# Bundled raster assets

These are warpi brand assets, not upstream Warp assets. They are the user's chosen
marks for the fork.

## `warpi-wordmark.png`

Wordmark for warpi, used on the About screen.

- **Provenance**: AI-generated for warpi with `gpt-image-1-mini` (via the OpenRouter image API on 2026-09-23).
- **Source**: `evidence/logos/warpi-wordmark.png` in the audit workspace (generation record: `evidence/logos/README.md`, `evidence/logos/generation-results.json`).
- **Mark**: light-ink `warpi` lockup with a mint caret on a transparent canvas.
- **Processing**: the transparent canvas margin was cropped away for sizing; the artwork pixels are untouched (no recolour, no inversion, no filters).

## `warpi-icon.png`

App icon for warpi, embedded for the runtime X11 window icon.

- **Provenance**: AI-generated for warpi with `gpt-image-1-mini` (via the OpenRouter image API on 2026-09-23).
- **Source**: `evidence/logos/warpi-icon-pi-caret.png` in the audit workspace (generation record: `evidence/logos/README.md`, `evidence/logos/generation-results.json`).
- **Mark**: mint `❯`/`π` glyph on a transparent canvas.
- **Processing**: downscaled with Lanczos resampling from the 1024x1024 source; the artwork is untouched (no recolour, no crop, no filters). The packaged channel icon lives in `app/channels/warpi/icon/`.
