#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
PROJECT=apps/Apple/Vorra.xcodeproj/project.pbxproj
bad=
fail() { bad=1; echo "::error::Version drift: $1" >&2; }
V=$(awk -F'"' '/^\[/ { p = ($0 == "[package]") } p && /^version = / { print $2; exit }' Cargo.toml)
[ "$V" = 0.1.0 ] || fail "Cargo.toml's package version is '$V'; it is the fixed placeholder 0.1.0, and releases take their version from scripts/version.sh"
lock=$(awk -F'"' 'prev && /^version = / { print $2; exit } { prev = ($0 == "name = \"vorra\"") }' Cargo.lock)
[ "$lock" = "$V" ] || fail "Cargo.lock's vorra package is '${lock:-missing}', Cargo.toml says '$V'"
B=$(scripts/build-number.sh 0.1.0 1)
for key in MARKETING_VERSION:0.1.0 CURRENT_PROJECT_VERSION:$B VORRA_BUILD:1; do
    name=${key%%:*}; want=${key#*:}
    seen=$(sed -n "s/.*\"$name\" = \"\\([^\"]*\\)\";.*/\\1/p" "$PROJECT" | sort -u | tr '\n' ' ')
    [ "$seen" = "$want " ] || fail "$PROJECT sets $name to '${seen% }', expected only the placeholder $want; release builds pass the real value to xcodebuild"
done
for plist in apps/Apple/iOS/Info.plist apps/Apple/macOS/Info.plist; do
    for key in CFBundleShortVersionString:MARKETING_VERSION CFBundleVersion:CURRENT_PROJECT_VERSION VorraBuild:VORRA_BUILD; do
        have=$(tr -d '\n' < "$plist" | sed -n "s|.*<key>${key%%:*}</key>[[:space:]]*<string>\\([^<]*\\)</string>.*|\\1|p")
        [ "$have" = "\$(${key#*:})" ] || fail "$plist sets ${key%%:*} to '$have'; it must be \$(${key#*:})"
    done
done
for use in $(git grep -lE 'CARGO_PKG_VERSION|option_env!\("VORRA_VERSION"\)' -- 'src/*.rs' 'src/**/*.rs' 'crates/*.rs' | grep -vx src/lib.rs); do
    fail "$use reads CARGO_PKG_VERSION or VORRA_VERSION directly; report vorra::VERSION so every binary shows the one stamped version"
done
if [ -n "${1:-}" ]; then
    want=$(scripts/version.sh)
    have=${1#v}; have=${have%-beta}
    [ "$have" = "$want" ] || fail "release tag $1 names $have, but scripts/version.sh computes $want at $(git rev-parse --short HEAD)"
fi
[ -z "$bad" ] || exit 1
echo "Version sources agree: Cargo.toml and Cargo.lock $V, Apple placeholders 0.1.0 ($B, build 1) with Info.plists reading the build settings${1:+, release tag $1 matches scripts/version.sh}."
