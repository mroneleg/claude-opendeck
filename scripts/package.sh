#!/usr/bin/env bash
# Assembles dist/com.mroneleg.claudeusage.sdPlugin from assets/ plus the
# binaries in BIN_DIR (default dist/bin, named claude-opendeck-<target>[.exe]),
# and zips it as dist/com.mroneleg.claudeusage.streamDeckPlugin with the
# .sdPlugin folder at the zip root (the layout OpenDeck installs from).
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
ID="com.mroneleg.claudeusage"
BIN_DIR="${1:-$ROOT/dist/bin}"
OUT="$ROOT/dist"

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
manifest_version="$(sed -n 's/^  "Version": "\(.*\)",/\1/p' "$ROOT/assets/manifest.json" | head -1)"
if [[ "$cargo_version" != "$manifest_version" ]]; then
  echo "version mismatch: Cargo.toml $cargo_version vs assets/manifest.json $manifest_version" >&2
  exit 1
fi

shopt -s nullglob
binaries=("$BIN_DIR"/claude-opendeck-*)
if [[ ${#binaries[@]} -eq 0 ]]; then
  echo "no claude-opendeck-<target> binaries in $BIN_DIR" >&2
  exit 1
fi

rm -rf "$OUT/$ID.sdPlugin" "$OUT/$ID.streamDeckPlugin"
mkdir -p "$OUT/$ID.sdPlugin"
cp -R "$ROOT/assets/." "$OUT/$ID.sdPlugin/"
for binary in "${binaries[@]}"; do
  cp "$binary" "$OUT/$ID.sdPlugin/"
  chmod +x "$OUT/$ID.sdPlugin/$(basename "$binary")"
done
(cd "$OUT" && zip -qr "$ID.streamDeckPlugin" "$ID.sdPlugin")
echo "packaged $OUT/$ID.streamDeckPlugin (v$cargo_version, ${#binaries[@]} binaries)"
