#!/usr/bin/env bash
# Package lmux.app as a DMG and publish it with a download page at
# https://lmux.lintlabs.org (a Cloudflare Worker serving static assets).
#
#   deploy/site/publish.sh
#
# Run deploy/macos/bundle.sh first; this ships target/bundle/lmux.app as built.
# With a notarytool keychain profile named lmux-notary
# (xcrun notarytool store-credentials lmux-notary …) the DMG is notarized and
# stapled, so it opens without the "Open Anyway" step.
set -euo pipefail

repo=$(git -C "$(dirname "$0")/../.." rev-parse --show-toplevel)
app="$repo/target/bundle/lmux.app"
out="$repo/target/site"
[ -d "$app" ] || { echo "no $app: run deploy/macos/bundle.sh first"; exit 1; }
identity=$(codesign -dvv "$app" 2>&1 | sed -n 's/^Authority=\(Developer ID Application:.*\)/\1/p')
[ -n "$identity" ] || { echo "lmux.app is not signed with a Developer ID; downloads would not open"; exit 1; }

echo "==> dmg"
rm -rf "$out" && mkdir -p "$out/public" "$out/stage"
cp -R "$app" "$out/stage/"
ln -s /Applications "$out/stage/Applications"
hdiutil create -volname lmux -srcfolder "$out/stage" -ov -format UDZO "$out/public/lmux.dmg" >/dev/null
codesign --sign "$identity" --timestamp "$out/public/lmux.dmg"

if xcrun notarytool history --keychain-profile lmux-notary >/dev/null 2>&1; then
  echo "==> notarizing (a few minutes)"
  xcrun notarytool submit "$out/public/lmux.dmg" --keychain-profile lmux-notary --wait
  xcrun stapler staple "$out/public/lmux.dmg"
  notarized=1
else
  echo "==> not notarized (no lmux-notary profile): users click Open Anyway"
  notarized=0
fi

echo "==> page"
version=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$app/Contents/Info.plist")
build=$(/usr/libexec/PlistBuddy -c 'Print CFBundleVersion' "$app/Contents/Info.plist")
sed "s/@VERSION@/$version ($build)/" "$repo/deploy/site/index.html" > "$out/public/index.html"
if [ "$notarized" = 1 ]; then
  # Drop the Gatekeeper note: a notarized download opens normally.
  python3 - "$out/public/index.html" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
open(p, "w").write(re.sub(r'\s*<div class="note" id="gatekeeper">.*?</div>', "", s, flags=re.S))
PY
fi
cp "$repo/deploy/site/lintlabs-logo.svg" "$repo/deploy/site/demo.html" "$out/public/"
sips -z 256 256 -s format png "$app/Contents/Resources/lmux.icns" --out "$out/public/icon.png" >/dev/null
cat > "$out/public/_headers" <<'H'
/lmux.dmg
  Content-Disposition: attachment; filename="lmux.dmg"
  Cache-Control: no-cache
H

echo "==> deploying"
(cd "$repo/deploy/site" && wrangler deploy --assets "$out/public")
echo "==> done: https://lmux.lintlabs.org"
