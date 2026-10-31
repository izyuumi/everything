#!/bin/sh
set -eu
IDENTIFIER=to.yumi.everything.cli
if [ "$(uname -s)" != "Darwin" ]; then
    echo "sign_macos.sh: macOS only" >&2
    exit 1
fi
if [ "$#" -eq 0 ]; then
    root=$(cd "$(dirname "$0")/.." && pwd)
    for candidate in "$root/target/debug/everything" "$root/target/release/everything"; do
        [ -f "$candidate" ] && set -- "$@" "$candidate"
    done
    [ "$#" -gt 0 ] || { echo "usage: scripts/sign_macos.sh [path-to-everything ...]" >&2; exit 64; }
fi
for binary in "$@"; do
    [ -f "$binary" ] || { echo "sign_macos.sh: no binary at $binary" >&2; exit 64; }
done
identity=${SIGN_IDENTITY:-${EVERYTHING_CODESIGN_IDENTITY:-}}
if [ -z "$identity" ]; then
    identities=$(security find-identity -v -p codesigning)
    identity=$(printf '%s\n' "$identities" | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*"$/\1/p' | head -n 1)
    [ -n "$identity" ] || identity=$(printf '%s\n' "$identities" | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Apple Development: .*"$/\1/p' | head -n 1)
fi
if [ -z "$identity" ]; then
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
        echo "::warning::$* not signed or notarized: no signing identity."
        exit 0
    fi
    echo "sign_macos.sh: no Developer ID Application or Apple Development identity found; set EVERYTHING_CODESIGN_IDENTITY" >&2
    exit 1
fi
if [ -n "${NOTARY_PROFILE:-}" ] || [ "${EVERYTHING_CODESIGN_RELEASE:-0}" = 1 ] || [ "${GITHUB_ACTIONS:-}" = true ]; then
    options="--options runtime --timestamp"
else
    options="--timestamp=none"
fi
WORK=
trap 'if [ -n "$WORK" ]; then rm -rf "$WORK"; fi' EXIT
for binary in "$@"; do
    codesign --force --sign "$identity" --identifier "$IDENTIFIER" $options "$binary"
    codesign --verify --strict "$binary"
    [ "$(codesign -d --verbose=2 "$binary" 2>&1 | sed -n 's/^Identifier=//p')" = "$IDENTIFIER" ] \
        || { echo "sign_macos.sh: $binary is not signed as $IDENTIFIER" >&2; exit 1; }
    codesign --display --requirements - "$binary" 2>&1 | sed -n 's/^designated => /designated requirement: /p'
    if [ -z "${NOTARY_PROFILE:-}" ]; then
        continue
    fi
    WORK=$(mktemp -d)
    ditto -c -k --keepParent "$binary" "$WORK/everything.zip"
    for try in 1 2 3; do
        xcrun notarytool submit "$WORK/everything.zip" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json \
            | jq -e '.status == "Accepted"' >/dev/null && break
        [ "$try" = 3 ] && exit 1
        sleep 60
    done
    rm -rf "$WORK"
    WORK=
    echo "Notarized $binary."
done
