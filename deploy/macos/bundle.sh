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

echo "==> signing (ad hoc)"
codesign --force --deep --sign - "$app" 2>&1 | grep -v 'replacing existing signature' || true

echo "==> installing to $dest and ~/.local/bin/lmux"
rm -rf "$dest/lmux.app"
cp -R "$app" "$dest/lmux.app"
mkdir -p "$HOME/.local/bin"
install -m755 "$repo/target/release/lmux" "$HOME/.local/bin/lmux"
echo "==> done: open -a lmux"
