#!/bin/sh
# Lightbulb Connect installer: downloads the latest release for this Mac, checks its
# checksum, puts it at ~/.lightbulb/connect/bin/lightbulb (with lightbulb-connect, the old name,
# a link beside it) and runs `setup`
# (which wires Claude Code, Codex, Cursor and Grok CLI and starts the background service).
# Re-running it upgrades an existing install in place.
set -eu

REPO="TheLightbulbCompany/lightbulb-connect"
ASSET="lightbulb-connect-macos-arm64.tar.gz"
BASE="https://github.com/$REPO/releases/latest/download"

[ "$(uname -s)" = "Darwin" ] || { echo "Lightbulb Connect runs on macOS for now." >&2; exit 1; }
[ "$(uname -m)" = "arm64" ] || { echo "Lightbulb Connect needs an Apple-silicon Mac for now." >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is needed." >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading Lightbulb Connect…"
curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"
curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256"
( cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null ) || { echo "Checksum mismatch; not installing." >&2; exit 1; }

BIN="$HOME/.lightbulb/connect/bin"
mkdir -p "$BIN"
tar -xzf "$TMP/$ASSET" -C "$TMP"
install -m 0755 "$TMP/lightbulb" "$BIN/lightbulb"
# The old name: hooks and launchd jobs from 0.2.1 run it. -n replaces 0.2.1's binary there.
ln -sfn lightbulb "$BIN/lightbulb-connect"
echo "Installed $BIN/lightbulb"
exec "$BIN/lightbulb" setup
