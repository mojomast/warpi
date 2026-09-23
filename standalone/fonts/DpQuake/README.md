# DpQuake fonts (optional terminal font)

Two decorative Quake fonts, shipped with warpi as an **option** (they are never
selected by default).

| File | Family name | Notes |
| --- | --- | --- |
| `dpquake_.ttf` | `DpQuake` | The main "Quake" font (DpQuake 2.3). Includes the Quake/id Software logos on some key combinations documented in `dpquake.txt`. |
| `quake2.ttf` | `QUAKE2` | The Quake 2-style variant from the same package. |
| `dpquake.txt` | — | The author's readme and licence, which **must be shipped with the fonts**. |

## Provenance

- Source: https://www.dafont.com/quake.font ("Quake + DpQuake" by *Dead Pete*)
- Downloaded: 2026-09-23
- dafont lists the package as **100% Free**. The author's own readme states:

  > You may freely distribute this font, but you must ALWAYS include this file!!

  Therefore `dpquake.txt` is part of this directory and is included in every
  package and installer that ships the fonts. The stylized "Q" and the logos
  are noted by the author as copyright id Software; a few logo glyphs are
  credited to Sean Johnson and included by the author for archival purposes.

## Enable it

```bash
standalone/scripts/install-quake-fonts.sh
```

The installer copies the fonts into `~/.local/share/fonts/warpi-quake/`,
refreshes the font cache, and adds a small fontconfig rule that marks `DpQuake`
as monospace so Warp offers it in **Settings → Appearance → Terminal font
family**. Pick `DpQuake` (or `QUAKE2`) there.

## Remove it

```bash
rm -rf ~/.local/share/fonts/warpi-quake ~/.config/fontconfig/conf.d/50-warpi-quake.conf
fc-cache -f
```

## Honest caveats

- DpQuake is a **display font**, not a coding font: it is proportional and has
  no programming ligatures, so text columns, diffs, and TUIs will not align.
  It is most usable for headers/prompts, or just for fun.
- The font is not embedded in the `warpi` binary; it becomes available through
  the system font cache after running the installer.
- If you redistribute warpi together with these fonts, keep `dpquake.txt`
  next to them.
