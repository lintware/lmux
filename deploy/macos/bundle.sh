#!/usr/bin/env bash
# Build lmux.app and install it, so lmux launches from the Dock, Spotlight or
# Finder like cmux does.
#
#   deploy/macos/bundle.sh [install-dir]      (default: /Applications)
#
# The bundle holds the app and the `lmux` daemon/CLI side by side in
# Contents/MacOS: the app starts the local daemon from its sibling. The same
# `lmux` is also installed to ~/.local/bin, where the Claude hooks and your
# shell find it. Re-running updates both in place; a running local daemon
# keeps running until it is restarted (its tmux sessions survive that).
# LMUX_SKIP_INSTALL=1 stops after assembling and signing target/bundle/lmux.app.
set -euo pipefail

dest=${1:-/Applications}
repo=$(git -C "$(dirname "$0")/../.." rev-parse --show-toplevel)
build="$repo/target/bundle"
app="$build/lmux.app"

echo "==> building release binaries"
(cd "$repo" && cargo build --release -p lmux 2>&1 | tail -1)
(cd "$repo/app" && cargo build --release 2>&1 | tail -1)

echo "==> assembling lmux.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$repo/app/target/release/lmux-app" "$app/Contents/MacOS/lmux-app"
cp "$repo/target/release/lmux" "$app/Contents/MacOS/lmux"
# tmux ships in the app, so a fresh Mac needs no Homebrew; the app puts this
# directory first on the daemon's PATH.
"$repo/deploy/macos/build-tmux.sh" "$app/Contents/MacOS"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/app/Cargo.toml" | head -1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>lmux</string>
  <key>CFBundleDisplayName</key><string>lmux</string>
  <key>CFBundleIdentifier</key><string>dev.lmux.app</string>
  <key>CFBundleExecutable</key><string>lmux-app</string>
  <key>CFBundleIconFile</key><string>lmux</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version:-0.1.0}</string>
  <key>CFBundleVersion</key><string>$(git -C "$repo" rev-list --count HEAD)</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSDocumentsFolderUsageDescription</key><string>Terminals in lmux open and run programs in your folders.</string>
  <key>NSDesktopFolderUsageDescription</key><string>Terminals in lmux open and run programs in your folders.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>Terminals in lmux open and run programs in your folders.</string>
</dict>
</plist>
PLIST

echo "==> remote installer and source"
# "Add Remote Host" installs lmux on the host from these.
cp "$repo/deploy/install-remote.sh" "$app/Contents/Resources/install-remote.sh"
mkdir -p "$app/Contents/Resources/cloud"
cp "$repo"/deploy/cloud/*.sh "$app/Contents/Resources/cloud/"
git -C "$repo" archive --format=tar HEAD > "$app/Contents/Resources/lmux-src.tar"

echo "==> icon"
icon_src="$build/icon-1024.png"
swift "$repo/deploy/macos/make-icon.swift" "$icon_src"
iconset="$build/lmux.iconset"
rm -rf "$iconset" && mkdir -p "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s "$icon_src" --out "$iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$icon_src" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/lmux.icns"

# macOS remembers privacy grants (Documents, Desktop…) by signing identity.
# An ad-hoc signature changes with every build, so each reinstall would ask
# again; a certificate keeps the grant. LMUX_SIGN_IDENTITY picks one, else the
# first Developer ID Application, then Apple Development, then ad hoc.
identity=${LMUX_SIGN_IDENTITY:-}
if [ -z "$identity" ]; then
  ids=$(security find-identity -v -p codesigning 2>/dev/null || true)
  identity=$(printf '%s\n' "$ids" | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)
  [ -n "$identity" ] || identity=$(printf '%s\n' "$ids" | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)
fi
if [ -n "$identity" ] && [ "$identity" != - ]; then
  echo "==> signing as $identity"
  # Hardened runtime and a timestamp: what notarization needs. Inner
  # binary first; --deep is deprecated for signing.
  sign() { codesign --force --options runtime --timestamp --sign "$identity" "$@"; }
  sign --identifier dev.lmux.cli "$app/Contents/MacOS/lmux"
  sign --identifier dev.lmux.tmux "$app/Contents/MacOS/tmux"
  sign "$app"
else
  echo "==> signing (ad hoc: no certificate, so macOS asks for folder access after each reinstall)"
  codesign --force --sign - "$app/Contents/MacOS/lmux"
  codesign --force --sign - "$app/Contents/MacOS/tmux"
  codesign --force --sign - "$app"
fi
codesign --verify --strict "$app"

# Release builds and trial builds want the bundle without replacing the lmux
# that's installed and running.
if [ -n "${LMUX_SKIP_INSTALL:-}" ]; then
  echo "==> built $app (LMUX_SKIP_INSTALL: not installed)"
  exit 0
fi

echo "==> installing to $dest and ~/.local/bin/lmux"
rm -rf "$dest/lmux.app"
cp -R "$app" "$dest/lmux.app"
mkdir -p "$HOME/.local/bin"
# The signed copy, so the CLI and daemon carry the same identity as the app.
install -m755 "$app/Contents/MacOS/lmux" "$HOME/.local/bin/lmux"
echo "==> done: open -a lmux"
