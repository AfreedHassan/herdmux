#!/usr/bin/env sh
# herdmux uninstall — clean up all tmux hooks, keybindings, sidebar panes, and env vars
# Run this BEFORE removing the plugin files.
#
# Usage:
#   sh /path/to/herdmux/integrations/tmux-plugin/scripts/uninstall.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

echo "herdmux: uninstalling..."

# --- Remove global hooks ---
for hook in \
  client-session-changed \
  session-created \
  session-closed \
  client-resized \
  after-select-window \
  after-new-window \
  after-kill-pane \
  pane-exited \
  after-resize-pane; do
  tmux set-hook -gu "$hook" 2>/dev/null || true
done
echo "  ✓ removed global hooks"

# --- Kill sidebar panes ---
# Find all panes titled "herdmux-sidebar" and kill them
sidebar_panes=$(tmux list-panes -a -F '#{pane_id} #{pane_title}' 2>/dev/null | grep 'herdmux-sidebar' | awk '{print $1}') || true
if [ -n "$sidebar_panes" ]; then
  for pane in $sidebar_panes; do
    tmux kill-pane -t "$pane" 2>/dev/null || true
  done
  echo "  ✓ killed sidebar panes"
fi

# --- Kill stash session ---
tmux kill-session -t "_os_stash" 2>/dev/null || true
echo "  ✓ removed stash session"

# --- Kill the server ---
curl -s -o /dev/null -X POST "http://${HOST}:${PORT}/shutdown" 2>/dev/null || true
echo "  ✓ stopped server (if running)"

# --- Remove keybindings ---
# Command table bindings (herdmux key table)
PREFIX_KEY=$(tmux show-option -gqv "@herdmux-prefix-key" 2>/dev/null)
PREFIX_KEY="${PREFIX_KEY:-o}"
tmux unbind-key "$PREFIX_KEY" 2>/dev/null || true

# Unbind all keys in the herdmux command table
tmux unbind-key -T herdmux s 2>/dev/null || true
tmux unbind-key -T herdmux t 2>/dev/null || true
tmux unbind-key -T herdmux e 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key -T herdmux "$i" 2>/dev/null || true
done
tmux unbind-key -T herdmux Any 2>/dev/null || true

# Direct prefix bindings
tmux unbind-key C-s 2>/dev/null || true
tmux unbind-key C-t 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key "M-$i" 2>/dev/null || true
done

# Global keys (if configured)
FOCUS_GLOBAL_KEY=$(tmux show-option -gqv "@herdmux-focus-global-key" 2>/dev/null)
if [ -n "$FOCUS_GLOBAL_KEY" ]; then
  tmux unbind-key -n "$FOCUS_GLOBAL_KEY" 2>/dev/null || true
fi
INDEX_KEYS=$(tmux show-option -gqv "@herdmux-index-keys" 2>/dev/null)
for key in $INDEX_KEYS; do
  tmux unbind-key -n "$key" 2>/dev/null || true
done
echo "  ✓ removed keybindings"

# --- Remove environment variables ---
tmux set-environment -gu HERDMUX_DIR 2>/dev/null || true
tmux set-environment -gu HERDMUX_WIDTH 2>/dev/null || true
tmux set-environment -gu HERDMUX_HOST 2>/dev/null || true
tmux set-environment -gu HERDMUX_PORT 2>/dev/null || true
tmux set-environment -gu HERDMUX_PID_FILE 2>/dev/null || true
echo "  ✓ removed environment variables"

echo "herdmux: uninstall complete. You can now remove the plugin files."
echo "  If using TPM: remove the line from .tmux.conf and run prefix + alt + u"
