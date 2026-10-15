#!/usr/bin/env bash
# Build and install lmux on the iPhone without xcodebuild.
#
# `xcodebuild` and `xcrun` refuse to run until the Xcode licence is accepted, but the
# toolchain binaries underneath them do not check it. So this calls swiftc directly,
# signs with the Apple Development identity, embeds the team's wildcard provisioning
# profile, and installs over USB with pymobiledevice3, or over Wi-Fi with devicectl.
#
#   ./build-ios.sh               build, sign, install on the phone
#   ./build-ios.sh --no-install  build and sign only (or NO_INSTALL=1)
#   SIM=1 ./build-ios.sh         unsigned arm64 simulator build, no install
set -euo pipefail

NO_INSTALL="${NO_INSTALL:-}"
SIM="${SIM:-}"
for arg in "$@"; do
  case "${arg}" in
    --no-install) NO_INSTALL=1 ;;
    *) echo "unknown argument: ${arg}" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="${ROOT}/ios/Lmux"
BUILD="${ROOT}/build"
APP="${BUILD}/Lmux.app"

DEV_ID="${DEV_ID:-00008140-00043D811E13001C}"
BUNDLE_ID="${BUNDLE_ID:-com.vas.lmux}"
TEAM_ID="UTSTY3J6NS"
IDENTITY="${IDENTITY:-Apple Development: Vasanth Sreeram (C8T8853TKY)}"
# The team's wildcard ("*") development profile. Xcode renews it under a new file
# name, so find it by its application identifier.
find_profile() {
  local dir="${HOME}/Library/Developer/Xcode/UserData/Provisioning Profiles" f
  for f in "${dir}"/*.mobileprovision; do
    [ -e "${f}" ] || continue
    if /usr/bin/security cms -D -i "${f}" 2>/dev/null | grep -q "<string>${TEAM_ID}\.\*</string>"; then
      echo "${f}"; return
    fi
  done
  echo "${dir}/(no wildcard profile for ${TEAM_ID})"
}
PROFILE="${PROFILE:-$(find_profile)}"

DEVELOPER="/Applications/Xcode.app/Contents/Developer"
TOOLCHAIN="${DEVELOPER}/Toolchains/XcodeDefault.xctoolchain/usr/bin"
MACOS_SDK="${DEVELOPER}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
PYMOBILE="${PYMOBILE:-/tmp/lmux-ios/bin/pymobiledevice3}"
FONT_SRC="${ROOT}/../app/assets/fonts/JetBrainsMono.ttf"

if [ -n "${SIM}" ]; then
  SDK="${DEVELOPER}/Platforms/iPhoneSimulator.platform/Developer/SDKs/iPhoneSimulator.sdk"
  TARGET="arm64-apple-ios17.0-simulator"
  PLATFORM="iPhoneSimulator"
  APP="${BUILD}/sim/Lmux.app"
  REQUIRED=("${TOOLCHAIN}/swiftc" "${SDK}" "${FONT_SRC}")
else
  SDK="${DEVELOPER}/Platforms/iPhoneOS.platform/Developer/SDKs/iPhoneOS.sdk"
  TARGET="arm64-apple-ios17.0"
  PLATFORM="iPhoneOS"
  REQUIRED=("${TOOLCHAIN}/swiftc" "${SDK}" "${FONT_SRC}" "${PROFILE}")
fi

for required in "${REQUIRED[@]}"; do
  [ -e "${required}" ] || { echo "missing: ${required}" >&2; exit 1; }
done

rm -rf "${APP}"
mkdir -p "${APP}"

# App icon as loose PNGs referenced by CFBundleIcons. This is the pre-asset-catalog
# route, so no actool / Assets.car is needed. Without app/assets/icon.png, draw the
# desktop app's icon with deploy/macos/make-icon.swift. It is compiled with the
# toolchain swiftc because the `swift` shim needs the accepted Xcode licence.
ICON_SRC="${ROOT}/../app/assets/icon.png"
if [ ! -f "${ICON_SRC}" ]; then
  MAKE_ICON="${ROOT}/../deploy/macos/make-icon.swift"
  ICON_SRC="${BUILD}/icon-1024.png"
  if [ ! -f "${ICON_SRC}" ] || [ "${MAKE_ICON}" -nt "${ICON_SRC}" ]; then
    echo "==> generating icon"
    mkdir -p "${BUILD}"
    "${TOOLCHAIN}/swiftc" -sdk "${MACOS_SDK}" -O -o "${BUILD}/make-icon" "${MAKE_ICON}"
    "${BUILD}/make-icon" "${ICON_SRC}" >/dev/null
  fi
fi
if [ -f "${ICON_SRC}" ]; then
  echo "==> icons"
  while read -r size name; do
    [ -n "${name}" ] || continue
    /usr/bin/sips -z "${size}" "${size}" "${ICON_SRC}" --out "${APP}/${name}.png" >/dev/null
  done <<'ICONS'
180 AppIcon60x60@3x
120 AppIcon60x60@2x
120 AppIcon40x40@3x
80 AppIcon40x40@2x
87 AppIcon29x29@3x
58 AppIcon29x29@2x
60 AppIcon20x20@3x
40 AppIcon20x20@2x
ICONS
fi

echo "==> font"
cp "${FONT_SRC}" "${APP}/JetBrainsMono.ttf"

echo "==> compiling"
EXTRA_LINK=()
if [ -n "${SIM}" ]; then
  # The simulator reads entitlements from this section (what Xcode does); without a
  # Keychain access group SecItem calls fail with -34018.
  cat > "${BUILD}/sim-entitlements.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>application-identifier</key><string>${TEAM_ID}.${BUNDLE_ID}</string>
  <key>keychain-access-groups</key><array><string>${TEAM_ID}.${BUNDLE_ID}</string></array>
</dict>
</plist>
PLIST
  EXTRA_LINK=(-Xlinker -sectcreate -Xlinker __TEXT -Xlinker __entitlements -Xlinker "${BUILD}/sim-entitlements.plist")
fi
"${TOOLCHAIN}/swiftc" \
  -sdk "${SDK}" \
  -target "${TARGET}" \
  -parse-as-library \
  -module-name Lmux \
  -O \
  -Xllvm -aarch64-use-tbi \
  -o "${APP}/Lmux" \
  ${EXTRA_LINK[@]+"${EXTRA_LINK[@]}"} \
  "${SRC}"/*.swift

echo "==> bundling"
cat > "${APP}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>Lmux</string>
  <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Lmux</string>
  <key>CFBundleDisplayName</key><string>lmux</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundleSupportedPlatforms</key><array><string>${PLATFORM}</string></array>
  <key>MinimumOSVersion</key><string>17.0</string>
  <key>UIDeviceFamily</key><array><integer>1</integer></array>
  <key>UIRequiredDeviceCapabilities</key><array><string>arm64</string></array>
  <key>UIApplicationSceneManifest</key>
  <dict><key>UIApplicationSupportsMultipleScenes</key><false/></dict>
  <key>UILaunchScreen</key><dict/>
  <key>UISupportedInterfaceOrientations</key>
  <array><string>UIInterfaceOrientationPortrait</string></array>
  <key>CFBundleIcons</key>
  <dict>
    <key>CFBundlePrimaryIcon</key>
    <dict>
      <key>CFBundleIconFiles</key>
      <array><string>AppIcon60x60</string></array>
    </dict>
  </dict>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>com.vas.lmux.share</string>
      <key>CFBundleURLSchemes</key><array><string>lmux</string></array>
    </dict>
  </array>
  <key>NSCameraUsageDescription</key>
  <string>Scan the QR code that lmux shows, or take a photo to send to your agent.</string>
  <key>NSPhotoLibraryUsageDescription</key>
  <string>Attach a photo to send to your agent.</string>
  <key>UIAppFonts</key>
  <array><string>JetBrainsMono.ttf</string></array>
</dict>
</plist>
PLIST

printf 'APPL????' > "${APP}/PkgInfo"

if [ -n "${SIM}" ]; then
  /usr/bin/codesign --force --sign - --timestamp=none "${APP}"
  echo
  echo "built ${APP} for the simulator (ad-hoc signed, not installed)."
  exit 0
fi

cat > "${BUILD}/entitlements.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>application-identifier</key><string>${TEAM_ID}.${BUNDLE_ID}</string>
  <key>com.apple.developer.team-identifier</key><string>${TEAM_ID}</string>
  <key>get-task-allow</key><true/>
  <key>keychain-access-groups</key><array><string>${TEAM_ID}.*</string></array>
</dict>
</plist>
PLIST

cp "${PROFILE}" "${APP}/embedded.mobileprovision"

echo "==> signing"
/usr/bin/codesign --force --sign "${IDENTITY}" \
  --entitlements "${BUILD}/entitlements.plist" \
  --timestamp=none \
  --generate-entitlement-der \
  "${APP}"

/usr/bin/codesign --verify --verbose=2 "${APP}" 2>&1 | tail -3

if [ -n "${NO_INSTALL}" ]; then
  echo
  echo "built and signed ${APP} (install skipped)."
  exit 0
fi
if [ ! -x "${PYMOBILE}" ]; then
  echo
  echo "built and signed ${APP}; install skipped: pymobiledevice3 not found at ${PYMOBILE}."
  exit 0
fi

echo "==> installing on ${DEV_ID}"
if "${PYMOBILE}" apps install --udid "${DEV_ID}" --developer "${APP}" 2>&1 | tail -20; then
  echo
  echo "installed. Launch it from the home screen (icon: lmux)."
elif echo "==> not on USB; trying over Wi-Fi with devicectl" &&
     "${DEVELOPER}/usr/bin/devicectl" device install app --device "${DEV_ID}" "${APP}" >/dev/null 2>&1; then
  echo "installed over Wi-Fi. Launch it from the home screen (icon: lmux)."
else
  echo
  echo "install failed (is the phone connected and unlocked?). The signed app is at ${APP}." >&2
fi
