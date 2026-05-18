#!/usr/bin/env bash
# Installs the app icon + .desktop into the user's local data dir (no sudo).
# After this, the dock/taskbar icon resolves (runtime icon-name alone is
# unreliable on Wayland).
#
# NOTE: we deliberately do NOT create ~/.local/share/icons/hicolor/index.theme
# or run gtk-update-icon-cache on it. A hand-written index.theme that lists
# only scalable/apps shadows every other app's icons in that same dir (e.g.
# Telegram's 256x256/apps PNG) and makes them fall back to a generic icon.
# scalable/apps is a standard hicolor dir, so the system hicolor theme
# resolves our SVG without any local index.theme at all.
set -e
APPID=dev.local.claude_code_linux_gui
HERE="$(cd "$(dirname "$0")" && pwd)"
ICON_SRC="$HERE/assets/hicolor/scalable/apps/$APPID.svg"
DESK_SRC="$HERE/assets/$APPID.desktop"
ICON_DST="$HOME/.local/share/icons/hicolor/scalable/apps"
DESK_DST="$HOME/.local/share/applications"

mkdir -p "$ICON_DST" "$DESK_DST"
cp "$ICON_SRC" "$ICON_DST/"
cp "$DESK_SRC" "$DESK_DST/"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$DESK_DST" 2>/dev/null || true
fi

echo "Installed:"
echo "  $ICON_DST/$APPID.svg"
echo "  $DESK_DST/$APPID.desktop"
echo "Log out/in (or restart GNOME Shell) so the dock picks up the icon."

# Ubuntu 23.10+ blocks unprivileged user namespaces by default, which makes
# WebKitGTK's bwrap sandbox (and thus this app) crash on launch. Detect that
# and point the user at the AppArmor installer (needs sudo, so we don't run
# it from here — this script is intentionally sudo-free).
RESTRICT=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
if [ -r "$RESTRICT" ] && [ "$(cat "$RESTRICT")" = "1" ] \
   && [ ! -f /etc/apparmor.d/$APPID ] \
   && [ ! -f /etc/apparmor.d/claude-code-linux-gui ]; then
  echo
  echo "NOTE: this system restricts unprivileged user namespaces, so the app"
  echo "      will crash with 'bwrap: setting up uid map: Permission denied'"
  echo "      until you install its AppArmor profile:"
  echo
  echo "        sudo $HERE/install-apparmor.sh"
fi
