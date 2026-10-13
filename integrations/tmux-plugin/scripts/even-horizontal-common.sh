#!/usr/bin/env sh

# Pure helpers for parsing pane row snapshots produced with:
#   #{pane_id}|#{pane_title}|#{pane_width}|#{pane_left}|#{pane_right}|#{pane_active}

count_sidebar_panes() {
  pane_rows="$1"
  title="${2:-herdmux-sidebar}"
  printf '%s\n' "$pane_rows" | awk -F '|' -v title="$title" '$2 == title { count++ } END { print count + 0 }'
}

count_non_sidebar_panes() {
  pane_rows="$1"
  title="${2:-herdmux-sidebar}"
  printf '%s\n' "$pane_rows" | awk -F '|' -v title="$title" '$2 != title { count++ } END { print count + 0 }'
}

extract_sidebar_info() {
  pane_rows="$1"
  title="${2:-herdmux-sidebar}"
  printf '%s\n' "$pane_rows" | awk -F '|' -v title="$title" '$2 == title { print $1 "|" $3 "|" $4 "|" $5 "|" $6; exit }'
}

detect_sidebar_side() {
  pane_rows="$1"
  sidebar_left="$2"
  sidebar_right="$3"
  title="${4:-herdmux-sidebar}"

  printf '%s\n' "$pane_rows" | awk -F '|' -v title="$title" -v sleft="$sidebar_left" -v sright="$sidebar_right" '
    $2 == title { next }
    {
      if (min_left == "" || $4 < min_left) min_left = $4;
      if (max_right == "" || $5 > max_right) max_right = $5;
    }
    END {
      if (min_left != "" && sright < min_left) print "left";
      else if (max_right != "" && sleft > max_right) print "right";
    }
  '
}
