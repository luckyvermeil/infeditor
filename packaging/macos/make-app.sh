#!/bin/bash
# InfEditor macOS bundle (unsigned) plus the zip that ships it.
#
# Usage, from the repository root (see .github/workflows/release.yml):
#   packaging/macos/make-app.sh -t aarch64-apple-darwin -v 0.1.0 \
#       -b target/aarch64-apple-darwin/release -o dist
#
# Writes dist/InfEditor-v0.1.0-aarch64-apple-darwin.zip holding one folder:
#   InfEditor.app/            Contents/{Info.plist, MacOS/infeditor, Resources/infeditor.icns}
#   INSTALL-macos.txt         the first-launch step Gatekeeper forces
#
# The bundle is **ad-hoc signed only** (`codesign -s -`): there is no Developer
# ID and no notarization, so the first launch is refused by Gatekeeper on any
# Mac that downloaded the zip. That is a fact of the shipping certificate, not
# something this script can paper over — INSTALL-macos.txt says what to do, and
# removing the quarantine flag silently would teach users to trust anything.
#
# A .app is a directory with a fixed layout; the three files above are the
# whole contract. `-t` only names the artifact (the binary is already built for
# that triple by cargo), so cross-building on one Mac still produces correctly
# named packages for both architectures.
#
# macOS-only by construction (iconutil, sips, ditto, codesign, plutil): the
# Windows and Linux legs never call this.
set -euo pipefail

TARGET=""
VERSION=""
BINDIR=""
OUTDIR="dist"

while getopts "t:v:b:o:" opt; do
  case "$opt" in
    t) TARGET="$OPTARG" ;;
    v) VERSION="$OPTARG" ;;
    b) BINDIR="$OPTARG" ;;
    o) OUTDIR="$OPTARG" ;;
    *) echo "usage: $0 -t <target-triple> [-v <version>] [-b <bindir>] [-o <outdir>]" >&2; exit 2 ;;
  esac
done

# Paths on the command line are repo-relative (the workflow passes
# target/<triple>/release), so resolve the root from this script and work there.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

[ -n "$TARGET" ] || { echo "-t <target-triple> is required" >&2; exit 2; }
[ -n "$BINDIR" ] || { echo "-b <bindir> is required" >&2; exit 2; }
# The version comes from the tag on a release run, from Cargo.toml otherwise.
# `-v` may therefore carry a branch name (workflow_dispatch), which is not a
# version: anything that does not start with an optional "v" and a digit falls
# back, instead of shipping "InfEditor-master.zip" or a plist whose
# CFBundleShortVersionString is "master".
if [ -z "$VERSION" ] || ! printf '%s' "$VERSION" | grep -qE '^v?[0-9]'; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi
VERSION="${VERSION#v}"

BIN="$BINDIR/infeditor"
[ -f "$BIN" ] || { echo "missing binary: $BIN (did the build step run?)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APP="$WORK/InfEditor.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# --- icon -------------------------------------------------------------------
# iconutil wants an .iconset whose ten file names encode the pixel size, so the
# mapping below is the whole story; the PNGs themselves come from
# materials/build.py (which renders each size from the SVG rather than scaling
# one bitmap). A missing size is a hard error: silently shipping a bundle
# without @2x sizes looks like a rendering bug on Retina screens.
ICONSET="$WORK/InfEditor.iconset"
mkdir -p "$ICONSET"
for spec in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
            128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 \
            512:icon_256x256@2x 512:icon_512x512 1024:icon_512x512@2x; do
  size="${spec%%:*}"
  name="${spec#*:}"
  src="packaging/icon/png/infeditor-$size.png"
  [ -f "$src" ] || { echo "missing icon source: $src" >&2; exit 1; }
  cp "$src" "$ICONSET/$name.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/infeditor.icns"

# --- Info.plist -------------------------------------------------------------
# CFBundleIdentifier is the app's identity for LaunchServices, TCC and the
# "Open with" database: it must never change once shipped. No CFBundleDocumentTypes
# yet — file associations are a Windows feature today (see packaging/windows),
# and claiming types macOS cannot honour would be worse than not claiming them.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>   <string>en</string>
    <key>CFBundleDisplayName</key>         <string>InfEditor</string>
    <key>CFBundleName</key>                <string>InfEditor</string>
    <key>CFBundleExecutable</key>          <string>infeditor</string>
    <key>CFBundleIconFile</key>            <string>infeditor.icns</string>
    <key>CFBundleIdentifier</key>          <string>io.github.luckyvermeil.infeditor</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundlePackageType</key>         <string>APPL</string>
    <key>CFBundleShortVersionString</key>  <string>$VERSION</string>
    <key>CFBundleVersion</key>             <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>      <string>11.0</string>
    <key>LSApplicationCategoryType</key>   <string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key>     <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

# --- payload ----------------------------------------------------------------
cp "$BIN" "$APP/Contents/MacOS/infeditor"
chmod +x "$APP/Contents/MacOS/infeditor"
# Strip whatever the CI checkout picked up, then sign the bundle ad hoc. The
# linker already signs the binary on Apple Silicon (an unsigned arm64 binary is
# killed on exec); signing the bundle makes the signature cover Info.plist and
# the icon too, which is what `spctl` and Gatekeeper look at.
xattr -cr "$APP"
codesign --force --sign - "$APP"
codesign --verify --strict "$APP"

# --- zip --------------------------------------------------------------------
# One folder per architecture keeps the download obvious next to the Windows
# zips (which are bare .exe): the user sees the app and the note together.
STAGE="$WORK/InfEditor-$VERSION-$TARGET"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/InfEditor.app"

cat > "$STAGE/INSTALL-macos.txt" <<NOTE
InfEditor $VERSION ($TARGET)
====================================================

1. Move InfEditor.app to /Applications (optional).

2. First launch: right-click (or Control-click) the app, choose Open, then
   Open again. Double-clicking first shows "cannot be opened because the
   developer cannot be verified" — this build is not notarized (no Apple
   Developer ID), so Gatekeeper asks once and remembers the answer.

   Same thing from a terminal:

     xattr -dr com.apple.quarantine "/Applications/InfEditor.app"

   macOS 15 also gates it behind System Settings -> Privacy & Security ->
   "Open Anyway" after the first refusal.

中文
----------------------------------------------------

1. 可选：把 InfEditor.app 拖到「应用程序」。

2. 首次打开：右键（或 Control-点按）App，选择「打开」，再确认一次「打开」。
   直接双击会提示「无法验证开发者」——此包没有 Apple 开发者签名、未做公证，
   系统只会问这一次，之后照常双击。

   也可以在终端里执行：

     xattr -dr com.apple.quarantine "/Applications/InfEditor.app"

   macOS 15 还会在第一次被拒后，要求到「系统设置 → 隐私与安全性 → 仍要打开」。
NOTE

mkdir -p "$OUTDIR"
ZIP="$OUTDIR/InfEditor-v$VERSION-$TARGET.zip"
rm -f "$ZIP"
# ditto (not zip) keeps the bundle's resource forks, symlinks and modes intact;
# --sequesterRsrc keeps a stray ._ file inside the archive instead of next to it.
ditto -c -k --sequesterRsrc --keepParent "$STAGE" "$ZIP"

echo "app bundle: $APP"
echo "package:    $ZIP ($(du -h "$ZIP" | cut -f1))"
