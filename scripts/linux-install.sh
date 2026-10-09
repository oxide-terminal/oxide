#!/bin/bash
# Install (or uninstall) OmniPTY from the Linux release tarball.
#
#   ./install.sh                 into ~/.local (no root needed)
#   sudo ./install.sh --prefix /usr/local
#   ./install.sh --uninstall     remove what a previous run installed
#
# Puts the binary on PATH as `omnipty`, the .desktop entry where launchers
# look, and the icon into the hicolor theme. Arch users: prefer the AUR
# package (omnipty-bin), which does the same through pacman.
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
UNINSTALL=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$PREFIX/bin/omnipty"
DESKTOP="$PREFIX/share/applications/omnipty.desktop"
ICONS="$PREFIX/share/icons/hicolor"

if [[ $UNINSTALL -eq 1 ]]; then
  rm -f "$BIN" "$DESKTOP"
  for dir in "$ICONS"/*/apps; do rm -f "$dir/omnipty.png"; done
  command -v update-desktop-database >/dev/null && update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$ICONS" 2>/dev/null || true
  echo "removed omnipty from $PREFIX"
  exit 0
fi

[[ -x "$HERE/omnipty" ]] || { echo "error: run this from the unpacked tarball (no ./omnipty here)" >&2; exit 1; }

# An install from before the rename (0.8.x) under the same prefix.
if [[ -e "$PREFIX/bin/oxide" ]]; then
  rm -f "$PREFIX/bin/oxide" "$PREFIX/share/applications/oxide.desktop"
  for dir in "$ICONS"/*/apps; do rm -f "$dir/oxide.png"; done
  echo "removed the old oxide install from $PREFIX"
fi

install -Dm755 "$HERE/omnipty" "$BIN"
install -Dm644 "$HERE/omnipty.desktop" "$DESKTOP"
for png in "$HERE"/icons/hicolor/*/apps/omnipty.png; do
  size="$(basename "$(dirname "$(dirname "$png")")")"
  install -Dm644 "$png" "$ICONS/$size/apps/omnipty.png"
done
command -v update-desktop-database >/dev/null && update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$ICONS" 2>/dev/null || true

echo "installed $BIN"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "note: $PREFIX/bin is not on your PATH" ;;
esac
