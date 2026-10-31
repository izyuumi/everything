#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
PLAIN=0; intro=; note=; feat=Features; fix=Fixes; perf=Performance; more=
if [ "${1:-}" = --what-to-test ]; then
    [ $# -ge 4 ] || { echo "usage: $0 --what-to-test <locale> <version> <build> [<commit>]" >&2; exit 64; }
    PLAIN=1; labels=apps/Apple/metadata/testflight/$2/what_to_test.txt
    [ -r "$labels" ] || { echo "No What to Test labels for locale '$2' ($labels)" >&2; exit 1; }
    for key in intro note feat fix perf more; do
        grep -q "^$key=" "$labels" || { echo "$labels has no $key= line" >&2; exit 1; }
    done
    label() { sed -n "s/^$1=//p" "$labels" | head -n 1; }
    intro=$(label intro | sed "s/{version}/$3/g; s/{build}/$4/g"); note=$(label note)
    feat=$(label feat); fix=$(label fix); perf=$(label perf); more=$(label more)
    shift 4
fi
TO=${1:-HEAD}
FROM=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*' "$TO^" 2>/dev/null || true)
git log --first-parent --format='%s%x1f%b%x1e' "${FROM:+$FROM..}$TO" | LC_ALL=C awk -v plain="$PLAIN" -v intro="$intro" -v note="$note" \
    -v lfeat="$feat" -v lfix="$fix" -v lperf="$perf" -v lmore="$more" '
    BEGIN { RS = "\036"; FS = "\037" }
    {
        subject = $1; sub(/^\n+/, "", subject); body = $2; number = ""
        if (subject ~ /^Merge pull request #[0-9]+ from /) {
            split(subject, word, " "); number = word[4]
            title = body; sub(/^\n+/, "", title); sub(/\n.*/, "", title)
        } else if (match(subject, / \(#[0-9]+\)$/)) {
            number = substr(subject, RSTART + 2, RLENGTH - 3); title = substr(subject, 1, RSTART - 1)
        } else if (subject ~ /^Merge /) {
            next
        } else {
            title = subject
        }
        if (!match(title, /^[a-z]+(\([^)]*\))?!?: /)) next
        head = substr(title, 1, RLENGTH - 2); text = substr(title, RLENGTH + 1)
        sub(/ *\[skip ci\]$/, "", text)
        type = head; sub(/[(!].*/, "", type)
        scope = ""; if (match(head, /\([^)]*\)/)) scope = substr(head, RSTART + 1, RLENGTH - 2)
        if (scope == "deps" || scope == "deps-dev") next
        if (type != "feat" && type != "fix" && type != "perf") next
        breaking = head ~ /!$/
        if (plain) line = "- " (breaking ? "Breaking: " : "") (scope != "" ? scope ": " : "") text (number != "" ? " (" number ")" : "")
        else {
            gsub(/&/, "\\&amp;", text); gsub(/</, "\\&lt;", text); gsub(/>/, "\\&gt;", text)
            line = "- " (breaking ? "**Breaking:** " : "") (scope != "" ? "**" scope ":** " : "") text (number != "" ? " (" number ")" : "")
        }
        count[type]++; item[type, count[type]] = line
    }
    END {
        name["feat"] = lfeat; name["fix"] = lfix; name["perf"] = lperf
        order[1] = "feat"; order[2] = "fix"; order[3] = "perf"
        out = ""
        if (plain) out = intro "\n" (note != "" ? note "\n" : "") "\n"
        size = length(out); closing = lmore; gsub(/\{count\}/, "9999", closing); room = length(closing) + 1; left = 0
        for (o = 1; o <= 3; o++) {
            t = order[o]; if (!count[t]) continue
            heading = plain ? name[t] "\n" : "## " name[t] "\n\n"
            shown = 0
            for (i = 1; i <= count[t]; i++) {
                if (plain && size + (shown ? 0 : length(heading)) + length(item[t, i]) + 2 + room > 4000) { left += count[t] - i + 1; break }
                if (!shown) { out = out heading; size += length(heading); shown = 1 }
                out = out item[t, i] "\n"; size += length(item[t, i]) + 1
            }
            if (shown) { out = out "\n"; size++ }
        }
        printf "%s", out
        if (left) { closing = lmore; gsub(/\{count\}/, left, closing); printf "%s\n", closing }
    }'
