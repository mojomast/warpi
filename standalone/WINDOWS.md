# Windows build status for warpi

Probe run on the Linux audit machine (`<fork checkout>`, revision
`e2f3c50d0fd02b96336c9aa86eedfe7f86aed6f7`) on 2026-09-23, using the shim
environment `standalone/dev-env.sh`. **No Windows binary was produced and no
Windows code was executed on this machine.** This document records how far a
cross-target build got, what stops it, and what a real Windows build needs.

## Bottom line

| # | Attempt | Result |
| --- | --- | --- |
| 1 | Windows std for the repo's pinned toolchain | **Initially absent** for the pinned `1.92.0` toolchain (present only for the default `stable` toolchain). Added to the pinned toolchain to continue the probe. |
| 2 | `cargo check -p standalone_agent --target x86_64-pc-windows-gnu` | **PASS** — exit 0, no errors, 20.57 s. |
| 3 | `cargo check -p warp --bin warpi --features gui --target x86_64-pc-windows-gnu` | **FAIL** — exit 101. First real blocker: `aws-lc-sys` cannot find `x86_64-w64-mingw32-gcc`; a `--keep-going` run completes with 43 build-script failures (all native C/asm deps). |
| 4 | Windows cross toolchain on this box | **None present**: no mingw-w64, no cargo-xwin/xwin, no `llvm-rc`/`windres`/`dlltool`, no clang/MSVC. |
| 5 | Windows packaging scripts vs. the `warpi` rename | **Updated 2026-09-24**: `bundle.ps1` and `windows-installer.iss` now carry the `warpi` channel (bin `warpi`, app `Warpi`, `warpi.cmd`, `Warpi` mutex); the installer *script* compiles under Inno Setup 6.7.1 (verified locally under Wine for the warpi/dev/stable channels) and is wired into CI, but the produced installer has not been run. |
| 6 | Pi helper (`standalone/pi-helper`) platform independence | **Yes at the JS level**: `dist/` is plain ESM JavaScript; native dependencies are prebuilt per-platform optional packages (win32 variants present in `package-lock.json`); no node-gyp build. Not executed on Windows. |
| 7 | Disk headroom | Started at 12 GB free; ended at 7.4 GB after the full GUI run. Free space never approached the 1.5 GB abort threshold. |

Recommended path: build on a real Windows runner, not on this Linux box — see
`ci/warpi-windows-job.md` for a GitHub Actions `windows-latest` job.

## 1. Rust targets

`rust-toolchain.toml` pins the workspace to **1.92.0**, and that pinned toolchain
is not the rustup default:

```
$ rustup show
active toolchain
----------------
name: 1.92.0-x86_64-unknown-linux-gnu
active because: overridden by '<fork checkout>/rust-toolchain.toml'
installed targets:
  x86_64-unknown-linux-gnu
```

The Windows target was installed, but for the *default* `stable` toolchain only:

```
$ rustup target list --installed
x86_64-unknown-linux-gnu

$ rustup target list --installed --toolchain stable-x86_64-unknown-linux-gnu
wasm32-unknown-unknown
x86_64-pc-windows-gnu
x86_64-unknown-linux-gnu
```

`cargo check --target x86_64-pc-windows-gnu` run from the repo therefore failed
immediately, before compiling anything, with:

```
error[E0463]: can't find crate for `core`
  |
  = note: the `x86_64-pc-windows-gnu` target may not be installed
  = help: consider downloading the target with `rustup target add x86_64-pc-windows-gnu`
```

To make the probe meaningful, the std component was added to the pinned
toolchain (environment change only; no repository files were touched):

```
$ rustup target add x86_64-pc-windows-gnu --toolchain 1.92.0-x86_64-unknown-linux-gnu
info: downloading component 'rust-std' for 'x86_64-pc-windows-gnu'
info: installing component 'rust-std' for 'x86_64-pc-windows-gnu'

$ rustup target list --installed --toolchain 1.92.0-x86_64-unknown-linux-gnu
x86_64-pc-windows-gnu
x86_64-unknown-linux-gnu
```

All later results use `rustc 1.92.0 (ded5c06cf 2025-12-08)`, the version the
repository pins.

## 2. `standalone_agent` cross-check: PASS

```
$ source standalone/dev-env.sh
$ cargo check -p standalone_agent --target x86_64-pc-windows-gnu
    Checking standalone_agent v0.1.0 (<fork checkout>/crates/standalone_agent)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.57s
$ echo $?
0
```

