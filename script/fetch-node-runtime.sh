#!/usr/bin/env bash
# Fetch, verify, and stage the pinned Node.js runtime warpi bundles.
#
# The helper must not depend on whatever `node` a machine happens to have on
# PATH. A release therefore ships a known-good Node runtime next to the
# executable, and the app prefers it over PATH (see standalone/ARCHITECTURE.md,
# "Runtime selection").
#
# This script downloads the *official* nodejs.org archive over HTTPS, verifies
# its SHA-256 against the value pinned below (copied from the matching
# https://nodejs.org/dist/v<Version>/SHASUMS256.txt), and stages only the
# runtime (`bin/node`) with Node's own LICENSE plus a provenance record.
#
# Usage:
#   script/fetch-node-runtime.sh --dest dist/warpi-linux-x86_64/standalone/node
#   script/fetch-node-runtime.sh --platform darwin --arch arm64 --dest <dir>
#
# The pinned version/hashes are the single source of truth for what a warpi
# release bundles; bump them together.

set -euo pipefail

VERSION="22.19.0"
PLATFORM=""
ARCH=""
DEST=""
FORCE=0

usage() {
    sed -n '2,20p' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --platform) PLATFORM="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --dest) DEST="$2"; shift 2 ;;
        --force) FORCE=1; shift ;;
        -h | --help) usage 0 ;;
        *) echo "fetch-node-runtime.sh: unknown argument: $1" >&2; usage 1 ;;
    esac
done

[ -n "$DEST" ] || { echo "fetch-node-runtime.sh: --dest is required" >&2; exit 1; }

if [ -z "$PLATFORM" ]; then
    case "$(uname -s)" in
        Darwin) PLATFORM="darwin" ;;
        Linux) PLATFORM="linux" ;;
        *) echo "fetch-node-runtime.sh: unsupported OS $(uname -s); pass --platform" >&2; exit 1 ;;
    esac
fi
if [ -z "$ARCH" ]; then
    case "$(uname -m)" in
        arm64 | aarch64) ARCH="arm64" ;;
        x86_64 | amd64) ARCH="x64" ;;
        *) echo "fetch-node-runtime.sh: unsupported arch $(uname -m); pass --arch" >&2; exit 1 ;;
    esac
fi

# SHA-256 of the official archive, from the matching SHASUMS256.txt.
pinned_hash() {
    case "$1/$2" in
        22.19.0/darwin-arm64) echo "c59006db713c770d6ec63ae16cb3edc11f49ee093b5c415d667bb4f436c6526d" ;;
        22.19.0/darwin-x64)   echo "3cfed4795cd97277559763c5f56e711852d2cc2420bda1cea30c8aa9ac77ce0c" ;;
        22.19.0/linux-x64)    echo "c0649af18e6a24f6fe5535a3e86b341dd49a8e71117c8b68bde973ef834f16f2" ;;
        22.19.0/linux-arm64)  echo "0b2d9f564b6594222a62c82e1df2efe119dd4a4aff29644f4dd325bf360b6bcc" ;;
        *) return 1 ;;
    esac
}

EXPECTED="$(pinned_hash "$VERSION" "$PLATFORM-$ARCH")" \
    || { echo "fetch-node-runtime.sh: no pinned SHA-256 for $VERSION/$PLATFORM-$ARCH; add it before bundling" >&2; exit 1; }

case "$PLATFORM" in
    darwin) EXT="tar.gz" ;;
    linux) EXT="tar.xz" ;;
    *) echo "fetch-node-runtime.sh: unsupported platform $PLATFORM" >&2; exit 1 ;;
esac
ARCHIVE="node-v$VERSION-$PLATFORM-$ARCH.$EXT"
URL="https://nodejs.org/dist/v$VERSION/$ARCHIVE"

PROVENANCE="$DEST/PROVENANCE.txt"
if [ -f "$DEST/bin/node" ] && [ "$FORCE" != "1" ] && [ -f "$PROVENANCE" ] && grep -q "$EXPECTED" "$PROVENANCE"; then
    echo "node runtime already staged and verified: $DEST/bin/node"
    exit 0
fi

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "downloading $URL"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$WORK/$ARCHIVE"
else
    wget -q "$URL" -O "$WORK/$ARCHIVE"
fi

ACTUAL="$(sha256_of "$WORK/$ARCHIVE")"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "fetch-node-runtime.sh: SHA-256 mismatch for $ARCHIVE: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
fi
echo "verified SHA-256 $ACTUAL"

mkdir -p "$WORK/extract"
tar -xf "$WORK/$ARCHIVE" -C "$WORK/extract"
ROOT="$WORK/extract/node-v$VERSION-$PLATFORM-$ARCH"
# Use -f (not -x): the execute bit is not preserved when this runs on a
# Windows filesystem, and the app only requires a regular file.
[ -f "$ROOT/bin/node" ] || { echo "fetch-node-runtime.sh: bin/node not found in $ARCHIVE" >&2; exit 1; }
[ -f "$ROOT/LICENSE" ] || { echo "fetch-node-runtime.sh: LICENSE not found in $ARCHIVE" >&2; exit 1; }

mkdir -p "$DEST/bin"
cp -f "$ROOT/bin/node" "$DEST/bin/node"
cp -f "$ROOT/LICENSE" "$DEST/LICENSE"
chmod 0755 "$DEST/bin/node"

cat >"$PROVENANCE" <<EOF
warpi bundled Node.js runtime
version: $VERSION
platform: $PLATFORM-$ARCH
source: $URL
sha256: $EXPECTED
retrieved: $(date -u +%Y-%m-%dT%H:%M:%SZ)
license: LICENSE (Node.js MIT and its bundled dependencies; see https://github.com/nodejs/node/blob/main/LICENSE)
note: this file names the exact artifact the runtime was staged from; it is provenance, not a signature.
EOF

echo "staged $DEST/bin/node ($VERSION $PLATFORM-$ARCH)"
