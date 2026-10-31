#!/bin/sh
cd "$(dirname "$0")/.."
usage() { echo "usage: $0 <version> <build> [--release-type MANUAL|AFTER_APPROVAL] [--create] [--attach] [--submit]" >&2; exit 64; }

VERSION="" BUILD="" RELEASE_TYPE="" CREATE="" ATTACH="" SUBMIT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --create) CREATE=1 ;;
        --attach) ATTACH=1 ;;
        --submit) SUBMIT=1 ;;
        --release-type) [ $# -ge 2 ] || usage; RELEASE_TYPE=$2; shift ;;
        --release-type=*) RELEASE_TYPE=${1#*=} ;;
        -*) usage ;;
        *) if [ -z "$VERSION" ]; then VERSION=$1; elif [ -z "$BUILD" ]; then BUILD=$1; else usage; fi ;;
    esac
    shift
done
[ -n "$VERSION" ] && [ -n "$BUILD" ] || usage
printf '%s' "$VERSION" | grep -qE '^[0-9]+(\.[0-9]+){0,2}$' || { echo "The version must look like 1.2.3, not '$VERSION'." >&2; exit 64; }
printf '%s' "$BUILD" | grep -qE '^[0-9]+$' || { echo "The build number must be digits, not '$BUILD'." >&2; exit 64; }
case "$RELEASE_TYPE" in
    '' | MANUAL | AFTER_APPROVAL) ;;
    SCHEDULED) echo "SCHEDULED needs a release date, which this script does not set; use MANUAL or AFTER_APPROVAL." >&2; exit 64 ;;
    *) echo "The release type must be MANUAL or AFTER_APPROVAL, not '$RELEASE_TYPE'." >&2; exit 64 ;;
esac
grep -q '<key>ITSAppUsesNonExemptEncryption</key><false/>' apps/Apple/iOS/Info.plist && PLIST_ENCRYPTION="false" || PLIST_ENCRYPTION="unset"

. scripts/asc_lib.sh
exec 3>&1

BLOCKED=0
block() { echo "Blocked: $*"; BLOCKED=1; }
q() { printf '%s' "$1" | jq -r "$2"; }

write() {
    if [ -n "$1" ]; then
        out=$(asc_send "$2" "$3" "$4") || { echo "App Store Connect refused: $5" >&2; exit 70; }
        echo "Done: $5" >&3
        printf '%s' "$out"
    else
        echo "Would: $5" >&3
    fi
}

EDITABLE='["PREPARE_FOR_SUBMISSION", "DEVELOPER_REJECTED", "REJECTED", "METADATA_REJECTED", "INVALID_BINARY"]'
state() { q "$1" '.attributes.appStoreState // .attributes.appVersionState // ""'; }
versions() { asc_get "$API/v1/apps/$APP_ID/appStoreVersions?filter%5Bplatform%5D=IOS&limit=200"; }

editable() { printf '%s' "$EDITABLE" | jq -e --arg s "$1" 'index($s)' >/dev/null; }
pick() { printf '%s' "$1" | jq -c --argjson s "$2" '[.data[] | select(.attributes.state as $x | $s | index($x))][0] // empty'; }

echo "App $APP_ID, iOS $VERSION build $BUILD"

all=$(versions)
live=$(q "$all" '[.data[] | select(.attributes.appStoreState == "READY_FOR_SALE"
    or .attributes.appVersionState == "READY_FOR_DISTRIBUTION")]
    | sort_by(.attributes.versionString | split(".") | map(tonumber? // 0)) | last // empty')
if [ -n "$live" ]; then
    echo "On sale: $(q "$live" '.attributes | "\(.versionString), release type \(.releaseType)"')"
else
    echo "On sale: nothing"
fi
RT_GIVEN=$RELEASE_TYPE
if [ -z "$RELEASE_TYPE" ]; then
    RELEASE_TYPE=$(q "${live:-null}" '.attributes.releaseType // ""')
    case "$RELEASE_TYPE" in
        MANUAL | AFTER_APPROVAL) echo "Release type: $RELEASE_TYPE, from the version on sale" ;;
        '') echo "No version is on sale to copy a release type from; pass --release-type." >&2; exit 65 ;;
        *) echo "The version on sale is $RELEASE_TYPE, which this script does not set; pass --release-type." >&2; exit 65 ;;
    esac
else
    echo "Release type: $RELEASE_TYPE, from --release-type"
fi

version=$(printf '%s' "$all" | jq -c --arg v "$VERSION" '[.data[] | select(.attributes.versionString == $v)][0] // empty')
VERSION_ID="" VERSION_STATE=PREPARE_FOR_SUBMISSION attached=""
if [ -n "$version" ]; then
    VERSION_ID=$(q "$version" .id)
    VERSION_STATE=$(state "$version")
    echo "Version: $VERSION exists ($VERSION_STATE, release type $(q "$version" .attributes.releaseType)); it is reused."
    attached=$(asc_get "$API/v1/appStoreVersions/$VERSION_ID/build" | jq -r '.data.id // ""')
    if [ "$(q "$version" .attributes.releaseType)" != "$RELEASE_TYPE" ] && ! editable "$VERSION_STATE"; then
        if [ -n "$RT_GIVEN" ]; then
            block "$VERSION is $VERSION_STATE, so its release type cannot change to $RELEASE_TYPE."
        else
            RELEASE_TYPE=$(q "$version" .attributes.releaseType)
            echo "Release type: $VERSION is $VERSION_STATE, so it keeps $RELEASE_TYPE."
        fi
    fi
