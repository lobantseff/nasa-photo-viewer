#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# install.sh — install NASA Photo Viewer from the portable tar.gz
#
# Copies the binary, icon and desktop entry so the application appears in the
# launcher. Prefer the bundled .deb on Debian and Ubuntu; this exists for
# everything else, and for installing without root.
#
# Usage:
#   cd nasa-photo-viewer-<version>/
#   ./install.sh              # into ~/.local, no sudo
#   sudo ./install.sh         # into /usr/local, system-wide
#   ./install.sh --uninstall  # remove again, matching the install prefix
# ---------------------------------------------------------------------------
set -euo pipefail

APP_NAME="nasa-photo-viewer"
DISPLAY_NAME="NASA Photo Viewer"
DESKTOP_NAME="$APP_NAME.desktop"

# Root installs system-wide, anyone else into their own home. Using the same
# rule when uninstalling is what makes the two operations symmetric.
if [ "$(id -u)" -eq 0 ]; then
    PREFIX="/usr/local"
    DESKTOP_DIR="/usr/share/applications"
    ICON_DIR="/usr/share/icons/hicolor/256x256/apps"
else
    PREFIX="$HOME/.local"
    DESKTOP_DIR="$HOME/.local/share/applications"
    ICON_DIR="$HOME/.local/share/icons/hicolor/256x256/apps"
fi
BIN_DIR="$PREFIX/bin"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

usage() {
    cat <<USAGE
Install $DISPLAY_NAME into your application launcher.

Usage:
  ./install.sh              Install into ~/.local (no sudo required)
  sudo ./install.sh         Install into /usr/local (system-wide)
  ./install.sh --uninstall  Remove a previous installation
  ./install.sh --help       Show this message
USAGE
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    -u|--uninstall)
        echo "Removing $DISPLAY_NAME from $PREFIX ..."
        rm -f "$BIN_DIR/$APP_NAME" "$ICON_DIR/$APP_NAME.png" "$DESKTOP_DIR/$DESKTOP_NAME"
        update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
        echo "Done."
        exit 0
        ;;
    "")
        ;;
    *)
        echo "error: unknown argument '$1'" >&2
        echo >&2
        usage >&2
        exit 2
        ;;
esac

for f in "$APP_NAME" "AppIcon.png" "$DESKTOP_NAME"; do
    if [ ! -f "$SCRIPT_DIR/$f" ]; then
        echo "error: $f is missing from $SCRIPT_DIR" >&2
        echo "Run this from inside the extracted archive." >&2
        exit 1
    fi
done

echo "Installing $DISPLAY_NAME into $PREFIX ..."
mkdir -p "$BIN_DIR" "$ICON_DIR" "$DESKTOP_DIR"

install -m 755 "$SCRIPT_DIR/$APP_NAME" "$BIN_DIR/$APP_NAME"
echo "  binary:  $BIN_DIR/$APP_NAME"

cp "$SCRIPT_DIR/AppIcon.png" "$ICON_DIR/$APP_NAME.png"
echo "  icon:    $ICON_DIR/$APP_NAME.png"

# Written rather than copied so Exec and Icon carry absolute paths, which is
# what makes the entry work from a prefix that is not on the default search
# path.
sed \
    -e "s|^Exec=.*|Exec=$BIN_DIR/$APP_NAME|" \
    -e "s|^Icon=.*|Icon=$ICON_DIR/$APP_NAME.png|" \
    "$SCRIPT_DIR/$DESKTOP_NAME" > "$DESKTOP_DIR/$DESKTOP_NAME"
echo "  desktop: $DESKTOP_DIR/$DESKTOP_NAME"

update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true

echo
echo "Installed. Search for '$DISPLAY_NAME' in your launcher."
if [ "$(id -u)" -ne 0 ] && ! echo ":$PATH:" | grep -q ":$BIN_DIR:"; then
    echo
    echo "$BIN_DIR is not on your PATH; to run it from a terminal add:"
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
fi
