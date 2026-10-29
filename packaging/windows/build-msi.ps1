# Builds the InfEditor MSI. Works locally and in CI.
#
# Usage (from anywhere):
#   packaging\windows\build-msi.ps1 [-Arch x64|arm64] [-BinDir <dir>] [-Version 0.1.0]
#
# -BinDir defaults to target/release (local build); CI passes
# target/<triple>/release. -Version defaults to the version in Cargo.toml;
# CI passes the tag-derived version explicitly.
#
# The MSI is written to **dist/** at the repo root (not next to this script,
# not the repo root): dist/ is the one place every package lands — MSI, NSIS
# setup, portable zips, DEB (see release.yml) — and the release workflow
# uploads `dist/InfEditor-*.msi` from there.
param(
    [ValidateSet("x64", "arm64")]
    [string]$Arch = "x64",
    [string]$BinDir = "target\release",
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

# Always resolve repo paths against this script's location so the working
# directory does not matter.
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent (Split-Path -Parent $here)

if (-not $Version) {
    $line = Select-String -Path (Join-Path $repo "Cargo.toml") -Pattern '^version = "(.+)"' |
        Select-Object -First 1
    if (-not $line) { throw "cannot read version from Cargo.toml" }
    $Version = $line.Matches[0].Groups[1].Value
}

if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    # Pinned: WiX v7 gates use behind an OSMF EULA acceptance; v6 has no gate
    # and the UI extension must match the toolset version.
    dotnet tool install --global wix --version 6.0.2
    if ($LASTEXITCODE -ne 0) { throw "wix tool install failed" }
}

# The UI extension (license dialog) is a separate nuget package. `wix
# extension add` re-resolves against nuget.org on every run, so on a machine
# without network it fails even when the extension is already installed.
# Check the global cache first and only add it when it is really missing.
$uiExt = "WixToolset.UI.wixext"
$haveUiExt = $false
try {
    $haveUiExt = [bool](& wix extension list -g | Select-String -Pattern "$uiExt\s+6\.0\.2")
} catch { }
if (-not $haveUiExt) {
    wix extension add -g "$uiExt/6.0.2"
    if ($LASTEXITCODE -ne 0) { throw "wix extension add failed" }
}

Push-Location $here
try {
    # BinDir/RepoRoot come in repo-relative; the .wxs consumes them relative to
    # its own directory, so absolutize both.
    $binAbs = if ([IO.Path]::IsPathRooted($BinDir)) { $BinDir } else { Join-Path $repo $BinDir }
    # Absolute output path: keeps the artifact in dist/ regardless of this
    # script's working directory (CI uploads dist/InfEditor-*.msi from there).
    $dist = Join-Path $repo "dist"
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $msi = Join-Path $dist "InfEditor-$Version-$Arch.msi"
    wix build `
        -arch $Arch `
        -ext WixToolset.UI.wixext `
        -d "ProductVersion=$Version" `
        -d "BinDir=$binAbs" `
        -d "RepoRoot=$repo" `
        -out $msi "InfEditor.wxs"
    if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
    Get-Item $msi | Select-Object Name, Length
}
finally {
    Pop-Location
}
