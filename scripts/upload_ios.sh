#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ "${1:-}" != "--confirm" ]; then
    echo "Refusing to upload without --confirm: it archives, signs and uploads a TestFlight build, which cannot be undone." >&2
    exit 64
fi
if [ "${VORRA_RELEASES:-}" != enabled ]; then
    echo "Refusing to upload: VORRA_RELEASES is '${VORRA_RELEASES:-}', not enabled." >&2
    exit 1
fi
. scripts/asc_lib.sh
METADATA=apps/Apple/metadata/testflight
VERSION=${VERSION:-$(scripts/version.sh)}
BASE=$(($(scripts/build-number.sh "$VERSION" 1) - 1))
ARCHIVE="$PWD/build/ios/VorraIOS.xcarchive"

wait_listed() {
    url="$API/v1/builds?filter%5Bapp%5D=$APP_ID&filter%5Bversion%5D=$BUNDLE_VERSION&filter%5BpreReleaseVersion.version%5D=$VERSION&limit=1"
    tries=$((${LIST_WAIT:-3600} / 30))
    id=
    while [ "$tries" -gt 0 ]; do
        id=$(asc_get "$url" | jq -r '.data[0].id // empty') || id=
        [ -n "$id" ] && break
        tries=$((tries - 1))
        [ "$tries" -gt 0 ] && sleep 30
    done
    [ -n "$id" ] && printf '%s' "$id"
}

set_whats_new() {
    notes=$(mktemp -d)
    for dir in "$METADATA"/*/; do
        loc=$(basename "$dir")
        scripts/release_notes.sh --what-to-test "$loc" "$VERSION" "$BUILD" HEAD > "$notes/$loc" || { rm -rf "$notes"; return 1; }
        bytes=$(wc -c < "$notes/$loc")
        [ "$bytes" -le 4000 ] || { echo "What to Test for $loc is $bytes bytes, over 4,000" >&2; rm -rf "$notes"; return 1; }
        head -n 1 "$notes/$loc" >> "$notes/.intros"
    done
    current=$(asc_get "$API/v1/builds/$BUILD_ID/betaBuildLocalizations?fields%5BbetaBuildLocalizations%5D=locale,whatsNew&limit=50") \
        || { rm -rf "$notes"; return 1; }
    for file in "$notes"/*; do
        loc=$(basename "$file"); text=$(cat "$file")
        id=$(printf '%s' "$current" | jq -r --arg l "$loc" '.data[] | select(.attributes.locale == $l) | .id')
        was=$(printf '%s' "$current" | jq -r --arg l "$loc" '.data[] | select(.attributes.locale == $l) | .attributes.whatsNew // ""')
        if [ -z "$id" ]; then
            asc_send POST "$API/v1/betaBuildLocalizations" "$(jq -n --arg id "$BUILD_ID" --arg l "$loc" --arg t "$text" \
                '{data: {type: "betaBuildLocalizations", attributes: {locale: $l, whatsNew: $t},
                  relationships: {build: {data: {type: "builds", id: $id}}}}}')" >/dev/null || { rm -rf "$notes"; return 1; }
            echo "What to Test $loc: created."
        elif [ "$was" = "$text" ]; then
            echo "What to Test $loc: already set."
        elif [ -z "$was" ] || grep -qxF -- "$(printf '%s\n' "$was" | head -n 1)" "$notes/.intros"; then
            asc_send PATCH "$API/v1/betaBuildLocalizations/$id" "$(jq -n --arg id "$id" --arg t "$text" \
                '{data: {type: "betaBuildLocalizations", id: $id, attributes: {whatsNew: $t}}}')" >/dev/null \
                || { rm -rf "$notes"; return 1; }
            echo "What to Test $loc: set."
        else
            echo "What to Test $loc: kept, it holds text this script did not write."
        fi
    done
    printf 'What to Test on build %s (en-US):\n%s\n' "$BUNDLE_VERSION" "$(cat "$notes/en-US")"
    rm -rf "$notes"
}

if [ -z "${BUILD:-}" ]; then
    url="$API/v1/builds?filter%5Bapp%5D=$APP_ID&filter%5BpreReleaseVersion.platform%5D=IOS&filter%5BpreReleaseVersion.version%5D=$VERSION&fields%5Bbuilds%5D=version&limit=200"
    max=0
    while [ -n "$url" ]; do
        page=$(asc_get "$url")
        top=$(printf '%s' "$page" | jq -er --argjson base "$BASE" \
            '[.data[].attributes.version | tonumber | if . > $base then . - $base else . end] | max // 0')
        if [ "$top" -gt "$max" ]; then max=$top; fi
        url=$(printf '%s' "$page" | jq -r '.links.next // empty')
    done
    BUILD=$((max + 1))
fi
BUNDLE_VERSION=$(scripts/build-number.sh "$VERSION" "$BUILD")
echo "Uploading Vorra iOS $VERSION ($BUILD), CFBundleVersion $BUNDLE_VERSION"
scripts/external_testflight.sh 2>&1 || true

mkdir -p build/ios
rm -rf "$ARCHIVE" build/ios/export
xcodebuild archive -project apps/Apple/Vorra.xcodeproj -scheme VorraIOS -configuration Release \
    -destination 'generic/platform=iOS' -archivePath "$ARCHIVE" -derivedDataPath .build/xcode-ios \
    -allowProvisioningUpdates \
    -authenticationKeyPath "$KEY" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID" \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUNDLE_VERSION" VORRA_BUILD="$BUILD"
xcodebuild -exportArchive -archivePath "$ARCHIVE" \
    -exportOptionsPlist apps/Apple/iOS/ExportOptions.plist -exportPath "$PWD/build/ios/export" \
    -allowProvisioningUpdates \
    -authenticationKeyPath "$KEY" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID"
echo "Uploaded Vorra iOS $VERSION ($BUILD). Once App Store Connect has processed it, TestFlight gives it to the internal group."
BUILD_ID=$(wait_listed) || {
    echo "::error::Uploaded Vorra iOS $VERSION ($BUILD), but App Store Connect did not list build $BUNDLE_VERSION within ${LIST_WAIT:-3600} s, so the next upload could reuse build $BUILD. Wait until it is listed before rerunning or merging." >&2
    exit 1
}
set_whats_new || {
    echo "::error::Uploaded Vorra iOS $VERSION ($BUILD), but App Store Connect refused its What to Test. Set it in App Store Connect by hand." >&2
    exit 1
}
