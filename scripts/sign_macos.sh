#!/bin/sh
set -eu
BINARY=${1:?usage: scripts/sign_macos.sh <binary>}
IDENTIFIER=${IDENTIFIER:-ai.everything.cli}
[ -f "$BINARY" ] || { echo "No binary at $BINARY" >&2; exit 64; }
if [ -z "${SIGN_IDENTITY:-}" ]; then
    echo "::warning::$BINARY is not signed or notarized: SIGN_IDENTITY is unset."
    exit 0
fi
codesign --force --timestamp --options runtime --identifier "$IDENTIFIER" --sign "$SIGN_IDENTITY" "$BINARY"
codesign --verify --strict --verbose=2 "$BINARY"
[ "$(codesign -d --verbose=2 "$BINARY" 2>&1 | sed -n 's/^Identifier=//p')" = "$IDENTIFIER" ] \
    || { echo "$BINARY is not signed as $IDENTIFIER" >&2; exit 1; }
if [ -z "${NOTARY_PROFILE:-}" ]; then
    echo "::warning::$BINARY is signed but not notarized: NOTARY_PROFILE is unset."
    exit 0
fi
ZIP=$(mktemp -d)/everything.zip
ditto -c -k --keepParent "$BINARY" "$ZIP"
for try in 1 2 3; do
    xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json \
        | jq -e '.status == "Accepted"' >/dev/null && break
    [ "$try" = 3 ] && exit 1
    sleep 60
done
rm -f "$ZIP"
echo "Signed and notarized $BINARY as $IDENTIFIER."
