#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
REPO=${REPO:-izyuumi/everything}
VERSION=${1:?usage: scripts/release_pr.sh <version> <build> <source sha> <sha256:SHA256SUMS digest>}
BUILD=${2:?}
SHA=${3:?}
DIGEST=${4:?}
NOTES=$(scripts/release_notes.sh "$SHA")
OLD=$(git show "$SHA:CHANGELOG.md" 2>/dev/null | sed '1,2d' || true)
FILE=$(mktemp)
printf '# Changelog\n\n## %s\n\n%s\n\n%s' "$VERSION" "$(printf '%s' "${NOTES:-No notable changes.}" | sed 's/^## /### /')" "$OLD" > "$FILE"
gh api -X PATCH "repos/$REPO/git/refs/heads/release" -f sha="$SHA" -F force=true >/dev/null 2>&1 \
    || gh api "repos/$REPO/git/refs" -f ref=refs/heads/release -f sha="$SHA" >/dev/null
blob=$(gh api "repos/$REPO/contents/CHANGELOG.md?ref=release" -q .sha 2>/dev/null || true)
gh api -X PUT "repos/$REPO/contents/CHANGELOG.md" -f message="chore: release $VERSION" -f branch=release \
    -f content="$(base64 < "$FILE" | tr -d '\n')" ${blob:+-f sha="$blob"} >/dev/null
rm -f "$FILE"
BODY="Merging this makes the CLI beta \`v$VERSION-beta\` (build $BUILD) stable as \`v$VERSION\` and deletes the beta. A newer beta updates this PR to name it.

$NOTES

Release-Build: $BUILD
Release-Commit: $SHA
Release-Digest: $DIGEST"
PR=$(gh pr list --repo "$REPO" --head release --state open --json number -q '.[].number' | head -1)
if [ -n "$PR" ]; then
    gh pr edit "$PR" --repo "$REPO" --title "chore: release $VERSION" --body "$BODY" >/dev/null
else
    gh pr create --repo "$REPO" --head release --base main --title "chore: release $VERSION" --body "$BODY" >/dev/null
fi
for workflow in validate.yml apple-validation.yml; do
    gh workflow run "$workflow" --repo "$REPO" --ref release \
        || echo "::warning::Could not start $workflow on the release branch; run it by hand so the release PR's required checks report."
done
echo "Release PR for $VERSION names build $BUILD."