No errors or warnings were emitted after the target std was installed. This is a
`cargo check` only: no Windows DLL/linker step ran and nothing was executed.

## 3. GUI cross-check: FAIL

```
$ cargo check -p warp --bin warpi --features gui --target x86_64-pc-windows-gnu
error: failed to run custom build command for `aws-lc-sys v0.39.1`

Caused by:
  process didn't exit successfully: `.../target/debug/build/aws-lc-sys-90b3891f3a51f056/build-script-main` (exit status: 1)
  --- stdout
  ...
  cargo:warning=Compiler family detection failed due to error: ToolNotFound: failed to find tool "x86_64-w64-mingw32-gcc": No such file or directory (os error 2)
  ...
  --- stderr
  error occurred in cc-rs: failed to find tool "x86_64-w64-mingw32-gcc": No such file or directory (os error 2)

warning: build failed, waiting for other jobs to finish...
```

Exit code 101 after about 30 seconds. The dependency path (from
`cargo tree -i aws-lc-sys --target x86_64-pc-windows-gnu`) is:

```
aws-lc-sys <- aws-lc-rs <- rustls <- aws-smithy-http-client <- aws-smithy-runtime
           <- aws-config <- warp
```

(and via `reqwest`/`http_client` for the HTTP client side). The Windows
dependency graph has ~1355 nodes.

A second run with `--keep-going` ran to completion with exit code 101. It
produced **43 failing crates, every one of them a build script** (no Rust
compile errors at all), all because `x86_64-w64-mingw32-gcc` is missing:

- `aws-lc-sys v0.39.1` (also needs NASM/Perl for x64 assembly; not present)
- `libsqlite3-sys v0.33.0` (bundled SQLite)
- `libz-sys v1.1.25`, `bzip2-sys v0.1.13+1.0.8`, `lzma-sys v0.1.20`, `zstd-sys v2.0.15+zstd.1.5.7`
- `onig_sys v69.9.1`
- 36 `arborium-* v2.13.0` grammar crates (tree-sitter C parsers), e.g. `arborium-javascript`, `arborium-python`, `arborium-rust`

(`libgit2-sys` is in the Windows graph but did not appear among the failures;
its build script may not have been reached given the other failures. Either way
it is a `cc`-based crate and would hit the same missing compiler.)

Two further caveats that a `cargo check` cross-compile hides:

- `cargo check` never links, so the missing Windows linker
  (`x86_64-w64-mingw32-gcc`, or `rust-lld` plus import libraries) was never
  reached. A real `cargo build` needs it.
- `app/build.rs` embeds the icon/version resource only when the *host* is
  Windows (`#[cfg(windows)]`), so the cross-check never exercised
  `embed-resource` (which needs `rc.exe`/`windres`). A build on a real Windows
  host will. Note also that Cargo does **not** set `CARGO_BIN_NAME` for build
  scripts (verified with a scratch crate), so without `CARGO_BIN_NAME` set
  explicitly the resource step falls back to `channels/local/...` and embeds
  the local-channel icon. As of 2026-09-23 the fork **does** have
  `app/channels/warpi/icon/no-padding/icon.ico`, and
  `app/Cargo.toml`'s `[package.metadata.bundle.bin.warpi]` points at
  `channels/warpi/icon/icon.icns`, so `CARGO_BIN_NAME=warpi` (plus
  `WARP_APP_NAME=Warpi`) is the value the Windows CI build and `bundle.ps1`'s
  `warpi` channel now set.

## 4. Tooling this machine lacks

`command -v` results (with `standalone/dev-env.sh` sourced):

| Tool | Status |
| --- | --- |
| `x86_64-w64-mingw32-gcc` / `-ar` / `-ld` / `-dlltool` / `-windres` | MISSING |
| `cargo-xwin`, `xwin` | MISSING |
| `llvm-rc`, `windres`, `dlltool`, `llvm-dlltool`, `lld-link` | MISSING |
| `clang`, `clang-cl`, `nasm` | MISSING |
| MSVC / Windows SDK (`cl.exe`, `rc.exe`) | not applicable on Linux |
| `cmake` | present via shim (`~/.local/venvs/tools/bin/cmake`, 3.31.6) |
| `protoc` | present as `$PROTOC` (`~/.local/share/protoc/bin/protoc`, libprotoc 25.1), not on `PATH` |
| `node` / `npm` | present (`node v22.19.0`) |
| `rust-lld` | ships in the Rust toolchain, but is not a C compiler |

`dpkg -l | grep mingw` is empty, and this machine has no sudo, so a mingw-w64
cross toolchain cannot be installed here with the approved workflow.

