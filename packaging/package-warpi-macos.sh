#!/usr/bin/env bash
# package-warpi-macos.sh — assemble a macOS bundle for warpi.
#
# Produces:
#   dist/warpi-macos-<arch>/            runnable tree (binary + helper + bundled Node)
#   dist/warpi-macos-<arch>.tar.gz      CLI-installable archive (+ .sha256)
#   dist/warpi-macos-<arch>.dmg         drag-to-/Applications disk image (when hdiutil exists)
#
# This script copies files that already exist and stages the pinned Node runtime
# (via script/fetch-node-runtime.sh, or WARPI_NODE_RUNTIME if set). It does not
# build the binary or the helper. Before running it:
#
#   cargo build -p warp --bin warpi --features gui --release
#   (cd standalone/pi-helper && npm ci && npm run build)
#
# Environment overrides:
#   WARPI_BINARY        binary to package   (default: <repo>/target/release/warpi)
#   WARPI_DIST_DIR      output root         (default: <repo>/dist)
#   WARPI_ARCH          architecture label  (default: uname -m, mapped to arm64/x64)
#   WARPI_NODE_RUNTIME  pre-fetched Node dir to copy instead of downloading
#   WARPI_VERSION       version string      (default: standalone/VERSION)
#   WARPI_SKIP_DMG=1    do not build a .dmg
#   WARPI_SKIP_CHECKSUMS=1  do not write .sha256

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER_DIR="$REPO_ROOT/standalone/pi-helper"
BINARY="${WARPI_BINARY:-$REPO_ROOT/target/release/warpi}"
DIST_ROOT="${WARPI_DIST_DIR:-$REPO_ROOT/dist}"
VERSION="${WARPI_VERSION:-$(cat "$REPO_ROOT/standalone/VERSION" 2>/dev/null || echo 0.1.0)}"

case "${WARPI_ARCH:-$(uname -m)}" in
    arm64 | aarch64) ARCH="arm64" ;;
    x86_64 | amd64) ARCH="x64" ;;
    *) echo "package-warpi-macos.sh: unsupported arch ${WARPI_ARCH:-$(uname -m)}" >&2; exit 1 ;;
esac

PKG_DIR="$DIST_ROOT/warpi-macos-$ARCH"
STAGE_HELPER="$PKG_DIR/standalone/pi-helper"
ICNS="$REPO_ROOT/app/channels/warpi/icon/icon.icns"

fail() {
    printf 'package-warpi-macos.sh: %s\n' "$*" >&2
    exit 1
}

[ -n "$PKG_DIR" ] && [ "$PKG_DIR" != "/" ] || fail "refusing to use an empty package directory"
[ -x "$BINARY" ] || fail "binary not found or not executable: $BINARY (run: cargo build -p warp --bin warpi --features gui --release)"
[ -f "$HELPER_DIR/dist/main.js" ] || fail "built helper not found: $HELPER_DIR/dist/main.js (run: cd standalone/pi-helper && npm ci && npm run build)"
[ -f "$HELPER_DIR/package.json" ] || fail "missing $HELPER_DIR/package.json"
[ -f "$HELPER_DIR/package-lock.json" ] || fail "missing $HELPER_DIR/package-lock.json"
[ -d "$HELPER_DIR/node_modules" ] || fail "missing $HELPER_DIR/node_modules (run npm ci in standalone/pi-helper first)"
[ -f "$REPO_ROOT/packaging/install.sh" ] || fail "missing packaging/install.sh"
[ -f "$REPO_ROOT/packaging/config.example.json" ] || fail "missing packaging/config.example.json"

rm -rf "$PKG_DIR"
mkdir -p "$PKG_DIR/standalone" "$STAGE_HELPER"

install -m 0755 "$BINARY" "$PKG_DIR/warpi"
install -m 0755 "$REPO_ROOT/packaging/install.sh" "$PKG_DIR/install.sh"
install -m 0644 "$REPO_ROOT/packaging/config.example.json" "$PKG_DIR/config.example.json"
install -m 0644 "$REPO_ROOT/LICENSE-AGPL" "$REPO_ROOT/LICENSE-MIT" "$PKG_DIR/"
[ -f "$REPO_ROOT/LICENSE-NOTES.md" ] && install -m 0644 "$REPO_ROOT/LICENSE-NOTES.md" "$PKG_DIR/"
[ -f "$REPO_ROOT/THIRD_PARTY_LICENSES.txt" ] && install -m 0644 "$REPO_ROOT/THIRD_PARTY_LICENSES.txt" "$PKG_DIR/"

