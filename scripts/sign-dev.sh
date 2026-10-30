#!/bin/sh
set -eu

identifier="ai.everything.cli"
identity="${EVERYTHING_CODESIGN_IDENTITY:-}"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "sign-dev.sh: macOS only" >&2
  exit 1
fi

if [ -z "$identity" ]; then
  identities="$(security find-identity -v -p codesigning)"
  identity="$(printf '%s\n' "$identities" | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*"$/\1/p' | head -n 1)"
  if [ -z "$identity" ]; then
    identity="$(printf '%s\n' "$identities" | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Apple Development: .*"$/\1/p' | head -n 1)"
  fi
fi

if [ -z "$identity" ]; then
  echo "sign-dev.sh: no Developer ID Application or Apple Development identity found; set EVERYTHING_CODESIGN_IDENTITY" >&2
  exit 1
fi

if [ "$#" -eq 0 ]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
  for candidate in "$root/target/debug/everything" "$root/target/release/everything"; do
    if [ -f "$candidate" ]; then
      set -- "$@" "$candidate"
    fi
  done
fi

if [ "$#" -eq 0 ]; then
  echo "usage: scripts/sign-dev.sh [path-to-everything ...]" >&2
  exit 1
fi

if [ "${EVERYTHING_CODESIGN_RELEASE:-0}" = "1" ]; then
  options="--options runtime --timestamp"
else
  options="--timestamp=none"
fi

for binary in "$@"; do
  codesign --force --sign "$identity" --identifier "$identifier" $options "$binary"
  codesign --verify --strict "$binary"
  codesign --display --requirements - "$binary" 2>&1 | sed -n 's/^designated => /designated requirement: /p'
done
