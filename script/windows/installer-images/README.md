# Inno Setup wizard images

`warp-banner.bmp` / `warp-logo.bmp` are the **upstream Warp** wizard images used
by the non-warpi channels.

The `warpi-*.png` files are the warpi brand images used by
`windows-installer.iss` (which builds `WarpiSetup.exe`). Each comes in two
resolutions so Inno Setup 6.7 can pick the best match for the system DPI
(`WizardImageFile` / `WizardSmallImageFile` accept a comma-separated list).

| File | Size | Wizard image area (Inno 6.7 defaults) |
| --- | --- | --- |
| `warpi-banner.png` | 202x386 | large, 100% |
| `warpi-banner-2x.png` | 430x824 | large, 200% |
| `warpi-logo.png` | 58x58 | small, 100% |
| `warpi-logo-2x.png` | 124x124 | small, 200% |

The sizes are the documented modern-style image areas for the post-6.6.0
defaults (100% large = 202x386, small = 58x58); see
<https://jrsoftware.org/ishelp/topic_setup_wizardimagefile.htm> and
<https://jrsoftware.org/ishelp/topic_setup_wizardsmallimagefile.htm>.

## Composition

The banner is a full-bleed `#0B0D10` tile (the same dark tile the About page
uses behind the light-ink wordmark) with the mint icon above the wordmark. The
small image is the mint icon on a transparent canvas, which reads on the light
wizard panel as well as a dark one. No logo artwork was created or recoloured;
the images only place the fork's checked-in assets:

- `app/assets/bundled/png/warpi-wordmark.png` (wordmark)
- `app/channels/warpi/icon/no-padding/512x512.png` (icon)

## Regenerating

```shell
python3 script/windows/installer-images/make-warpi-images.py
```

Run from the repository root. The script needs Pillow and rewrites the four
`warpi-*.png` files in place.
