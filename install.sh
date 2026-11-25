#!/bin/sh
# Install the latest Concord release binary.
#
#   curl -fsSL https://raw.githubusercontent.com/lucas-rus/concord/main/install.sh | sh
#
# Installs to $CONCORD_INSTALL_DIR (default: ~/.local/bin).
set -eu

REPO="lucas-rus/concord"
DIR="${CONCORD_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
  Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-gnu ;;
  *) echo "concord: no prebuilt binary for $(uname -s) $(uname -m); build from source with cargo" >&2; exit 1 ;;
esac

TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
[ -n "$TAG" ] || { echo "concord: could not find the latest release" >&2; exit 1; }

NAME="concord-$TAG-$TARGET"
URL="https://github.com/$REPO/releases/download/$TAG/$NAME.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $NAME ..."
curl -fsSL "$URL" -o "$TMP/$NAME.tar.gz"
curl -fsSL "$URL.sha256" -o "$TMP/$NAME.tar.gz.sha256"
(cd "$TMP" && shasum -a 256 -c "$NAME.tar.gz.sha256" >/dev/null) || { echo "concord: checksum mismatch" >&2; exit 1; }
tar -C "$TMP" -xzf "$TMP/$NAME.tar.gz"

mkdir -p "$DIR"
install -m 755 "$TMP/$NAME/concord" "$DIR/concord"
echo "Installed concord $TAG to $DIR/concord"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "Note: $DIR is not on your PATH; add it, or use the full path in your MCP configuration." ;;
esac
if [ "${1:-}" = "--setup" ]; then
  shift
  "$DIR/concord" setup "$@"
else
  echo "Next: run '$DIR/concord setup' to connect OpenCode, Claude Code, or Cursor automatically."
fi