else
    other=$(printf '%s' "$all" | jq -c --argjson e "$EDITABLE" \
        '[.data[] | select((.attributes.appStoreState // "") as $s | $e | index($s))][0] // empty')
    if [ -n "$other" ]; then
        block "iOS $(q "$other" '.attributes | "\(.versionString) is \(.appStoreState)"'); App Store Connect allows one version in preparation, so $VERSION cannot be created. Rename or remove that one in App Store Connect."
    else
        echo "Version: $VERSION does not exist yet."
    fi
fi

builds=$(asc_get "$API/v1/builds?filter%5Bapp%5D=$APP_ID&filter%5Bversion%5D=$BUILD&filter%5BpreReleaseVersion.platform%5D=IOS&include=preReleaseVersion&limit=10")
build=$(q "$builds" '.data[0] // empty')
BUILD_ID="" ENCRYPTION=""
if [ -z "$build" ]; then
    block "App Store Connect has no iOS build $BUILD."
else
    BUILD_ID=$(q "$build" .id)
    pre=$(printf '%s' "$builds" | jq -r --arg id "$(q "$build" '.relationships.preReleaseVersion.data.id // ""')" \
        '[.included[]? | select(.type == "preReleaseVersions" and .id == $id) | .attributes.version][0] // ""')
    processing=$(q "$build" '.attributes.processingState // ""')
    expired=$(q "$build" '.attributes.expired')
    ENCRYPTION=$(q "$build" '.attributes.usesNonExemptEncryption | if . == null then "unanswered" else tostring end')
    echo "Build: $BUILD for version ${pre:-unknown}, $processing, expired $expired, uploaded $(q "$build" '.attributes.uploadedDate // "unknown"')"
    [ "$pre" = "$VERSION" ] || block "build $BUILD was uploaded for version ${pre:-unknown}, not $VERSION."
    [ "$processing" = VALID ] || block "build $BUILD is $processing, not VALID."
    [ "$expired" = false ] || block "build $BUILD has expired."
fi

rights=$(asc_get "$API/v1/apps/$APP_ID?fields%5Bapps%5D=contentRightsDeclaration" | jq -r '.data.attributes.contentRightsDeclaration // ""')
echo "Content rights: ${rights:-not declared}"
[ -n "$rights" ] || block "the content rights declaration is not set; set it in App Store Connect under App Information."
echo "Export compliance: build usesNonExemptEncryption ${ENCRYPTION:-unknown}; ITSAppUsesNonExemptEncryption in apps/Apple/iOS/Info.plist: $PLIST_ENCRYPTION"
[ "$PLIST_ENCRYPTION" = false ] || block "apps/Apple/iOS/Info.plist does not set ITSAppUsesNonExemptEncryption to false."
[ "$ENCRYPTION" != unanswered ] || block "build $BUILD has no export compliance answer; answer it in App Store Connect."
[ "$ENCRYPTION" != true ] || block "build $BUILD declares non-exempt encryption, which needs export compliance documents first."

subs=$(asc_get "$API/v1/reviewSubmissions?filter%5Bapp%5D=$APP_ID&filter%5Bplatform%5D=IOS&filter%5Bstate%5D=READY_FOR_REVIEW,WAITING_FOR_REVIEW,IN_REVIEW,UNRESOLVED_ISSUES,CANCELING,COMPLETING&limit=20")
items() { asc_get "$API/v1/reviewSubmissions/$1/items?include=appStoreVersion&limit=50"; }
has_version() { [ -n "$VERSION_ID" ] && items "$1" | jq -e --arg v "$VERSION_ID" \
    'any(.data[]; .relationships.appStoreVersion.data.id == $v and (.attributes.state // "") != "REMOVED")' >/dev/null; }
sent=$(pick "$subs" '["WAITING_FOR_REVIEW", "IN_REVIEW"]')
busy=$(pick "$subs" '["CANCELING", "COMPLETING"]')
open=$(pick "$subs" '["READY_FOR_REVIEW", "UNRESOLVED_ISSUES"]')
SUBMITTED=""
if [ -n "$sent" ] && has_version "$(q "$sent" .id)"; then
    SUBMITTED=1
    echo "Review: $VERSION is already submitted ($(q "$sent" .attributes.state)); nothing to submit."
elif [ -n "$sent" ]; then
    block "another iOS review submission is $(q "$sent" .attributes.state); wait for it or cancel it in App Store Connect."
elif [ -n "$busy" ]; then
    block "an iOS review submission is $(q "$busy" .attributes.state); try again once it finishes."
