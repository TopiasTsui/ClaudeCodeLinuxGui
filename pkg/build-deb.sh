#!/usr/bin/env bash
# Build a .deb for the Claude Code Linux GUI with dpkg-deb.
#
# Layout it produces (system-wide, unlike install.sh which is per-user):
#   /usr/bin/claude-code-linux-gui
#   /usr/share/applications/dev.local.claude_code_linux_gui.desktop
#   /usr/share/icons/hicolor/scalable/apps/dev.local.claude_code_linux_gui.svg
#   /etc/apparmor.d/claude-code-linux-gui      (loaded by postinst on 23.10+)
#
# The maintainer scripts in pkg/deb/{postinst,prerm} already key off that
# exact AppArmor path, and the shipped profile already lists
# /usr/bin/claude-code-linux-gui — so a dpkg install needs no extra wiring.
#
# Usage:
#   pkg/build-deb.sh              # build release, then package
#   pkg/build-deb.sh --no-build   # package an existing target/release binary
#
# Output: dist/<pkg>_<version>_<arch>.deb
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"   # repo root (script lives in pkg/)
PKG=claude-code-linux-gui
APPID=dev.local.claude_code_linux_gui

# Version comes from Cargo.toml so the .deb never drifts from the crate.
VERSION="$(grep -m1 '^version' "$HERE/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
if [ -z "$VERSION" ]; then
  echo "Could not parse version from Cargo.toml" >&2
  exit 1
fi

# Debian arch label (amd64 / arm64 / …), not uname's x86_64.
ARCH="$(dpkg-architecture -qDEB_HOST_ARCH)"

if [ "${1:-}" != "--no-build" ]; then
  CARGO="$(command -v cargo || echo "$HOME/.cargo/bin/cargo")"
  if [ ! -x "$CARGO" ]; then
    echo "cargo not found — install Rust or pass --no-build with a prebuilt binary." >&2
    exit 1
  fi
  echo "Building release…"
  ( cd "$HERE" && "$CARGO" build --release )
fi

BIN="$HERE/target/release/$PKG"
if [ ! -x "$BIN" ]; then
  echo "No release binary at $BIN — run without --no-build first." >&2
  exit 1
fi

# Stage the package tree in a temp dir; clean it up on any exit.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp -d is 0700; the package root ('./') must be world-readable (0755)
# or the .deb ships a 'drwx------ ./' that lintian flags.
chmod 0755 "$STAGE"

install -Dm0755 "$BIN"                                   "$STAGE/usr/bin/$PKG"
install -Dm0644 "$HERE/assets/$APPID.desktop"            "$STAGE/usr/share/applications/$APPID.desktop"
install -Dm0644 "$HERE/assets/hicolor/scalable/apps/$APPID.svg" \
                "$STAGE/usr/share/icons/hicolor/scalable/apps/$APPID.svg"
install -Dm0644 "$HERE/assets/apparmor/$PKG"             "$STAGE/etc/apparmor.d/$PKG"

# Maintainer scripts must be executable; dpkg refuses them otherwise.
install -Dm0755 "$HERE/pkg/deb/postinst" "$STAGE/DEBIAN/postinst"
install -Dm0755 "$HERE/pkg/deb/prerm"    "$STAGE/DEBIAN/prerm"

# Installed-Size is in KiB, rounded up — what apt shows before install.
SIZE_KB=$(( ( $(du -sb "$STAGE" | cut -f1) + 1023 ) / 1024 ))

# Runtime deps: GTK4 + WebKitGTK 6.0 (the `webkit6` crate links libwebkitgtk-6.0).
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Section: devel
Priority: optional
Architecture: $ARCH
Maintainer: Topias <topiastsui@gmail.com>
Installed-Size: $SIZE_KB
Depends: libc6, libgtk-4-1, libwebkitgtk-6.0-4
Recommends: apparmor
Homepage: https://github.com/TopiasTsui/ClaudeCodeLinuxGui
Description: Native GTK4 Linux GUI for the official Claude Code CLI
 A native (GTK4, no Electron) Linux GUI for the official Claude Code CLI:
 persistent per-session process, streaming output, multi-session,
 per-session permission mode, full tools, Markdown rendering.
 Not affiliated with Anthropic.
EOF

mkdir -p "$HERE/dist"
OUT="$HERE/dist/${PKG}_${VERSION}_${ARCH}.deb"
# --root-owner-group: files owned by root:root without needing fakeroot/sudo.
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"

echo
echo "Built: $OUT"
echo "Install with:  sudo apt install $OUT"
echo "       (or:    sudo dpkg -i $OUT && sudo apt -f install)"
