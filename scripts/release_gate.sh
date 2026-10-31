#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
markers='gh release (create|delete|edit|upload)|git tag|git push|gh pr (create|edit|merge)|git/refs|scripts/release\.sh --(publish|promote)|scripts/release_pr\.sh|upload_ios\.sh --confirm|app_store_submit\.sh|notarytool|altool|contents: write|pull-requests: write'
bad=0
for file in $(git ls-files '.github/workflows/*.yml' '.github/workflows/*.yaml'); do
    [ -f "$file" ] || continue
    out=$(awk -v markers="$markers" -v file="$file" '
        function finish() {
            if (job != "" && hit != "" && !gated)
                printf "::error file=%s,line=%d::Job %s (%s:%d) can publish, tag, upload or move refs (%s) but its if: lacks vars.VORRA_RELEASES == '\''enabled'\''\n", file, start, job, file, start, hit
        }
        /^jobs:[[:space:]]*$/ { injobs = 1; next }
        injobs && /^[^[:space:]#]/ { finish(); injobs = 0; job = "" }
        !injobs { next }
        /^  [A-Za-z0-9_-]+:[[:space:]]*$/ { finish(); job = $1; sub(/:$/, "", job); start = NR; hit = ""; gated = 0; next }
        job != "" && /^    if:/ && index($0, "vars.VORRA_RELEASES == '\''enabled'\''") { gated = 1 }
        job != "" && hit == "" && match($0, markers) { hit = substr($0, RSTART, RLENGTH) }
        END { finish() }
    ' "$file")
    if [ -n "$out" ]; then printf '%s\n' "$out"; bad=1; fi
done
writes='gh release (create|delete|edit|upload)|git tag|git push|gh pr (create|edit|merge)|git/refs|asc_send|xcodebuild -exportArchive'
for file in $(git ls-files 'scripts/*.sh'); do
    case "$file" in scripts/release_gate.sh|scripts/asc_lib.sh) continue ;; esac
    [ -f "$file" ] || continue
    grep -qE "$writes" "$file" || continue
    grep -qE '"\$\{VORRA_RELEASES:-\}" (=|!=) enabled' "$file" && continue
    grep -nE "$writes" "$file" | head -1 | sed "s|^|::error file=$file::A publishing script without the VORRA_RELEASES=enabled check: $file:|"
    bad=1
done
[ "$bad" = 0 ] && echo "Every workflow job and script that can publish, tag, upload, move refs or open the release PR requires VORRA_RELEASES enabled."
exit "$bad"
