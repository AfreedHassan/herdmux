#!/usr/bin/env bash
# Toggle herdmux sidebar in Zellij via the server.
# Ensures the server is running first, then calls POST /toggle.
#
# Designed to be called from a zellij keybinding. Add to ~/.config/zellij/config.kdl:
#
#   bind "s" {
#     Run "bash" "${HERDMUX_DIR}/integrations/tmux-plugin/scripts/zellij-toggle.sh" {
#       close_on_exit true
#     };
#     SwitchToMode "Normal";
#   }

set -euo pipefail

PORT="${HERDMUX_PORT:-7391}"
HOST="${HERDMUX_HOST:-127.0.0.1}"

HERDMUX_DIR="${HERDMUX_DIR:-$(cd "$(dirname "$0")/../../.." && pwd)}"

RUST_SERVER_BIN=""
if [ -x "$HERDMUX_DIR/target/release/herdmux-server" ]; then
    RUST_SERVER_BIN="$HERDMUX_DIR/target/release/herdmux-server"
elif [ -x "$HERDMUX_DIR/target/debug/herdmux-server" ]; then
    RUST_SERVER_BIN="$HERDMUX_DIR/target/debug/herdmux-server"
fi

# --- Ensure server is running ---
server_alive() {
    curl -s -o /dev/null -m 0.2 "http://${HOST}:${PORT}/" 2>/dev/null
}

if ! server_alive; then
    if [ -z "$RUST_SERVER_BIN" ]; then
        echo "herdmux: server binary not found. Run: cd $HERDMUX_DIR && cargo build --release -p herdmux-server" >&2
        exit 1
    fi
    "$RUST_SERVER_BIN" &>/dev/null &
    disown
    for i in $(seq 1 30); do
        sleep 0.1
        server_alive && break
    done
fi

# --- Build context: |session|tabId ---
SESSION_NAME="${ZELLIJ_SESSION_NAME:-}"
# Get active tab ID from JSON (works from inside zellij)
TAB_ID="0"
TAB_JSON=$(zellij action list-tabs --json 2>/dev/null || echo "")
if [ -n "$TAB_JSON" ]; then
    TAB_ID=$(echo "$TAB_JSON" | python3 -c "
import json,sys
try:
    tabs=json.load(sys.stdin)
    active=[t for t in tabs if t.get('active')]
    print(active[0]['tab_id'] if active else tabs[0]['tab_id'] if tabs else '0')
except: print('0')" 2>/dev/null || echo "0")
fi

CTX="|${SESSION_NAME}|${TAB_ID}"
curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -X POST "http://${HOST}:${PORT}/toggle" -d "$CTX"
