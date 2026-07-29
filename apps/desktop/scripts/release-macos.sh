#!/usr/bin/env bash
# Build, Developer-ID sign, notarize, staple, and verify the public macOS DMG.
set -euo pipefail

cd "$(dirname "$0")/.."

: "${APPLE_SIGNING_IDENTITY:?set APPLE_SIGNING_IDENTITY to a Developer ID Application identity}"
: "${ENGRAM_DESKTOP_NOTARY_PROFILE:?set ENGRAM_DESKTOP_NOTARY_PROFILE to a notarytool keychain profile}"

if ! security find-identity -v -p codesigning |
  grep -F "\"${APPLE_SIGNING_IDENTITY}\"" >/dev/null; then
  echo "error: signing identity is unavailable: ${APPLE_SIGNING_IDENTITY}" >&2
  exit 1
fi
if ! xcrun notarytool history \
  --keychain-profile "$ENGRAM_DESKTOP_NOTARY_PROFILE" \
  --output-format json >/dev/null 2>&1; then
  echo "error: notarytool profile is unavailable: ${ENGRAM_DESKTOP_NOTARY_PROFILE}" >&2
  exit 1
fi

./scripts/build-release.sh

version="$(node -p 'require("./package.json").version')"
app="src-tauri/target/release/bundle/macos/Engram.app"
dmg="src-tauri/target/release/bundle/dmg/Engram_${version}_aarch64.dmg"
checksum="${dmg}.sha256"

codesign --verify --deep --strict --verbose=2 "$app"
codesign --verify --verbose=2 "$dmg"
xcrun notarytool submit "$dmg" \
  --keychain-profile "$ENGRAM_DESKTOP_NOTARY_PROFILE" \
  --wait
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
spctl -a -vv --type open --context context:primary-signature "$dmg"
hdiutil verify "$dmg"

mount_dir="$(mktemp -d /tmp/engram-desktop-release.XXXXXX)"
cleanup() {
  if [[ -n "${mount_dir:-}" && -d "$mount_dir" ]]; then
    hdiutil detach "$mount_dir" >/dev/null 2>&1 || true
    rmdir "$mount_dir" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

hdiutil attach -nobrowse -readonly -mountpoint "$mount_dir" "$dmg" >/dev/null
codesign --verify --deep --strict --verbose=2 "$mount_dir/Engram.app"
spctl -a -vv --type execute "$mount_dir/Engram.app"
hdiutil detach "$mount_dir" >/dev/null
rmdir "$mount_dir"
mount_dir=""

(
  cd "$(dirname "$dmg")"
  shasum -a 256 "$(basename "$dmg")"
) >"$checksum"
echo "release: ${dmg}"
echo "checksum: ${checksum}"
