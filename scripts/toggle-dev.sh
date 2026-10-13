#!/usr/bin/env bash
# Toggle herdmux between dev (local workspace) and prod (TPM) in tmux.conf
# Usage: ./scripts/toggle-dev.sh [dev|prod]
#   No argument: toggle current mode
#   dev:  switch to local workspace
#   prod: switch to TPM
#
# Prerequisites:
#   Your tmux.conf must have both lines (one commented out):
#     set -g @plugin 'AfreedHassan/herdmux'
#     # run '<path-to-workspace>/herdmux.tmux'

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEV_LINE="run '${CURRENT_DIR}/herdmux.tmux'"
TPM_LINE="set -g @plugin 'AfreedHassan/herdmux'"

CONF="${XDG_CONFIG_HOME:-$HOME/.config}/tmux/tmux.conf"
[ ! -f "$CONF" ] && CONF="$HOME/.tmux.conf"

if [ ! -f "$CONF" ]; then
  echo "ERROR: tmux.conf not found" >&2
  exit 1
fi

# Detect current mode
if grep -q "^${TPM_LINE}" "$CONF"; then
  CURRENT="prod"
elif grep -q "^run '.*herdmux.tmux'" "$CONF"; then
  CURRENT="dev"
else
  echo "ERROR: couldn't detect herdmux config in $CONF" >&2
  exit 1
fi

# Determine target
TARGET="${1:-}"
if [ -z "$TARGET" ]; then
  [ "$CURRENT" = "prod" ] && TARGET="dev" || TARGET="prod"
fi

if [ "$TARGET" = "$CURRENT" ]; then
  echo "Already in $CURRENT mode"
  exit 0
fi

if [ "$TARGET" = "dev" ]; then
  sed -i '' "s|^${TPM_LINE}|# ${TPM_LINE}|" "$CONF"
  sed -i '' "s|^# *${DEV_LINE}|${DEV_LINE}|" "$CONF"
  echo "✓ Switched to DEV (${CURRENT_DIR})"
else
  sed -i '' "s|^run '.*herdmux.tmux'|# ${DEV_LINE}|" "$CONF"
  sed -i '' "s|^# *${TPM_LINE}|${TPM_LINE}|" "$CONF"
  echo "✓ Switched to PROD (TPM)"
fi

echo "  Reload with: tmux source-file $CONF"
