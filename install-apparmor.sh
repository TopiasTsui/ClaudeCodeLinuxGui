#!/usr/bin/env bash
# Installs the AppArmor profile that re-grants unprivileged user-namespace
# creation to this app's binary. Required on Ubuntu 23.10+ where
# kernel.apparmor_restrict_unprivileged_userns=1 otherwise makes WebKitGTK's
# bwrap sandbox fail ("bwrap: setting up uid map: Permission denied").
#
# Run as root:   sudo ./install-apparmor.sh
# Uninstall:     sudo ./install-apparmor.sh --uninstall
set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
PROFILE_NAME=claude-code-linux-gui
SRC="$HERE/assets/apparmor/$PROFILE_NAME"
DST="/etc/apparmor.d/$PROFILE_NAME"

if [ "$(id -u)" -ne 0 ]; then
  echo "Must run as root. Try:  sudo $0 $*" >&2
  exit 1
fi

if ! command -v apparmor_parser >/dev/null 2>&1; then
  echo "apparmor_parser not found — is AppArmor installed/enabled?" >&2
  exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
  if [ -f "$DST" ]; then
    apparmor_parser -R "$DST" 2>/dev/null || true
    rm -f "$DST"
    echo "Removed and unloaded: $DST"
  else
    echo "Nothing to remove ($DST not present)."
  fi
  exit 0
fi

if [ ! -f "$SRC" ]; then
  echo "Profile source missing: $SRC" >&2
  exit 1
fi

install -m 0644 "$SRC" "$DST"
apparmor_parser -r -W "$DST"

echo "Installed and loaded AppArmor profile:"
echo "  $DST"
echo
echo "Verify with:  sudo aa-status | grep $PROFILE_NAME"
echo "Now re-run the app (e.g. 'cargo run'); bwrap should start cleanly."
