#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
TARGET=${1:?usage: scripts/release_build.sh <target triple> [<out dir>]}
DIST=${2:-dist}
VERSION=${VERSION:-$(scripts/version.sh)}
BUILD=${BUILD:-1}
case "$BUILD" in ''|*[!0-9]*) echo "BUILD must be a positive integer, got '$BUILD'" >&2; exit 64 ;; esac
VORRA_VERSION="$VERSION+$BUILD" cargo build --release --locked --package vorra --bin vorra --target "$TARGET"
BINARY=target/$TARGET/release/vorra
case "$TARGET" in *-apple-darwin) scripts/sign_macos.sh "$BINARY" ;; esac
mkdir -p "$DIST"
STAGE=$(mktemp -d)
cp "$BINARY" "$STAGE/vorra"
COPYFILE_DISABLE=1 tar -C "$STAGE" -czf "$DIST/vorra-$TARGET.tar.gz" vorra
rm -rf "$STAGE"
echo "Built vorra $VERSION+$BUILD for $TARGET in $DIST/vorra-$TARGET.tar.gz"
