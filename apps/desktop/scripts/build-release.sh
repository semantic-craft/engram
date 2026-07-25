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

echo "ok: no ${HOME} paths in the bundle"
echo "dmg: $(ls src-tauri/target/release/bundle/dmg/Engram_*.dmg)"
