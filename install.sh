#!/bin/sh
# Lightbulb installer: downloads the latest release (or $LIGHTBULB_VERSION, a tag like v0.2.2) for this Mac or Linux
# computer, checks its checksum, puts it at ~/.lightbulb/connect/bin/lightbulb and runs `setup`
# (which wires Claude Code, Codex, Cursor and Grok CLI and, on a Mac, starts the background service).
# Re-running it upgrades an existing install in place.
set -eu

REPO="TheLightbulbCompany/cli"
case "$(uname -s) $(uname -m)" in
  "Darwin arm64") ASSET="lightbulb-macos-arm64.tar.gz" ;;
  "Linux aarch64") ASSET="lightbulb-linux-arm64.tar.gz" ;;
  "Linux x86_64") ASSET="lightbulb-linux-x86_64.tar.gz" ;;
  *) echo "Lightbulb runs on Apple-silicon Macs and on Linux (arm64, x86_64)." >&2; exit 1 ;;
esac
if [ -n "${LIGHTBULB_VERSION:-}" ]; then
  case "$LIGHTBULB_VERSION" in v*) ;; *) LIGHTBULB_VERSION="v$LIGHTBULB_VERSION" ;; esac
  BASE="https://github.com/$REPO/releases/download/$LIGHTBULB_VERSION"
else
  BASE="https://github.com/$REPO/releases/latest/download"
fi

command -v curl >/dev/null || { echo "curl is needed." >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading Lightbulb…"
curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"
curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256"
if command -v shasum >/dev/null; then SUM="shasum -a 256"; else SUM="sha256sum"; fi
( cd "$TMP" && $SUM -c "$ASSET.sha256" >/dev/null ) || { echo "Checksum mismatch; not installing." >&2; exit 1; }

BIN="$HOME/.lightbulb/connect/bin"
mkdir -p "$BIN"
tar -xzf "$TMP/$ASSET" -C "$TMP"
install -m 0755 "$TMP/lightbulb" "$BIN/lightbulb"
echo "Installed $BIN/lightbulb"
[ "$(uname -s)" = "Linux" ] || exec "$BIN/lightbulb" setup
# Linux: no background service is installed.
"$BIN/lightbulb" setup || exit $?
echo "Start it with: lightbulb daemon (or as a systemd --user service)."
