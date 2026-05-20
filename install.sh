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
BIN=claude-code-linux-gui
HERE="$(cd "$(dirname "$0")" && pwd)"
ICON_SRC="$HERE/assets/hicolor/scalable/apps/$APPID.svg"
DESK_SRC="$HERE/assets/$APPID.desktop"
ICON_DST="$HOME/.local/share/icons/hicolor/scalable/apps"
DESK_DST="$HOME/.local/share/applications"
BIN_DST="$HOME/.local/bin"

# Build + install the release binary so you can launch from the app menu /
# `claude-code-linux-gui` instead of `cargo run`. Pass --no-build to skip
# compiling and just install an existing target/release binary.
if [ "${1:-}" != "--no-build" ]; then
  CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
  if [ ! -x "$CARGO" ]; then
    echo "cargo not found — install Rust or pass --no-build with a prebuilt binary." >&2
    exit 1
  fi
  echo "Building release (this can take a few minutes the first time)…"
  ( cd "$HERE" && "$CARGO" build --release )
fi
RELEASE_BIN="$HERE/target/release/$BIN"
if [ ! -x "$RELEASE_BIN" ]; then
  echo "No release binary at $RELEASE_BIN — run without --no-build first." >&2
  exit 1
fi

mkdir -p "$ICON_DST" "$DESK_DST" "$BIN_DST"

# Kill any running instance BEFORE replacing the binary, so /proc/<pid>/exe
# still resolves to a real path. If we kill after `install`, the kernel
# tags the still-mapped old binary as deleted and readlink returns
# ".../claude-code-linux-gui (deleted)" — the case below would not match
# and the old process would survive, silently re-activating itself as the
# single-instance GApplication on the next launch (you'd test stale code).
# Match by real executable path via /proc, never by command-line pattern
# (pkill -f would also match this script's own path). The "(deleted)" arm
# is a belt-and-suspenders fallback for binaries that were already
# replaced out of band.
KILLED=0
for pid in $(pgrep -x "$BIN" 2>/dev/null) $(pgrep -f "$BIN" 2>/dev/null); do
  exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null) || continue
  case "$exe" in
    */"$BIN" | */"$BIN"" (deleted)")
      kill "$pid" 2>/dev/null && KILLED=$((KILLED+1)) ;;
  esac
done
[ "$KILLED" -gt 0 ] && echo "Stopped $KILLED running instance(s) so the new build takes effect."

install -m 0755 "$RELEASE_BIN" "$BIN_DST/$BIN"
cp "$ICON_SRC" "$ICON_DST/"
cp "$DESK_SRC" "$DESK_DST/"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$DESK_DST" 2>/dev/null || true
fi

echo "Installed:"
echo "  $BIN_DST/$BIN"
echo "  $ICON_DST/$APPID.svg"
echo "  $DESK_DST/$APPID.desktop"
echo "Launch from the app menu, or run: $BIN"
echo "Log out/in (or restart GNOME Shell) so the dock picks up the icon."

# The .desktop has Exec=claude-code-linux-gui (bare name → PATH). On Ubuntu
# ~/.local/bin is on PATH only if it existed when ~/.profile ran; warn if not.
case ":$PATH:" in
  *":$BIN_DST:"*) ;;
  *)
    echo
    echo "NOTE: $BIN_DST is not on your PATH. Add it (then log out/in):"
    echo "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.profile"
    ;;
esac

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