fi
if [ -z "$SUBMITTED" ] && [ -n "$SUBMIT" ] && [ -z "$ATTACH" ] && { [ -z "$BUILD_ID" ] || [ "$attached" != "$BUILD_ID" ]; }; then
    block "build $BUILD is not attached to $VERSION; run with --attach first."
fi
if [ -z "$VERSION_ID" ] && [ -z "$CREATE" ] && [ -n "$ATTACH$SUBMIT" ]; then
    block "iOS version $VERSION does not exist; run with --create first."
fi

if [ "$BLOCKED" = 1 ]; then
    if [ -n "$CREATE$ATTACH$SUBMIT" ]; then echo "Stopped; nothing was written." >&2; else echo "Plan only; nothing was written, and the blocks above stop it." >&2; fi
    exit 65
fi

if [ -z "$VERSION_ID" ]; then
    created=$(write "$CREATE" POST "$API/v1/appStoreVersions" "$(jq -nc --arg v "$VERSION" --arg r "$RELEASE_TYPE" --arg app "$APP_ID" \
        '{data: {type: "appStoreVersions", attributes: {platform: "IOS", versionString: $v, releaseType: $r},
          relationships: {app: {data: {type: "apps", id: $app}}}}}')" "create iOS version $VERSION, release type $RELEASE_TYPE.")
    VERSION_ID=$(q "${created:-null}" '.data.id // ""')
    [ -z "$CREATE" ] || [ -n "$VERSION_ID" ] || { echo "App Store Connect returned no id for the new version." >&2; exit 70; }
elif [ "$(q "$version" .attributes.releaseType)" = "$RELEASE_TYPE" ]; then
    echo "Release type: $VERSION is already $RELEASE_TYPE."
else
    write "$CREATE" PATCH "$API/v1/appStoreVersions/$VERSION_ID" "$(jq -nc --arg id "$VERSION_ID" --arg r "$RELEASE_TYPE" \
        '{data: {type: "appStoreVersions", id: $id, attributes: {releaseType: $r}}}')" \
        "set the release type of $VERSION to $RELEASE_TYPE." >/dev/null
fi

if [ "$attached" = "$BUILD_ID" ]; then
    echo "Build: $BUILD is already attached to $VERSION."
elif ! editable "$VERSION_STATE"; then
    echo "$VERSION is $VERSION_STATE, so build $BUILD cannot be attached." >&2; exit 65
else
    [ -z "$attached" ] || echo "Build: $VERSION has another build attached, which build $BUILD replaces."
    write "$ATTACH" PATCH "$API/v1/appStoreVersions/${VERSION_ID:-new}/relationships/build" \
        "$(jq -nc --arg id "$BUILD_ID" '{data: {type: "builds", id: $id}}')" "attach build $BUILD to $VERSION." >/dev/null
    if [ -n "$ATTACH" ]; then
        [ "$(asc_get "$API/v1/appStoreVersions/$VERSION_ID/build" | jq -r '.data.id // ""')" = "$BUILD_ID" ] \
            || { echo "App Store Connect does not show build $BUILD on $VERSION after attaching it." >&2; exit 70; }
    fi
fi

if [ -z "$SUBMITTED" ]; then
    SUB_ID=""
    if [ -n "$open" ]; then
        SUB_ID=$(q "$open" .id)
        echo "Review: the open submission $SUB_ID ($(q "$open" .attributes.state)) is reused."
    else
        created=$(write "$SUBMIT" POST "$API/v1/reviewSubmissions" "$(jq -nc --arg app "$APP_ID" \
            '{data: {type: "reviewSubmissions", attributes: {platform: "IOS"}, relationships: {app: {data: {type: "apps", id: $app}}}}}')" \
            "create an iOS review submission.")
        SUB_ID=$(q "${created:-null}" '.data.id // ""')
        [ -z "$SUBMIT" ] || [ -n "$SUB_ID" ] || { echo "App Store Connect returned no id for the new submission." >&2; exit 70; }
    fi
    if [ -n "$SUB_ID" ] && has_version "$SUB_ID"; then
        echo "Review: $VERSION is already in submission $SUB_ID."
    else
        write "$SUBMIT" POST "$API/v1/reviewSubmissionItems" "$(jq -nc --arg s "${SUB_ID:-new}" --arg v "${VERSION_ID:-new}" \
            '{data: {type: "reviewSubmissionItems", relationships: {reviewSubmission: {data: {type: "reviewSubmissions", id: $s}},
              appStoreVersion: {data: {type: "appStoreVersions", id: $v}}}}}')" "add $VERSION to the review submission." >/dev/null
    fi
    write "$SUBMIT" PATCH "$API/v1/reviewSubmissions/${SUB_ID:-new}" "$(jq -nc --arg id "${SUB_ID:-new}" \
        '{data: {type: "reviewSubmissions", id: $id, attributes: {submitted: true}}}')" \
        "submit $VERSION build $BUILD for App Store review." >/dev/null
fi

if [ -z "$CREATE$ATTACH$SUBMIT" ]; then
    echo "Plan only; nothing was written. Nothing blocks it: --create, --attach and --submit make these changes."
fi