## 5. Windows packaging scripts vs. the rename (updated 2026-09-23)

This section originally recorded the stale state; the bundlers have since been
updated. The probe results below are retained as history.

`script/windows/bundle.ps1`:
- `-Channel` now accepts `local | dev | preview | stable | warpi`; the stale
  `oss` value is gone.
- The `warpi` branch sets `$WARP_BIN = 'warpi'`, `$BINARY_NAME = 'warpi.exe'`,
  `$APP_NAME = 'Warpi'`, `$FEATURES = 'release_bundle,gui'` (no Sentry). The GUI
  bin `warpi` exists (`app/src/bin/warpi.rs`, `Channel::Warpi`).
- The TUI switches use `warpi-tui` / `WarpiAgentCLI` / `CLI_NAME = 'warpi'` /
  `tui-warpi`, matching `Channel::Warpi`'s command names.
- The script sets `CARGO_BIN_NAME`/`WARP_APP_NAME` from the channel name, and
  derives the installer name as `<AppName>Setup.exe` (so `WarpiSetup.exe`).

`script/windows/windows-installer.iss`:
- Defaults `MyAppExeName = "dev.exe"`, `ReleaseChannel = "dev"`.
- Channel set in the preprocessor checks: `stable`, `dev`, `preview`, `local`,
  `integration`, `warpi` — the `oss` branch was replaced by `warpi`, whose
  mutex/`ChannelPascalCase` is `Warpi` (matching `single_instance_manager.rs`)
  and whose CLI shim is `warpi.cmd` (matching `Channel::cli_command_name`).
- The shortcut/AppUserModelID namespace now uses an `AppIdPrefix` that is
  `dev.warpi` for warpi and `dev.warp` otherwise; the registry key remains
  `SOFTWARE\Warp.dev\{#MyAppName}`.
- It requires icons at `app\channels\{#ReleaseChannel}\icon\no-padding\icon.ico`
  (the `warpi` channel directory now exists) and payload DLLs from
  `app\assets\windows\{Arch}`.
- `[Files]` now also stages warpi's private Pi helper
  (`standalone/pi-helper/{dist,package.json,package-lock.json,node_modules}`)
  next to the executable with `skipifsourcedoesntexist`, so the installer can
  resolve `{app}\standalone\pi-helper\dist\main.js`; the `resources\*` entry is
  likewise skippable.

As of 2026-09-24 `.github/workflows/warpi-build.yml` builds `WarpiSetup.exe`
via Inno Setup on `windows-latest` from the staged bundle. The
`windows-installer.iss` **script now compiles under Inno Setup 6.7.1** (verified
locally under Wine for the `warpi`/`dev`/`stable` channels), but **the produced
installer has still never been run/installed on Windows**, so it remains
end-to-end unverified. It is not Authenticode-signed; CI attaches a GitHub
build-provenance attestation (not a signature) to the uploaded file.

The original compile failure was not the `#elif` or the `AppIdPrefix`
conditional: a standalone `;` comment placed between the backslash-continued
`ChannelPascalCase` define and `[Setup]` corrupted the `[Code]` section's line
accounting. That comment is relocated; the `#elif` is also rewritten as the
simple `#if`/`#else`/`#endif` form already used elsewhere in the file, and the
missing semicolon in the `stable` branch is fixed.


Conclusion (updated 2026-09-24): the warpi entries the first probe called for
are now in both scripts, and the CI job builds the installer. The installer
script compiles under Inno Setup 6.7.1 (verified locally under Wine); what
remains is running the produced installer on a real Windows host. It is
unsigned (provenance-attested instead).

## 6. Pi helper platform independence

`standalone/pi-helper/dist/` contains `main.js`, `protocol.js`, `runtime.js`,
`workspace-tools.js` (plus `.d.ts`/source maps). `main.js` is plain ESM
JavaScript with a `#!/usr/bin/env node` shebang that imports only `node:*`
builtins and the sibling `./protocol.js`/`./runtime.js`. `node --check` passes
for all four files. There are no `.node` imports in `dist/`.

Native modules exist only in `node_modules`, all as **prebuilt** binaries:

- `@mariozechner/clipboard-linux-x64-gnu` (NAPI-RS prebuilt, loaded per
  `process.platform` by `@mariozechner/clipboard`)
- `@earendil-works/pi-tui` ships its own prebuilds: `darwin-x64`, `darwin-arm64`,
  `win32-x64`, `win32-arm64` (`*-modifiers.node` / `win32-console-mode.node`)

