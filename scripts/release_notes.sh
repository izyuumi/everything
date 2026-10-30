#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
TO=${1:-HEAD}
FROM=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*' "$TO^" 2>/dev/null || true)
git log --first-parent --format='%s%x1f%b%x1e' "${FROM:+$FROM..}$TO" | LC_ALL=C awk '
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
        gsub(/&/, "\\&amp;", text); gsub(/</, "\\&lt;", text); gsub(/>/, "\\&gt;", text)
        line = "- " (breaking ? "**Breaking:** " : "") (scope != "" ? "**" scope ":** " : "") text (number != "" ? " (" number ")" : "")
        count[type]++; item[type, count[type]] = line
    }
    END {
        name["feat"] = "Features"; name["fix"] = "Fixes"; name["perf"] = "Performance"
        order[1] = "feat"; order[2] = "fix"; order[3] = "perf"
        for (o = 1; o <= 3; o++) {
            t = order[o]; if (!count[t]) continue
            out = out "## " name[t] "\n\n"
            for (i = 1; i <= count[t]; i++) out = out item[t, i] "\n"
            out = out "\n"
        }
        printf "%s", out
    }'
