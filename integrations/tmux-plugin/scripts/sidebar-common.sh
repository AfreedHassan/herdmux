#!/usr/bin/env sh

SIDEBAR_PANE_TITLE="herdmux-sidebar"
STASH_SESSION="_os_stash"
PANE_FIELD_SEP='|'

current_window_id() {
  tmux display-message -p '#{window_id}' 2>/dev/null || true
}

current_pane_id() {
  tmux display-message -p '#{pane_id}' 2>/dev/null || true
}

ensure_stash() {
  tmux has-session -t "$STASH_SESSION" 2>/dev/null && return 0
  tmux new-session -d -s "$STASH_SESSION" -x 80 -y 24 >/dev/null 2>&1
}

last_stash_window() {
  tmux list-windows -t "$STASH_SESSION" -F '#{window_id}' 2>/dev/null | awk 'NF { last = $0 } END { print last }'
}

stash_sidebar_pane() {
  pane_id="$1"
  ensure_stash || return 1

  target="$(last_stash_window)"
  target="${target:-${STASH_SESSION}:}"
  tmux resize-window -t "$target" -x 200 -y 200 >/dev/null 2>&1 || true

  if tmux join-pane -d -s "$pane_id" -t "$target" >/dev/null 2>&1; then
    return 0
  fi

  tmux new-window -d -t "${STASH_SESSION}:" >/dev/null 2>&1 || return 1
  target="$(last_stash_window)"
  target="${target:-${STASH_SESSION}:}"
  tmux resize-window -t "$target" -x 200 -y 200 >/dev/null 2>&1 || true
  tmux join-pane -d -s "$pane_id" -t "$target" >/dev/null 2>&1
}

window_edge_pane() {
  window_id="$1"
  side="$2"
  tmux list-panes -t "$window_id" -F "#{pane_id}${PANE_FIELD_SEP}#{pane_left}${PANE_FIELD_SEP}#{pane_right}" 2>/dev/null |
    awk -F "$PANE_FIELD_SEP" -v side="$side" '
      side == "left" {
        if (pane == "" || $2 < edge) { edge = $2; pane = $1 }
      }
      side == "right" {
        if (pane == "" || $3 > edge) { edge = $3; pane = $1 }
      }
      END { print pane }
    '
}

restore_sidebar_pane() {
  pane_id="$1"
  window_id="$2"
  side="$3"
  width="$4"

  target_pane_id="$(window_edge_pane "$window_id" "$side")"
  [ -n "$target_pane_id" ] || return 1

  join_flag="-h"
  if [ "$side" = "left" ]; then
    join_flag="-hb"
  fi

  tmux join-pane "$join_flag" -d -f -l "$width" -s "$pane_id" -t "$target_pane_id" >/dev/null 2>&1 || return 1

  attempt=0
  while [ "$attempt" -lt 5 ]; do
    tmux resize-pane -t "$pane_id" -x "$width" >/dev/null 2>&1 || true
    actual_width="$(tmux display-message -p -t "$pane_id" '#{pane_width}' 2>/dev/null || true)"
    if [ "$actual_width" = "$width" ]; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 0.02
  done

  return 0
}
