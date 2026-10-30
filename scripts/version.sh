#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
TAG=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*' HEAD 2>/dev/null || true)
IFS=. read -r major minor patch <<V
${TAG#v}
V
RANGE=${TAG:+$TAG..}HEAD
subjects=$(git log --no-merges --format=%s "$RANGE")
if git log --no-merges --format=%B "$RANGE" | grep -qE '^BREAKING[ -]CHANGE: ' \
    || printf '%s\n' "$subjects" | grep -qE '^[a-z]+(\([^)]*\))?!: '; then
    if [ "${major:-0}" -gt 0 ]; then echo "$((major + 1)).0.0"; else echo "0.$((${minor:-0} + 1)).0"; fi
elif printf '%s\n' "$subjects" | grep -qE '^feat(\([^)]*\))?: '; then
    echo "${major:-0}.$((${minor:-0} + 1)).0"
else
    echo "${major:-0}.${minor:-0}.$((${patch:-0} + 1))"
fi
