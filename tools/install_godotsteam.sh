#!/usr/bin/env bash
# Download the GodotSteam GDExtension pinned in .godotsteam-version into addons/godotsteam/ (ignored by
# git: nothing of GodotSteam or Steam's redistributables is committed). gdnet's connect layer
# (addons/gdnet/connect/, docs/guide/12_playing_online.md) finds it at run time; the project works
# without it. Not used by CI or the session hook: the check never needs Steam.
#
# Idempotent: an installed copy of the pinned version is kept (addons/godotsteam/.installed-version).
# Overrides: GODOTSTEAM_VERSION (the version), GODOTSTEAM_URL (the archive's URL), GODOTSTEAM_SHA256
# (its SHA-256; default: .godotsteam-sha256 when that file exists and is not empty).
#
# UNVERIFIED (2026-11-13): the release URL below follows GodotSteam's release naming as remembered; the
# releases moved to codeberg.org, which this script's author could not reach. If the download fails with
# 404, open https://codeberg.org/godotsteam/godotsteam/releases, copy the GDExtension plugin archive's
# link for the pinned version, and run GODOTSTEAM_URL=<link> tools/install_godotsteam.sh (then fix the
# default below and write the archive's SHA-256 into .godotsteam-sha256).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${GODOTSTEAM_VERSION:-$(tr -d '[:space:]' < "$ROOT/.godotsteam-version" 2>/dev/null || echo 4.24)}"
URL="${GODOTSTEAM_URL:-https://codeberg.org/godotsteam/godotsteam/releases/download/v${VERSION}-gde/godotsteam-${VERSION}-gdextension-plugin-4.4.zip}"
SHA="${GODOTSTEAM_SHA256:-}"
if [ -z "$SHA" ] && [ -f "$ROOT/.godotsteam-sha256" ]; then
  SHA="$(tr -d '[:space:]' < "$ROOT/.godotsteam-sha256")"
fi
DEST="$ROOT/addons/godotsteam"
STAMP="$DEST/.installed-version"

if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$VERSION" ] && ls "$DEST"/*.gdextension >/dev/null 2>&1; then
  echo "GodotSteam $VERSION is already installed in addons/godotsteam/"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "Downloading $URL" >&2
curl -fsSL --retry 3 --retry-delay 2 -o "$TMP/godotsteam.zip" "$URL"

if [ -n "$SHA" ]; then
  echo "$SHA  $TMP/godotsteam.zip" | sha256sum -c - >&2
else
  echo "install_godotsteam.sh: no checksum pinned (.godotsteam-sha256 is missing or empty); archive SHA-256:" >&2
  sha256sum "$TMP/godotsteam.zip" | cut -d' ' -f1 >&2
fi

unzip -q "$TMP/godotsteam.zip" -d "$TMP/x"
# The archive holds addons/godotsteam/ (a .gdextension and per-platform libraries, Steam's steam_api
# among them); find that folder wherever the archive put it.
GDEXT="$(find "$TMP/x" -name '*.gdextension' -path '*godotsteam*' | head -n 1)"
if [ -z "$GDEXT" ]; then
  echo "install_godotsteam.sh: no godotsteam .gdextension in the archive; is $URL the GDExtension plugin?" >&2
  exit 1
fi
SRC="$(dirname "$GDEXT")"
rm -rf "$DEST"
mkdir -p "$(dirname "$DEST")"
cp -R "$SRC" "$DEST"
echo "$VERSION" > "$STAMP"
echo "GodotSteam $VERSION installed in addons/godotsteam/ ($(basename "$GDEXT"))"