shopt -s nullglob
docs=("$REPO_ROOT"/standalone/*.md)
shopt -u nullglob
((${#docs[@]} > 0)) && cp -a "${docs[@]}" "$PKG_DIR/standalone/"

cp -a "$HELPER_DIR/dist" "$STAGE_HELPER/dist"
install -m 0644 "$HELPER_DIR/package.json" "$HELPER_DIR/package-lock.json" "$STAGE_HELPER/"

# Production dependency tree, dev-pruned when an offline install is possible.
if command -v npm >/dev/null 2>&1 \
    && (cd "$STAGE_HELPER" && npm ci --omit=dev --offline --no-audit --no-fund >/dev/null 2>&1); then
    printf 'helper dependencies: production install (offline, dev dependencies pruned)\n'
else
    printf 'helper dependencies: copying the full node_modules tree (npm ci --offline was unavailable)\n' >&2
    rm -rf "$STAGE_HELPER/node_modules"
    cp -a "$HELPER_DIR/node_modules" "$STAGE_HELPER/node_modules"
fi
[ -f "$STAGE_HELPER/node_modules/@earendil-works/pi-coding-agent/package.json" ] \
    || fail "staged helper is missing the Pi SDK; run npm ci in standalone/pi-helper and retry"

# Pinned Node runtime next to the executable, matching the app's selection rule
# (<package>/standalone/node/bin/node).
if [ -n "${WARPI_NODE_RUNTIME:-}" ]; then
    mkdir -p "$PKG_DIR/standalone/node/bin"
    cp -a "$WARPI_NODE_RUNTIME/." "$PKG_DIR/standalone/node/"
else
    "$REPO_ROOT/script/fetch-node-runtime.sh" --platform darwin --arch "$ARCH" --dest "$PKG_DIR/standalone/node"
fi
[ -f "$PKG_DIR/standalone/node/bin/node" ] || fail "bundled Node runtime is missing: $PKG_DIR/standalone/node/bin/node"

# CLI archive.
tar -C "$DIST_ROOT" -czf "$DIST_ROOT/warpi-macos-$ARCH.tar.gz" "warpi-macos-$ARCH"

if [ "${WARPI_SKIP_CHECKSUMS:-0}" != "1" ] && command -v shasum >/dev/null 2>&1; then
    (cd "$DIST_ROOT" && shasum -a 256 "warpi-macos-$ARCH.tar.gz" >"warpi-macos-$ARCH.tar.gz.sha256")
fi

# GUI app bundle + disk image, when the macOS tooling is present.
if [ "${WARPI_SKIP_DMG:-0}" != "1" ] && command -v hdiutil >/dev/null 2>&1; then
    APP="$DIST_ROOT/warpi.app"
    DMG_STAGE="$DIST_ROOT/dmg-stage"
    rm -rf "$APP" "$DMG_STAGE"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    install -m 0755 "$BINARY" "$APP/Contents/MacOS/warpi"
    # The app resolves the helper and runtime next to the executable
    # (standalone/pi-helper/dist/main.js and standalone/node/bin/node).
    cp -a "$PKG_DIR/standalone" "$APP/Contents/MacOS/standalone"
    [ -f "$ICNS" ] && install -m 0644 "$ICNS" "$APP/Contents/Resources/icon.icns"
    cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>warpi</string>
  <key>CFBundleDisplayName</key><string>warpi</string>
  <key>CFBundleIdentifier</key><string>dev.warpi.warpi</string>
  <key>CFBundleExecutable</key><string>warpi</string>
  <key>CFBundleIconFile</key><string>icon.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
    printf 'APPL????' >"$APP/Contents/PkgInfo"
    mkdir -p "$DMG_STAGE"
    cp -a "$APP" "$DMG_STAGE/warpi.app"
    ln -s /Applications "$DMG_STAGE/Applications"
    hdiutil create -volname warpi -srcfolder "$DMG_STAGE" -ov -format UDZO "$DIST_ROOT/warpi-macos-$ARCH.dmg" >/dev/null
    rm -rf "$DMG_STAGE" "$APP"
fi

printf '\ncreated %s\n' "$PKG_DIR"
printf 'archive:  %s/warpi-macos-%s.tar.gz\n' "$DIST_ROOT" "$ARCH"
[ -f "$DIST_ROOT/warpi-macos-$ARCH.dmg" ] && printf 'disk image: %s/warpi-macos-%s.dmg\n' "$DIST_ROOT" "$ARCH"
printf '\nInstall from the command line:\n  tar -xzf warpi-macos-%s.tar.gz && ./warpi-macos-%s/install.sh\n' "$ARCH" "$ARCH"
printf 'Or open the .dmg and drag warpi.app to Applications.\n'