`package-lock.json` lists the win32 clipboard variants
(`@mariozechner/clipboard-win32-x64-msvc`, `-win32-arm64-msvc`) as optional
dependencies, and contains zero `gypfile`/`node-gyp` entries. So on Windows,
`npm ci` fetches prebuilt win32 binaries — no native compilation, no Python,
no C toolchain. The runtime requirement is Node >= 22.19.0 (helper
`engines` and the in-code version check); the local Node is v22.19.0. The one
platform-specific devDependency (`@esbuild/linux-x64`, via `tsx`) is only used
by the test runner, not by `npm run build` or the helper at runtime.

Not verified: actually running the helper on Windows (or the app with it).

## 7. Disk cost

| Point | Free on `/` |
| --- | --- |
| Before any cross build | 12 GB |
| After `standalone_agent` cross-check | 11 GB |
| After the first GUI cross-check (30 s, exit 101) | 9.4 GB |
| After the full `--keep-going` GUI run (exit 101, 43 failed build scripts) | 7.4 GB |

Free space fell from 12 GB to 7.4 GB across the three runs (including downloaded
crates and the rustup std component). The `target/` directory grew from ~7.8 GB
to ~9.8 GB; the `x86_64-pc-windows-gnu` subdirectory itself is ~640 MB.

## What a real Windows build requires

On a Windows host (the project's supported configuration):

- Rust 1.92.0 (rustup installs it from `rust-toolchain.toml`) with the default
  `x86_64-pc-windows-msvc` host target.
- MSVC C/C++ toolchain + Windows SDK (provides `cl.exe`, `rc.exe`,
  `link.exe`); GitHub's `windows-latest` image has these.
- CMake, NASM, and Perl for `aws-lc-sys`; `protoc` for `prost-build`
  (the repo's own `prepare_environment` action installs it with
  `choco: protoc`, with a winget fallback).
- Node >= 22.19.0 for the helper (`npm ci && npm run build`).
- The checked-in Windows payload DLLs under `app/assets/windows/x64` (already in
  the repository).

Cross-compiling from this Linux box is not recommended: it would need
mingw-w64 (gcc/binutils, unavailable without sudo), NASM/Perl for `aws-lc-sys`,
and `CARGO_BIN_NAME`/resource-compiler handling for `embed-resource`; and the
result would be a GNU-ABI binary, not the MSVC build the packaging scripts
expect.

## Recommended path

1. Use the GitHub Actions job in `ci/warpi-windows-job.md` (or a real Windows
   machine with the same prerequisites). It builds the helper, then
   `cargo build -p warp --bin warpi --features gui`, and uploads both.
2. Validate the binary on Windows before touching packaging. Then update
   `script/windows/bundle.ps1` and `windows-installer.iss` for the `warpi`
   channel (currently unchanged; see section 5).

## Not verified

- No `warpi.exe` was produced or executed; no Windows GUI, agent turn, or
  helper session has ever run on Windows.
- `aws-lc-sys`, `libsqlite3-sys`, the `arborium-*` crates, `zstd-sys`, etc. were
  not shown to build on a Windows host; only their Linux-host cross-compilation
  was shown to fail for lack of a cross C compiler.
- The `windows-latest` job was written against the repository layout but has
  never been run.
- The helper was not executed on Windows; its native win32 dependencies were
  inspected in the lockfile only.
- Packaging/installer behavior for `warpi` on Windows is untested: the scripts
  are updated and the installer script compiles under Inno Setup 6.7.1 (local,
  under Wine), but no installer has been run and no `warpi.exe` has executed
  (section 5).

## Reproduce

```bash
cd <fork checkout>
source standalone/dev-env.sh

# 1. target availability
rustup target list --installed
rustup target list --installed --toolchain stable-x86_64-unknown-linux-gnu

# 2. pure-Rust cross-check (PASS)
cargo check -p standalone_agent --target x86_64-pc-windows-gnu

# 3. GUI cross-check (FAIL at aws-lc-sys)
cargo check -p warp --bin warpi --features gui --target x86_64-pc-windows-gnu

# 4. tool inventory
for t in x86_64-w64-mingw32-gcc cargo-xwin xwin llvm-rc windres dlltool nasm; do
  command -v "$t" || echo "$t MISSING"
done

# 6. helper
ls standalone/pi-helper/dist
for f in standalone/pi-helper/dist/*.js; do node --check "$f"; done
find standalone/pi-helper/node_modules -name '*.node'
```
