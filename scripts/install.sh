#!/usr/bin/env bash
# Builds for this machine, packages, and copies the plugin into OpenDeck's
# plugins folder. It must be a real copy: OpenDeck's file server refuses files
# under a symlinked plugin folder (icons and the property inspector break).
# Restart OpenDeck afterwards. Set OPENDECK_PLUGINS_DIR to override the target.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
ID="com.mroneleg.claudeusage"
cd "$ROOT"

target="$(rustc -vV | sed -n 's/^host: //p')"
cargo build --release
mkdir -p dist/bin
rm -f dist/bin/claude-opendeck-*
ext=""; [[ "$target" == *windows* ]] && ext=".exe"
cp "target/release/claude-opendeck$ext" "dist/bin/claude-opendeck-$target$ext"
scripts/package.sh dist/bin

if [[ -n "${OPENDECK_PLUGINS_DIR:-}" ]]; then
  plugins="$OPENDECK_PLUGINS_DIR"
elif [[ "$(uname)" == "Linux" ]]; then
  plugins="${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins"
else
  echo "set OPENDECK_PLUGINS_DIR (OpenDeck: Settings -> Open config directory -> plugins)" >&2
  exit 1
fi
mkdir -p "$plugins"
rm -rf "$plugins/$ID.sdPlugin"
cp -R "dist/$ID.sdPlugin" "$plugins/"
echo "installed to $plugins/$ID.sdPlugin; restart OpenDeck to load it"
