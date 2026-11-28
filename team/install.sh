#!/bin/sh
# Install concord-team (and concord, if missing) into ~/.local/bin.
#   curl -fsSL https://raw.githubusercontent.com/lucas-rus/concord/teams/team/install.sh | sh
set -eu
VERSION="0.2.0"
BIN="${CONCORD_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$BIN"
command -v python3 >/dev/null 2>&1 || { echo "concord-team needs python3 (macOS: xcode-select --install)" >&2; exit 1; }
if ! command -v concord >/dev/null 2>&1 && [ ! -x "$BIN/concord" ]; then
  echo "installing concord..."
  curl -fsSL https://raw.githubusercontent.com/lucas-rus/concord/main/install.sh | sh
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "https://github.com/lucas-rus/concord/releases/download/team-v$VERSION/concord-team-$VERSION.tar.gz" | tar xz -C "$TMP"
install -m 755 "$TMP/concord-team-$VERSION/team/concord_team.py" "$BIN/concord-team"
echo "installed concord-team $VERSION to $BIN/concord-team"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "add $BIN to your PATH:  echo 'export PATH=\"$BIN:\$PATH\"' >> ~/.zshrc" ;; esac
command -v tailscale >/dev/null 2>&1 || [ -d /Applications/Tailscale.app ] || echo "next: install Tailscale (https://tailscale.com/download) and log in"
echo "then, in your clone:  concord-team init   (or: concord-team join <invite>)"
