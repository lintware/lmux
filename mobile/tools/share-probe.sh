#!/usr/bin/env bash
# Compile the app's networking files for macOS and run them against a share link.
#   mobile/tools/share-probe.sh 'https://x.trycloudflare.com/#code' ['echo hi']
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEV="/Applications/Xcode.app/Contents/Developer"
SWIFTC="${DEV}/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc"
SDK="${DEV}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
OUT="${ROOT}/build/share-probe"
mkdir -p "${ROOT}/build"
"${SWIFTC}" -sdk "${SDK}" -target arm64-apple-macos14.0 -parse-as-library -o "${OUT}" \
  "${ROOT}/tools/share-probe.swift" \
  "${ROOT}/ios/Lmux/Models.swift" \
  "${ROOT}/ios/Lmux/ShareLink.swift" \
  "${ROOT}/ios/Lmux/ShareClient.swift" \
  "${ROOT}/ios/Lmux/Keychain.swift"
exec "${OUT}" "$@"
