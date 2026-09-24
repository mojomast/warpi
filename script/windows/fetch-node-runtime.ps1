# Fetch, verify, and stage the pinned Node.js runtime warpi bundles on Windows.
#
# The helper cannot rely on whatever `node.exe` a machine happens to have on
# PATH (see standalone/WINDOWS.md): a missing, old, or misconfigured runtime
# turns every prompt into a startup crash. A release therefore ships a known-good
# Node runtime next to the executable, and the app prefers it over PATH.
#
# This script downloads the *official* nodejs.org archive over HTTPS, verifies
# its SHA-256 against the value pinned below (which is copied from the matching
# https://nodejs.org/dist/v<Version>/SHASUMS256.txt), and stages only the
# runtime (`node.exe`) with Node's own LICENSE plus a provenance record.
#
# Usage:
#   script/windows/fetch-node-runtime.ps1 -Arch x64 -Dest dist/warpi-windows-x86_64/standalone/node
#
# The pinned version/hashes are the single source of truth for what a warpi
# release bundles; bump them together (see standalone/WINDOWS.md).

param(
    [string]$Version = "22.19.0",
    [ValidateSet("x64", "arm64")]
    [string]$Arch = "x64",
    [Parameter(Mandatory = $true)]
    [string]$Dest,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# SHA-256 of node-v<Version>-win-<Arch>.zip, from the official SHASUMS256.txt.
# A hash that is not listed here is refused rather than downloaded unverified.
$PinnedHashes = @{
    "22.19.0/x64"   = "ea3fad0e67a991d8477d8c01344b56e69c676ccb733f065b22436994b1253f86"
    "22.19.0/arm64" = "e4a7336010d58ff35b53d9dd5869095c56089c70913cf22508cf8183593e56b2"
}

$key = "$Version/$Arch"
if (-not $PinnedHashes.ContainsKey($key)) {
    throw "no pinned Node.js SHA-256 for '$key'. Add it to script/windows/fetch-node-runtime.ps1 from https://nodejs.org/dist/v$Version/SHASUMS256.txt before bundling."
}
$expectedHash = $PinnedHashes[$key]

$archiveName = "node-v$Version-win-$Arch.zip"
$url = "https://nodejs.org/dist/v$Version/$archiveName"
$provenancePath = Join-Path $Dest "PROVENANCE.txt"

$nodeExe = Join-Path $Dest "node.exe"
if ((Test-Path $nodeExe) -and -not $Force -and (Test-Path $provenancePath)) {
    if ((Get-Content $provenancePath -Raw) -match [regex]::Escape($expectedHash)) {
        Write-Host "node runtime already staged and verified: $nodeExe"
        exit 0
    }
}

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("warpi-node-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    $archive = Join-Path $work $archiveName
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing

    $actualHash = (Get-FileHash -Path $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "SHA-256 mismatch for ${archiveName}: expected $expectedHash, got $actualHash"
    }
    Write-Host "verified SHA-256 $actualHash"

    $extract = Join-Path $work "extract"
    Expand-Archive -Path $archive -DestinationPath $extract -Force
    $root = Join-Path $extract ("node-v$Version-win-$Arch")
    $sourceExe = Join-Path $root "node.exe"
    $sourceLicense = Join-Path $root "LICENSE"
    if (-not (Test-Path $sourceExe)) { throw "node.exe not found in $archiveName" }
    if (-not (Test-Path $sourceLicense)) { throw "LICENSE not found in $archiveName" }

    Copy-Item -Path $sourceExe -Destination $nodeExe -Force
    Copy-Item -Path $sourceLicense -Destination (Join-Path $Dest "LICENSE") -Force

    $provenance = @(
        "warpi bundled Node.js runtime",
        "version: $Version",
        "architecture: win-$Arch",
        "source: $url",
        "sha256: $expectedHash",
        "retrieved: $(Get-Date -Format 'yyyy-MM-ddTHH:mm:ssK')",
        "license: LICENSE (Node.js MIT and its bundled dependencies; see https://github.com/nodejs/node/blob/main/LICENSE)",
        "note: this file names the exact artifact the runtime was staged from; it is provenance, not a signature."
    ) -join "`n"
    Set-Content -Path $provenancePath -Value $provenance -Encoding UTF8

    Write-Host "staged $nodeExe ($Version win-$Arch)"
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
