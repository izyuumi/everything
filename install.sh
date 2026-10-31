#!/bin/sh
set -eu
REPO=${VORRA_REPO:-izyuumi/everything}
INSTALL_DIR=${VORRA_INSTALL_DIR:-$HOME/.local/bin}
TAG=
while [ $# -gt 0 ]; do
    case "$1" in
        --beta) TAG=beta ;;
        --version) TAG=${2:?--version needs a tag such as v0.2.0}; shift ;;
        --dir) INSTALL_DIR=${2:?--dir needs a directory}; shift ;;
        -h|--help) echo "Usage: install.sh [--beta | --version <tag>] [--dir <directory>]"; exit 0 ;;
        *) echo "Unknown option $1" >&2; exit 64 ;;
    esac
    shift
done
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) TARGET=aarch64-apple-darwin ;;
    Darwin/x86_64) TARGET=x86_64-apple-darwin ;;
    Linux/x86_64) TARGET=x86_64-unknown-linux-gnu ;;
    Linux/aarch64|Linux/arm64) TARGET=aarch64-unknown-linux-gnu ;;
    *) echo "No vorra build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
if [ -n "${VORRA_DOWNLOAD_URL:-}" ]; then
    BASE=$VORRA_DOWNLOAD_URL
elif [ "$TAG" = beta ]; then
    TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=30" \
        | sed -n 's/.*"tag_name": *"\(v[0-9]*\.[0-9]*\.[0-9]*-beta\)".*/\1/p' | head -1)
    [ -n "$TAG" ] || { echo "There is no beta release of $REPO" >&2; exit 1; }
    BASE=https://github.com/$REPO/releases/download/$TAG
elif [ -n "$TAG" ]; then
    BASE=https://github.com/$REPO/releases/download/$TAG
else
    BASE=https://github.com/$REPO/releases/latest/download
fi
ASSET=vorra-$TARGET.tar.gz
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
curl -fsSL -o "$WORK/$ASSET" "$BASE/$ASSET" \
    || { echo "Could not download $BASE/$ASSET; without a stable release yet, try --beta" >&2; exit 1; }
curl -fsSL -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS"
if command -v sha256sum >/dev/null; then SUM="sha256sum"; else SUM="shasum -a 256"; fi
want=$(sed -n "s/^\([0-9a-f]\{64\}\)  $ASSET\$/\1/p" "$WORK/SHA256SUMS")
have=$($SUM "$WORK/$ASSET" | cut -d' ' -f1)
[ -n "$want" ] && [ "$want" = "$have" ] || { echo "$ASSET does not match SHA256SUMS" >&2; exit 1; }
tar -xzf "$WORK/$ASSET" -C "$WORK" vorra
mkdir -p "$INSTALL_DIR"
mv -f "$WORK/vorra" "$INSTALL_DIR/vorra"
chmod 755 "$INSTALL_DIR/vorra"
echo "Installed $("$INSTALL_DIR/vorra" --version) to $INSTALL_DIR/vorra"
case ":$PATH:" in *":$INSTALL_DIR:"*) ;; *) echo "Add $INSTALL_DIR to PATH to run vorra." ;; esac
