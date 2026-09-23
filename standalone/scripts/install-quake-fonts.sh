#!/usr/bin/env bash
#
# Install the bundled DpQuake fonts so Warp/warpi offers them as a terminal
# font family. No sudo, no network: the fonts are shipped in this repository.
#
# Usage: standalone/scripts/install-quake-fonts.sh [--uninstall]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FONT_SRC="$(cd "$SCRIPT_DIR/.." && pwd)/fonts/DpQuake"
FONT_DEST="${XDG_DATA_HOME:-$HOME/.local/share}/fonts/warpi-quake"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fontconfig/conf.d"
CONF_FILE="$CONF_DIR/50-warpi-quake.conf"

if [ "${1:-}" = "--uninstall" ]; then
  rm -rf "$FONT_DEST" "$CONF_FILE"
  fc-cache -f >/dev/null 2>&1 || true
  echo "Removed DpQuake fonts and the fontconfig rule."
  exit 0
fi

if [ ! -f "$FONT_SRC/dpquake_.ttf" ]; then
  echo "error: bundled fonts not found at $FONT_SRC" >&2
  exit 1
fi
if ! command -v fc-cache >/dev/null 2>&1; then
  echo "error: fontconfig (fc-cache) is required to install fonts" >&2
  exit 1
fi

mkdir -p "$FONT_DEST" "$CONF_DIR"
# The author's readme must accompany the fonts: "You may freely distribute this
# font, but you must ALWAYS include this file!!"
cp -f "$FONT_SRC/dpquake_.ttf" "$FONT_SRC/quake2.ttf" "$FONT_SRC/dpquake.txt" "$FONT_DEST/"

# DpQuake is a display font, so fontconfig does not classify it as monospace and
# Warp's terminal font picker would hide it. Mark it monospace locally so the
# font shows up as an option; this only affects this user account.
cat > "$CONF_FILE" <<'EOF'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<!-- Added by warpi's install-quake-fonts.sh: offer DpQuake as a terminal font. -->
<fontconfig>
  <match target="pattern">
    <test name="family" compare="eq" ignore-blanks="true">
      <string>DpQuake</string>
    </test>
    <edit name="spacing" mode="assign">
      <int>100</int>
    </edit>
    <edit name="family" mode="append">
      <string>monospace</string>
    </edit>
  </match>
  <alias binding="same">
    <family>DpQuake</family>
    <accept><family>monospace</family></accept>
  </alias>
</fontconfig>
EOF

fc-cache -f >/dev/null 2>&1 || true

if command -v fc-list >/dev/null 2>&1 && fc-list : family | grep -qi 'dpquake'; then
  echo "Installed. Available families:"
  fc-list : family | grep -i -E 'dpquake|quake2' | sort -u | sed 's/^/  - /'
else
  echo "Fonts copied to $FONT_DEST, but fc-list does not show them yet."
  echo "Log out and back in (or run 'fc-cache -f -v') and check 'fc-list | grep -i quake'."
fi

echo
echo "Select it in Warp:  Settings -> Appearance -> Terminal font family -> DpQuake"
echo "Remove it later with: standalone/scripts/install-quake-fonts.sh --uninstall"
