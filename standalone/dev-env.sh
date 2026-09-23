#!/usr/bin/env bash
# Development-only environment for building the fork on this Linux box.
#
# The stock Ubuntu image here has no sudo, so protoc/cmake/alsa pkg-config are
# provided from the user's home directory instead of apt. These paths are NOT
# part of the packaged application; see BUILDING.md for the real prerequisites.
export PROTOC="${PROTOC:-$HOME/.local/share/protoc/bin/protoc}"
export PATH="$HOME/.local/venvs/tools/bin:$PATH"
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
