#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
REPO=${REPO:-izyuumi/everything}
TARGETS="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"
beta_tag() {
    gh release list --repo "$REPO" --limit 100 --json tagName,isPrerelease -q '.[] | select(.isPrerelease) | .tagName' \
        | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+-beta$' | head -1 || true
}
digest() { printf 'sha256:%s' "$(shasum -a 256 "$1" | cut -d' ' -f1)"; }

case "${1:-}" in
--next)
    VERSION=${VERSION:-$(scripts/version.sh)}
    BETA=$(beta_tag)
    previous=
    if [ "$BETA" = "v$VERSION-beta" ]; then
        previous=$(gh release download "$BETA" --repo "$REPO" --pattern candidate.json --output - 2>/dev/null \
            | jq -r '.build // empty' || true)
    fi
    case "$previous" in ''|*[!0-9]*) previous=0 ;; esac
    echo "version=$VERSION"
    echo "build=$((previous + 1))"
    ;;

--publish)
    DIST=${2:?usage: scripts/release.sh --publish <dir with the tarballs>}
    VERSION=${VERSION:?VERSION is required}
    BUILD=${BUILD:?BUILD is required}
    SOURCE_SHA=$(git rev-parse HEAD)
    TAG="v$VERSION-beta"
    [ -z "$(git status --porcelain)" ] || { echo "Refusing to publish from a dirty tree: candidate.json names the commit." >&2; exit 1; }
    for target in $TARGETS; do
        [ -f "$DIST/everything-$target.tar.gz" ] || { echo "Missing $DIST/everything-$target.tar.gz" >&2; exit 1; }
    done
    (cd "$DIST" && shasum -a 256 everything-*.tar.gz > SHA256SUMS)
    jq -n --arg tag "$TAG" --arg version "$VERSION" --arg build "$BUILD" --arg sha "$SOURCE_SHA" --arg targets "$TARGETS" \
        '{schema: 1, tag: $tag, version: $version, build: ($build | tonumber), source_branch: "main", source_sha: $sha,
          assets: ($targets | split(" ") | map({target: ., name: "everything-\(.).tar.gz"}))}' > "$DIST/candidate.json"
    { echo "everything $VERSION+$BUILD, beta."; echo; scripts/release_notes.sh "$SOURCE_SHA"; } > "$DIST.notes.md"
    BETA=$(beta_tag)
    [ -z "$BETA" ] || gh release delete "$BETA" --repo "$REPO" --cleanup-tag --yes
    gh release create "$TAG" --repo "$REPO" --target "$SOURCE_SHA" --prerelease --latest=false --title "$TAG" \
        --notes-file "$DIST.notes.md" "$DIST"/everything-*.tar.gz "$DIST/SHA256SUMS" "$DIST/candidate.json"
    echo "Published $TAG."
    scripts/release_pr.sh "$VERSION" "$BUILD" "$SOURCE_SHA" "$(digest "$DIST/SHA256SUMS")"
    ;;

--promote)
    N=${2:-}; WANT_SHA=${3:-}; WANT_DIGEST=${4:-}
    usage="Usage: scripts/release.sh --promote <build> <source sha> <sha256:SHA256SUMS digest>, from the release PR's Release-Build, Release-Commit and Release-Digest lines"
    case "$N" in ''|*[!0-9]*) echo "$usage" >&2; exit 64 ;; esac
    printf '%s' "$WANT_SHA" | grep -qE '^[0-9a-f]{40}$' || { echo "$usage (Release-Commit is '$WANT_SHA')" >&2; exit 64; }
    printf '%s' "$WANT_DIGEST" | grep -qE '^sha256:[0-9a-f]{64}$' || { echo "$usage (Release-Digest is '$WANT_DIGEST')" >&2; exit 64; }
    BETA=$(beta_tag)
    [ -n "$BETA" ] || { echo "There is no beta to promote" >&2; exit 1; }
    OLD=${BETA#v}; OLD=${OLD%-beta}
    if gh release view "v$OLD" --repo "$REPO" >/dev/null 2>&1; then
        echo "v$OLD is already released; nothing to promote."
        exit 0
    fi
    FILES=$(mktemp -d)
    gh release view "$BETA" --repo "$REPO" --json tagName,targetCommitish,assets > "$FILES.release.json"
    gh release download "$BETA" --repo "$REPO" --dir "$FILES"
    bad=
    mismatch() { bad=1; echo "::error::$BETA no longer matches the release PR: $1 is ${2:-missing}, the PR names $3."; }
    have=$(jq -r '.build // empty' "$FILES/candidate.json" 2>/dev/null || true)
    [ "$have" = "$N" ] || mismatch "candidate.json's build" "$have" "$N"
    have=$(jq -r '.tag // empty' "$FILES/candidate.json" 2>/dev/null || true)
    [ "$have" = "$BETA" ] || mismatch "candidate.json's tag" "$have" "$BETA"
    have=$(jq -r '.version // empty' "$FILES/candidate.json" 2>/dev/null || true)
    [ "$have" = "$OLD" ] || mismatch "candidate.json's version" "$have" "$OLD"
    have=$(jq -r '.source_sha // empty' "$FILES/candidate.json" 2>/dev/null || true)
    [ "$have" = "$WANT_SHA" ] || mismatch "candidate.json's source commit" "$have" "$WANT_SHA"
    have=$(jq -r '.targetCommitish // empty' "$FILES.release.json")
    [ "$have" = "$WANT_SHA" ] || mismatch "the release's target commit" "$have" "$WANT_SHA"
    have=$(gh api "repos/$REPO/commits/$BETA" -q .sha 2>/dev/null || true)
    [ "$have" = "$WANT_SHA" ] || mismatch "the commit the tag $BETA names" "$have" "$WANT_SHA"
    have=$(jq -r '.assets[] | select(.name == "SHA256SUMS") | .digest // empty' "$FILES.release.json")
    [ "$have" = "$WANT_DIGEST" ] || mismatch "SHA256SUMS's digest on GitHub" "$have" "$WANT_DIGEST"
    have=$([ -f "$FILES/SHA256SUMS" ] && digest "$FILES/SHA256SUMS" || true)
    [ "$have" = "$WANT_DIGEST" ] || mismatch "the downloaded SHA256SUMS's digest" "$have" "$WANT_DIGEST"
    for target in $TARGETS; do
        grep -qE "  everything-$target\.tar\.gz\$" "$FILES/SHA256SUMS" 2>/dev/null \
            || mismatch "SHA256SUMS's entry for $target" "" "everything-$target.tar.gz"
    done
    (cd "$FILES" && shasum -a 256 -c SHA256SUMS >/dev/null 2>&1) || mismatch "the downloaded tarballs" "different from SHA256SUMS" "matching tarballs"
    if [ -n "$bad" ]; then
        echo "Not promoting $BETA: it is not the beta the release PR named, so nothing was published or deleted. The next beta opens a new release PR; check that beta, then merge it." >&2
        exit 1
    fi
    { echo "everything $OLD+$N."; echo; scripts/release_notes.sh "$WANT_SHA"; } > "$FILES.md"
    gh release create "v$OLD" --repo "$REPO" --target "$WANT_SHA" --latest --title "v$OLD" --notes-file "$FILES.md" "$FILES"/*
    gh release delete "$BETA" --repo "$REPO" --cleanup-tag --yes
    echo "Published v$OLD."
    ;;

*)
    echo "Usage: scripts/release.sh --next | --publish <dir> | --promote <build> <source sha> <sha256:digest>" >&2
    exit 64
    ;;
esac
