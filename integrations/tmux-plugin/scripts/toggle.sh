#!/usr/bin/env sh
# Toggle the herdmux sidebar via the server.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

ensure_server || exit 0

CTX=$(tmux display-message -p '#{client_tty}|#{session_name}|#{window_id}|#{pane_id}|#{pane_active}' 2>/dev/null)
curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -X POST "http://${HOST}:${PORT}/toggle" -d "$CTX"
tmux switch-client -T root >/dev/null 2>&1
