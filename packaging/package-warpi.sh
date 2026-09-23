#!/usr/bin/env bash
# package-warpi.sh — assemble a Linux development bundle for warpi.
#
# This script only copies files that already exist. It does not build anything,
# does not use sudo, and does not use the network. Before running it:
#
#   cargo build -p warp --bin warpi --features gui         # -> target/debug/warpi
#   (cd standalone/pi-helper && npm ci && npm run build)   # -> dist/main.js
#
# Output: dist/warpi-linux-<arch>/ containing the warpi binary, the private Pi
# helper (dist, package metadata, and its production node_modules), the
# standalone/*.md documentation, license files, an example config, an install.sh,
# and SHA256SUMS.
#
# Environment overrides:
#   WARPI_BINARY      binary to package       (default: <repo>/target/debug/warpi)
#   WARPI_DIST_DIR    output root             (default: <repo>/dist)
#   WARPI_ARCH        architecture label      (default: uname -m)
#   WARPI_SKIP_CHECKSUMS=1  do not write SHA256SUMS

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER_DIR="$REPO_ROOT/standalone/pi-helper"
BINARY="${WARPI_BINARY:-$REPO_ROOT/target/debug/warpi}"
ARCH="${WARPI_ARCH:-$(uname -m)}"
DIST_ROOT="${WARPI_DIST_DIR:-$REPO_ROOT/dist}"
PKG_DIR="$DIST_ROOT/warpi-linux-$ARCH"
STAGE_HELPER="$PKG_DIR/standalone/pi-helper"

fail() {
    printf 'package-warpi.sh: %s\n' "$*" >&2
    exit 1
}

[ -n "$PKG_DIR" ] && [ "$PKG_DIR" != "/" ] || fail "refusing to use an empty package directory"

[ -x "$BINARY" ] \
    || fail "binary not found or not executable: $BINARY (run: cargo build -p warp --bin warpi --features gui)"
[ -f "$HELPER_DIR/dist/main.js" ] \
    || fail "built helper not found: $HELPER_DIR/dist/main.js (run: cd standalone/pi-helper && npm ci && npm run build)"
[ -f "$HELPER_DIR/package.json" ] || fail "missing $HELPER_DIR/package.json"
[ -f "$HELPER_DIR/package-lock.json" ] || fail "missing $HELPER_DIR/package-lock.json"
[ -d "$HELPER_DIR/node_modules" ] \
    || fail "missing $HELPER_DIR/node_modules (run npm ci in standalone/pi-helper first)"
[ -f "$REPO_ROOT/packaging/install.sh" ] || fail "missing packaging/install.sh"
[ -f "$REPO_ROOT/packaging/config.example.json" ] || fail "missing packaging/config.example.json"
[ -f "$REPO_ROOT/LICENSE-AGPL" ] && [ -f "$REPO_ROOT/LICENSE-MIT" ] \
    || fail "missing LICENSE-AGPL / LICENSE-MIT at the repository root"
[ -f "$REPO_ROOT/THIRD_PARTY_LICENSES.txt" ] \
    || fail "missing THIRD_PARTY_LICENSES.txt at the repository root"

rm -rf "$PKG_DIR"
mkdir -p "$PKG_DIR/standalone" "$STAGE_HELPER"

install -m 0755 "$BINARY" "$PKG_DIR/warpi"
install -m 0755 "$REPO_ROOT/packaging/install.sh" "$PKG_DIR/install.sh"
install -m 0644 "$REPO_ROOT/packaging/config.example.json" "$PKG_DIR/config.example.json"
install -m 0644 "$REPO_ROOT/LICENSE-AGPL" "$REPO_ROOT/LICENSE-MIT" "$PKG_DIR/"
[ -f "$REPO_ROOT/LICENSE-NOTES.md" ] && install -m 0644 "$REPO_ROOT/LICENSE-NOTES.md" "$PKG_DIR/"
install -m 0644 "$REPO_ROOT/THIRD_PARTY_LICENSES.txt" "$PKG_DIR/"

shopt -s nullglob
docs=("$REPO_ROOT"/standalone/*.md)
shopt -u nullglob
((${#docs[@]} > 0)) || fail "no standalone/*.md documentation found"
cp -a "${docs[@]}" "$PKG_DIR/standalone/"

cp -a "$HELPER_DIR/dist" "$STAGE_HELPER/dist"
install -m 0644 "$HELPER_DIR/package.json" "$HELPER_DIR/package-lock.json" "$STAGE_HELPER/"

# The compiled helper imports the Pi SDK by bare specifier, so the production
# dependency tree must ship with it. Prefer an offline, dev-pruned install using
# the already-populated npm cache; fall back to copying the existing tree.
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

if [ "${WARPI_SKIP_CHECKSUMS:-0}" != "1" ] && command -v sha256sum >/dev/null 2>&1; then
    (cd "$PKG_DIR" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum >SHA256SUMS)
fi

printf '\ncreated %s\n' "$PKG_DIR"
du -sh "$PKG_DIR" 2>/dev/null || true
printf '\nInstall without sudo:\n  %s/install.sh\n' "$PKG_DIR"
printf 'Or run it in place:\n  WARPI_PI_HELPER_ENTRY=%s %s/warpi\n' "$STAGE_HELPER/dist/main.js" "$PKG_DIR"
