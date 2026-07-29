#!/usr/bin/env bash
# Release build for Engram Desktop with path sanitization.
#
# Rust release binaries embed absolute source paths (panic locations,
# tracing metadata) for every compiled crate, including everything under
# ~/.cargo/registry — shipping one leaks the builder's home directory
# and username. --remap-path-prefix rewrites those strings at compile
# time, and the gate below fails the build if any slip through.
set -euo pipefail

cd "$(dirname "$0")/.."

export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }--remap-path-prefix=${HOME}=~"
npm run tauri build

app="src-tauri/target/release/bundle/macos/Engram.app"
bin="${app}/Contents/MacOS/engram-desktop"
resources="${app}/Contents/Resources"
version="$(node -p 'require("./package.json").version')"
dmg="src-tauri/target/release/bundle/dmg/Engram_${version}_aarch64.dmg"
if [[ ! -f "$bin" ]]; then
  echo "error: expected release binary is missing: ${bin}" >&2
  exit 1
fi
if [[ ! -d "$resources" ]]; then
  echo "error: expected bundle resources are missing: ${resources}" >&2
  exit 1
fi
if strings "$bin" | grep -F "$HOME" >/dev/null; then
  echo "error: ${bin} still contains ${HOME} — sanitization failed, do not ship" >&2
  exit 1
fi
if grep -rqF "$HOME" "$resources" 2>/dev/null; then
  echo "error: bundle resources contain ${HOME} — do not ship" >&2
  exit 1
fi
if [[ ! -f "$dmg" ]]; then
  echo "error: expected release DMG is missing: ${dmg}" >&2
  exit 1
fi

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --verify --deep --strict --verbose=2 "$app"
  if ! codesign -dv --verbose=4 "$app" 2>&1 |
    grep -F "Authority=${APPLE_SIGNING_IDENTITY}" >/dev/null; then
    echo "error: app is not signed by ${APPLE_SIGNING_IDENTITY}" >&2
    exit 1
  fi
else
  echo "warning: APPLE_SIGNING_IDENTITY is unset; this bundle is for local testing only" >&2
fi

echo "ok: no ${HOME} paths in the bundle"
echo "dmg: ${dmg}"
