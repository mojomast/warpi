#!/usr/bin/env bash
# Development-only environment for building the fork on this Linux box.
#
# The stock Ubuntu image here has no sudo, so protoc/cmake/alsa pkg-config are
# provided from the user's home directory instead of apt. These paths are NOT
# part of the packaged application; see BUILDING.md for the real prerequisites.
export PROTOC="${PROTOC:-$HOME/.local/share/protoc/bin/protoc}"
export PATH="$HOME/.local/venvs/tools/bin:$PATH"
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
# This audit machine is disk-constrained; incremental artifacts are large and
# are not needed for correctness. Re-enable locally on a normal machine.
export CARGO_INCREMENTAL=0
# This box has libfontconfig.so.1 but no fontconfig.pc; use the crate's dlopen
# path (which is what the GUI uses at runtime on Linux anyway).
export RUST_FONTCONFIG_DLOPEN=1
