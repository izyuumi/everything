#!/bin/sh
set -eu
IFS=. read -r major minor patch <<V
$1
V
build=$2
for n in "$major" "$minor" "$patch" "$build"; do
    case "$n" in ''|*[!0-9]*) echo "Expected a numeric major.minor.patch and build, got $1 $2" >&2; exit 64 ;; esac
done
if [ "$minor" -ge 100 ] || [ "$patch" -ge 100 ] || [ "$build" -lt 1 ] || [ "$build" -ge 1000 ]; then
    echo "Version $1 build $2 overflows CFBundleVersion: minor and patch below 100, build 1 to 999" >&2; exit 65
fi
echo $((major * 10000000 + minor * 100000 + patch * 1000 + build))
