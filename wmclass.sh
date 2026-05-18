#!/usr/bin/env bash
# Self-contained: builds, launches the GUI in the background, finds its window
# by title (no clicking, no second terminal), prints WM_CLASS + GTK app id,
# then closes it. Run: bash wmclass.sh   — then paste ALL output.
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

echo "[1/4] building..."
cargo build -q 2>&1 | tail -3

BIN=target/debug/claude-code-linux-gui
echo "[2/4] launching app in background..."
"$BIN" >/tmp/ccgui.out 2>&1 &
APP_PID=$!

echo "[3/4] waiting for the GUI window (up to 30s)..."
FOUND=""
for i in $(seq 1 30); do
  sleep 1
  for id in $(xprop -root _NET_CLIENT_LIST 2>/dev/null | sed 's/.*# //; s/,//g'); do
    nm=$(xprop -id "$id" WM_NAME 2>/dev/null)
    case "$nm" in
      *Claude*Code*) FOUND="$id"; break;;
    esac
  done
  [ -n "$FOUND" ] && break
done

echo "[4/4] result:"
if [ -z "$FOUND" ]; then
  echo "  GUI window NOT found after 30s. App stdout/stderr:"
  tail -5 /tmp/ccgui.out
else
  echo "  window id = $FOUND"
  xprop -id "$FOUND" WM_NAME 2>/dev/null
  xprop -id "$FOUND" WM_CLASS 2>/dev/null
  xprop -id "$FOUND" _GTK_APPLICATION_ID 2>/dev/null
fi

kill "$APP_PID" 2>/dev/null
wait "$APP_PID" 2>/dev/null
echo "(app closed)"
