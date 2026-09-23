# warpi Windows build — GitHub Actions job

Status: **proposal, not run.** This job is written against the repository
layout at revision `e2f3c50d0f`, but it has never been executed. See
`../WINDOWS.md` for the Linux cross-compilation probe that motivated it.

Drop this file into `.github/workflows/build-warpi-windows.yml` (or paste the
`jobs:` block into an existing workflow). It builds the Pi helper, builds the
`warpi` GUI binary on a real Windows runner, and uploads both as artifacts.

```yaml
name: Build warpi (Windows)

on:
  workflow_dispatch:
  # Optional: run on pushes that touch the fork's standalone work.
  push:
    branches: [master, main]
    paths:
      - "app/**"
      - "crates/**"
      - "standalone/**"
      - "Cargo.toml"
      - "Cargo.lock"
      - ".github/workflows/build-warpi-windows.yml"

permissions:
  contents: read

jobs:
  build-warpi-windows:
    name: Build warpi (Windows x64)
    runs-on: windows-latest
    timeout-minutes: 120
    defaults:
      run:
        # The repo's Windows scripts are invoked from Git Bash in
        # .github/actions/prepare_environment, so use the same shell here.
        shell: bash
    steps:
      - name: Check out the repository
        uses: actions/checkout@v4

      - name: Install Rust 1.92.0
        uses: dtolnay/rust-toolchain@stable
        with:
          # Matches rust-toolchain.toml.
          toolchain: 1.92.0

      - name: Cache cargo artifacts
        uses: Swatinem/rust-cache@v2
        with:
          key: windows-warpi

      - name: Set up Node 22.19
        uses: actions/setup-node@v4
        with:
          # The helper requires Node >= 22.19.0 (engines + in-code check).
          node-version: "22.19.0"
          cache: npm
          cache-dependency-path: standalone/pi-helper/package-lock.json

      - name: Install protoc (needed by prost-build)
        shell: pwsh
        run: choco install protoc --no-progress -y

      - name: Install build dependencies (repo script)
        # Installs rustup if missing and runs script/install_cargo_build_deps.
        run: ./script/windows/install_build_deps.ps1

      - name: Install internal channel config (best effort)
        # External forks have no SSH access to warpdotdev/warp-channel-config;
        # the script exits non-zero and the build continues. warpi does not use
        # the generated channel config (app/src/bin/warpi.rs constructs
        # ChannelConfig directly), so this is only for parity with the repo's
        # own setup.
        run: |
          ./script/install_channel_config || \
            echo "Skipping internal channel config installation (no repo access)."

      - name: Build Pi helper
        working-directory: standalone/pi-helper
        run: |
          npm ci
          npm run build
          # npm test  # optional: 13 Node tests

      - name: Build warpi
        env:
          # On a Windows host app/build.rs embeds
          # channels/<CARGO_BIN_NAME>/icon/no-padding/icon.ico via
          # embed-resource. Cargo does not set CARGO_BIN_NAME for build scripts,
          # and the fork has no app/channels/warpi directory; bin.warpi's bundle
          # metadata points at the oss icon. "oss" is therefore the value that
          # matches the fork's intent (script/windows/bundle.ps1 sets the same
          # variable for its channel builds). WARP_APP_NAME goes into the
          # embedded version resource.
          CARGO_BIN_NAME: oss
          WARP_APP_NAME: Warpi
        run: cargo build -p warp --bin warpi --features gui

      - name: Upload warpi executable
        uses: actions/upload-artifact@v4
        with:
          name: warpi-windows-x64-debug
          path: |
            target/debug/warpi.exe
            target/debug/warpi.pdb
          if-no-files-found: error

      - name: Upload Pi helper
        uses: actions/upload-artifact@v4
        with:
          name: warpi-pi-helper
          path: standalone/pi-helper/dist/**
          if-no-files-found: error
```

## Notes for whoever runs it

- **Why Windows and not this Linux box.** The probe in `../WINDOWS.md` shows the
  cross build fails on every native dependency (`aws-lc-sys`,
  `libsqlite3-sys`, 36 `arborium-*` crates, `zstd-sys`, ...) because no Windows
  C toolchain is installed. `windows-latest` has MSVC, the Windows SDK, CMake,
  NASM, Perl, and (after the `choco` step) protoc.
- **Runtime layout.** At runtime the app looks for the helper at
  `standalone/pi-helper/dist/main.js` relative to the executable (or at
  `WARPI_PI_HELPER_ENTRY`); see `../BUILDING.md`. Unpack the two artifacts so
  that path exists, and make sure Node >= 22.19 is installed on the target
  machine.
- **Running the GUI additionally needs** the checked-in Windows payload files
  from `app/assets/windows/x64` (`conpty.dll`, `OpenConsole.exe`,
  `vcruntime140*.dll`, `msvcp140.dll`, `dxcompiler.dll`, `dxil.dll`) copied next
  to the executable. That is what `script/windows/prepare_bundled_resources.ps1`
  and the installer do; this job only produces the binary and helper.
- **Packaging is not covered.** `script/windows/bundle.ps1` and
  `windows-installer.iss` still use the upstream `oss`/`warp-oss`/`WarpOss`
  names and would need warpi entries first (`../WINDOWS.md` section 5).
- **Alternative setup style.** The repo's own
  `.github/actions/prepare_environment` (with `target_os: windows`) already
  installs protoc and runs `script/windows/install_build_deps.ps1`; a job in
  this repository can use it instead of the hand-rolled `choco` step above.
  It expects the private SSH key only in `warpdotdev/warp-internal`, so a fork
  run is fine by default.
- **Cost.** Unknown; the Windows dependency graph is ~1355 crates and the job
  has never been timed. `timeout-minutes: 120` is generous on purpose.
