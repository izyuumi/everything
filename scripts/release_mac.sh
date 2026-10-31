#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
REPO=${REPO:-izyuumi/everything}
DIST=${1:?usage: scripts/release_mac.sh <output dir>}
[ "${VORRA_RELEASES:-}" = enabled ] || { echo "Refusing to notarize: VORRA_RELEASES is '${VORRA_RELEASES:-}', not enabled." >&2; exit 1; }
VERSION=${VERSION:?VERSION is required}
BUILD=${BUILD:?BUILD is required}
IDENTITY=${SIGN_IDENTITY:?SIGN_IDENTITY must name the Developer ID Application identity}
NOTARY_PROFILE=${NOTARY_PROFILE:?NOTARY_PROFILE must name a notarytool keychain profile}
SPARKLE_ED_KEY_FILE=${SPARKLE_ED_KEY_FILE:?SPARKLE_ED_KEY_FILE must hold the Sparkle EdDSA private key}
PUBLIC_KEY=${VORRA_SPARKLE_PUBLIC_KEY:?VORRA_SPARKLE_PUBLIC_KEY must hold the Sparkle EdDSA public key}
[ -s "$SPARKLE_ED_KEY_FILE" ] || { echo "$SPARKLE_ED_KEY_FILE is empty" >&2; exit 1; }
TAG="v$VERSION-beta"
BUNDLE_VERSION=$(scripts/build-number.sh "$VERSION" "$BUILD")
WORK=.build/mac-release
APP="$WORK/Vorra.app"

rm -rf "$WORK" && mkdir -p "$WORK" "$DIST"
env -u SDKROOT xcodebuild build -project apps/Apple/Vorra.xcodeproj -scheme VorraMac -configuration Release \
    -destination 'platform=macOS' -derivedDataPath "$WORK/derived" -clonedSourcePackagesDirPath .build/xcode-packages \
    -quiet MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUNDLE_VERSION" VORRA_BUILD="$BUILD" \
    VORRA_SPARKLE_PUBLIC_KEY="$PUBLIC_KEY" CODE_SIGNING_ALLOWED=YES CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$IDENTITY" \
    DEVELOPMENT_TEAM=AN5KM8QGEF OTHER_CODE_SIGN_FLAGS=--timestamp CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO ENABLE_HARDENED_RUNTIME=YES
ditto "$WORK/derived/Build/Products/Release/Vorra.app" "$APP"
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework"
for item in "$SPARKLE/Versions/B/XPCServices/Installer.xpc" "$SPARKLE/Versions/B/XPCServices/Downloader.xpc" \
    "$SPARKLE/Versions/B/Autoupdate" "$SPARKLE/Versions/B/Updater.app" "$SPARKLE" "$APP"; do
    codesign --force --timestamp --options runtime --preserve-metadata=entitlements --sign "$IDENTITY" "$item"
done
codesign --verify --deep --strict "$APP"
plist() { /usr/libexec/PlistBuddy -c "Print $1" "$APP/Contents/Info.plist"; }
[ "$(plist CFBundleIdentifier)" = to.yumi.vorra.mac ] || { echo "The Mac app must be to.yumi.vorra.mac, the id Sparkle updates" >&2; exit 1; }
[ "$(plist CFBundleVersion)" = "$BUNDLE_VERSION" ] || { echo "The Mac app's CFBundleVersion is not $BUNDLE_VERSION" >&2; exit 1; }
[ "$(plist SUPublicEDKey)" = "$PUBLIC_KEY" ] || { echo "The Mac app does not carry the Sparkle public key" >&2; exit 1; }
[ "$(plist SUFeedURL)" = "https://github.com/$REPO/releases/latest/download/appcast.xml" ] \
    || { echo "The Mac app's SUFeedURL is not the stable appcast of $REPO" >&2; exit 1; }

STAGE="$WORK/stage"
mkdir -p "$STAGE" "$WORK/feed"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
DMG="$WORK/feed/Vorra.dmg"
hdiutil create -volname Vorra -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
codesign --force --timestamp --sign "$IDENTITY" "$DMG"
for try in 1 2 3; do
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait && break
    [ "$try" = 3 ] && exit 1
    sleep 60
done
xcrun stapler staple "$DMG"
spctl --assess --type open --context context:primary-signature -v "$DMG"

GENERATE=$(find .build/xcode-packages/artifacts -name generate_appcast -type f -perm -u+x | head -1)
[ -n "$GENERATE" ] || { echo "Sparkle's generate_appcast is missing from .build/xcode-packages" >&2; exit 1; }
"$GENERATE" --ed-key-file "$SPARKLE_ED_KEY_FILE" --download-url-prefix "https://github.com/$REPO/releases/download/$TAG/" "$WORK/feed"
grep -q "<sparkle:version>$BUNDLE_VERSION</sparkle:version>" "$WORK/feed/appcast.xml" \
    || { echo "appcast.xml does not carry build $BUNDLE_VERSION" >&2; exit 1; }
grep -q "url=\"https://github.com/$REPO/releases/download/$TAG/Vorra.dmg\"" "$WORK/feed/appcast.xml" \
    || { echo "appcast.xml does not point at $TAG/Vorra.dmg" >&2; exit 1; }
cp "$DMG" "$WORK/feed/appcast.xml" "$DIST/"
echo "Built Vorra $VERSION ($BUILD), CFBundleVersion $BUNDLE_VERSION, in $DIST/"
