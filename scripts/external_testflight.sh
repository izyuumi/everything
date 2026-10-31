#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ "${1:-}" = --audit ]; then
    markers='betaAppReviewSubmissions|betaGroups|distribute_external|distribute-external|submit_beta_review|publicLinkEnabled'
    bad=0
    for file in $(git ls-files .github/workflows scripts); do
        [ "$file" = scripts/external_testflight.sh ] && continue
        [ -f "$file" ] || continue
        grep -qE "$markers" "$file" || continue
        grep -q 'external_testflight\.sh' "$file" && continue
        grep -nE "$markers" "$file" | sed "s|^|::error file=$file::An external TestFlight path without scripts/external_testflight.sh: $file:|"
        bad=1
    done
    [ "$bad" = 0 ] && echo "No external TestFlight or Beta App Review path runs without scripts/external_testflight.sh."
    exit "$bad"
fi
if [ "${VORRA_EXTERNAL_TESTFLIGHT:-}" = true ]; then
    echo "VORRA_EXTERNAL_TESTFLIGHT is true: external TestFlight groups and Beta App Review are allowed."
    exit 0
fi
echo "VORRA_EXTERNAL_TESTFLIGHT is '${VORRA_EXTERNAL_TESTFLIGHT:-}', not true: internal group only; no external group, no Beta App Review." >&2
exit 1
