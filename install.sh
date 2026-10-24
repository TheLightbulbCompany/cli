#!/bin/sh
# Lightbulb installer: downloads the latest release (or $LIGHTBULB_VERSION, a tag like v0.2.2) for this Mac, checks its checksum, puts it
# at ~/.lightbulb/connect/bin/lightbulb and runs `setup`
# (which wires Claude Code, Codex, Cursor and Grok CLI and starts the background service).
# Re-running it upgrades an existing install in place.
set -eu

REPO="TheLightbulbCompany/cli"
ASSET="lightbulb-macos-arm64.tar.gz"
if [ -n "${LIGHTBULB_VERSION:-}" ]; then
  case "$LIGHTBULB_VERSION" in v*) ;; *) LIGHTBULB_VERSION="v$LIGHTBULB_VERSION" ;; esac
  BASE="https://github.com/$REPO/releases/download/$LIGHTBULB_VERSION"
else
  BASE="https://github.com/$REPO/releases/latest/download"
fi

[ "$(uname -s)" = "Darwin" ] || { echo "Lightbulb runs on macOS for now." >&2; exit 1; }
[ "$(uname -m)" = "arm64" ] || { echo "Lightbulb needs an Apple-silicon Mac for now." >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is needed." >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading Lightbulb…"
curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"
curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256"
( cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null ) || { echo "Checksum mismatch; not installing." >&2; exit 1; }

BIN="$HOME/.lightbulb/connect/bin"
mkdir -p "$BIN"
tar -xzf "$TMP/$ASSET" -C "$TMP"
install -m 0755 "$TMP/lightbulb" "$BIN/lightbulb"
echo "Installed $BIN/lightbulb"
exec "$BIN/lightbulb" setup
