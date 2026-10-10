#!/usr/bin/env bash
# Cut a GitHub release: build and sign lmux.app, package it, and publish the
# DMG, the CLI/daemon tarball and checksums under the tag.
#
#   deploy/release.sh v0.2.0 [--draft]
#
# Run it on the commit to release, checked out on main and pushed. It refuses
# to run when the tree is dirty, when the tag doesn't match the version in
# Cargo.toml and app/Cargo.toml, or when HEAD isn't what origin/main has.
#
# The release notes are the matching section of CHANGELOG.md. With a notarytool
# keychain profile named lmux-notary (xcrun notarytool store-credentials
# lmux-notary …) the DMG is notarized and stapled. Without one it is signed
# with the Developer ID only, and the notes say that macOS asks for Open Anyway.
#
# Needs: cargo, a Developer ID Application certificate, gh (logged in).
set -euo pipefail

tag=${1:?usage: release.sh vX.Y.Z [--draft]}
draft=${2:-}
case "$tag" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "tag must look like v1.2.3"; exit 1 ;; esac
version=${tag#v}

repo=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
cd "$repo"

echo "==> checking"
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean"; exit 1; }
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "releases are cut from main"; exit 1; }
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "HEAD is not origin/main: push or pull first"; exit 1; }
ws=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app_v=$(sed -n 's/^version = "\(.*\)"/\1/p' app/Cargo.toml | head -1)
[ "$ws" = "$version" ] && [ "$app_v" = "$version" ] || {
  echo "Cargo.toml says $ws and app/Cargo.toml says $app_v, but the tag is $tag"; exit 1; }
if gh release view "$tag" >/dev/null 2>&1; then echo "release $tag already exists"; exit 1; fi

notes=$(awk -v v="$version" '
  /^## / { if (on) exit; if ($2 == v || $2 == "[" v "]") { on = 1; next } }
  on { print }' CHANGELOG.md)
[ -n "$(printf '%s' "$notes" | tr -d '[:space:]')" ] || { echo "CHANGELOG.md has no section for $version"; exit 1; }

echo "==> building lmux.app"
LMUX_SKIP_INSTALL=1 deploy/macos/bundle.sh
app="$repo/target/bundle/lmux.app"
identity=$(codesign -dvv "$app" 2>&1 | sed -n 's/^Authority=\(Developer ID Application:.*\)/\1/p')
[ -n "$identity" ] || { echo "lmux.app is not signed with a Developer ID"; exit 1; }
arch=$(lipo -archs "$app/Contents/MacOS/lmux")
case "$arch" in arm64) arch=arm64 ;; x86_64) arch=x86_64 ;; *) echo "unexpected architectures: $arch"; exit 1 ;; esac

out="$repo/target/release-$version"
rm -rf "$out" && mkdir -p "$out/stage"
name="lmux-$version-macos-$arch"

echo "==> $name.dmg"
cp -R "$app" "$out/stage/"
ln -s /Applications "$out/stage/Applications"
hdiutil create -volname lmux -srcfolder "$out/stage" -ov -format UDZO "$out/$name.dmg" >/dev/null
codesign --sign "$identity" --timestamp "$out/$name.dmg"

if xcrun notarytool history --keychain-profile lmux-notary >/dev/null 2>&1; then
  echo "==> notarizing (a few minutes)"
  xcrun notarytool submit "$out/$name.dmg" --keychain-profile lmux-notary --wait
  xcrun stapler staple "$out/$name.dmg"
  gatekeeper="The DMG is notarized."
else
  gatekeeper="The DMG is signed with a Developer ID but not notarized yet: on first launch, right-click lmux.app and choose Open, or allow it in System Settings → Privacy & Security."
fi

echo "==> $name-cli.tar.gz"
# The signed daemon/CLI and the tmux it runs, for hosts that want no app.
cli="$out/cli/lmux-$version-macos-$arch"
mkdir -p "$cli"
cp "$app/Contents/MacOS/lmux" "$app/Contents/MacOS/tmux" "$cli/"
cp LICENSE "$cli/"
tar -C "$out/cli" -czf "$out/$name-cli.tar.gz" "lmux-$version-macos-$arch"

echo "==> checksums"
(cd "$out" && shasum -a 256 "$name.dmg" "$name-cli.tar.gz" > SHA256SUMS && cat SHA256SUMS)

{
  printf '%s\n\n' "$notes"
  printf '## Downloads\n\n'
  printf -- '- **%s.dmg**: the macOS app (Apple silicon, macOS 12 or later). tmux is inside. %s\n' "$name" "$gatekeeper"
  printf -- '- **%s-cli.tar.gz**: the `lmux` CLI and daemon with its tmux, for a Mac host that runs no app.\n' "$name"
  printf -- '- **SHA256SUMS**: checksums of the files above.\n\n'
  printf 'Linux and other hosts: `deploy/install-remote.sh <ssh-host>` builds lmux there from this tag (or use *Add Remote Host…* in the app).\n'
} > "$out/notes.md"

echo "==> publishing $tag"
if ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  git tag -a "$tag" -m "lmux $version"
fi
git push origin "refs/tags/$tag"
gh release create "$tag" \
  "$out/$name.dmg" "$out/$name-cli.tar.gz" "$out/SHA256SUMS" \
  --title "lmux $version" --notes-file "$out/notes.md" --verify-tag \
  ${draft:+--draft}
echo "==> done: $(gh release view "$tag" --json url --jq .url)"
