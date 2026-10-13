#!/usr/bin/env sh

server_key() {
  if [ -n "$HERDMUX_SERVER_KEY" ]; then
    printf '%s\n' "$HERDMUX_SERVER_KEY"
    return
  fi

  if [ -z "$TMUX" ]; then
    return
  fi

  socket_path="${TMUX%%,*}"
  [ -n "$socket_path" ] || return

  awk -v input="$socket_path" 'BEGIN {
    hash = 0
    for (i = 1; i <= length(input); i++) {
      hash = (hash + ord(substr(input, i, 1)) * i) % 20000
    }
    printf "%d\n", hash
  }
  function ord(ch,    chars) {
    chars = " !\"#$%&\047()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~"
    return index(chars, ch) + 31
  }'
}

SERVER_KEY="$(server_key)"
PORT_BASE=22000
TMUX_HERDMUX_PORT="$(tmux show-environment -g HERDMUX_PORT 2>/dev/null | cut -d= -f2)"
TMUX_HERDMUX_HOST="$(tmux show-environment -g HERDMUX_HOST 2>/dev/null | cut -d= -f2)"
TMUX_HERDMUX_PID_FILE="$(tmux show-environment -g HERDMUX_PID_FILE 2>/dev/null | cut -d= -f2)"

if [ -n "$TMUX_HERDMUX_PORT" ]; then
  PORT="$TMUX_HERDMUX_PORT"
elif [ -n "$SERVER_KEY" ]; then
  PORT=$((PORT_BASE + SERVER_KEY))
else
  PORT="7391"
fi
HOST="${TMUX_HERDMUX_HOST:-127.0.0.1}"
if [ -n "$TMUX_HERDMUX_PID_FILE" ]; then
  PID_FILE="$TMUX_HERDMUX_PID_FILE"
elif [ -n "$SERVER_KEY" ]; then
  PID_FILE="/tmp/herdmux.${SERVER_KEY}.pid"
else
  PID_FILE="/tmp/herdmux.pid"
fi

PLUGIN_DIR="$(tmux show-environment -g HERDMUX_DIR 2>/dev/null | cut -d= -f2)"
PLUGIN_DIR="${PLUGIN_DIR:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
SERVER_LOG="/tmp/herdmux.${SERVER_KEY:-default}.server.log"
START_LOCK_DIR="/tmp/herdmux.${SERVER_KEY:-default}.start.lock"

RUST_SERVER_BIN=""
if [ -x "$PLUGIN_DIR/bin/herdmux-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/bin/herdmux-server"
elif [ -x "$PLUGIN_DIR/target/release/herdmux-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/target/release/herdmux-server"
elif [ -x "$PLUGIN_DIR/target/debug/herdmux-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/target/debug/herdmux-server"
fi

show_startup_error() {
  message="$1"
  tmux display-message "$message" >/dev/null 2>&1 || true
  printf '%s\n' "$message" >&2
}

server_alive() {
  curl -s -o /dev/null -m 0.2 "http://${HOST}:${PORT}/" 2>/dev/null
}

acquire_start_lock() {
  attempt=0
  while ! mkdir "$START_LOCK_DIR" 2>/dev/null; do
    if server_alive; then
      return 2
    fi

    lock_pid=""
    if [ -f "$START_LOCK_DIR/pid" ]; then
      lock_pid="$(cat "$START_LOCK_DIR/pid" 2>/dev/null)"
    fi
    if [ -n "$lock_pid" ] && ! kill -0 "$lock_pid" 2>/dev/null; then
      rm -rf "$START_LOCK_DIR"
      continue
    fi

    if [ "$attempt" -ge 50 ]; then
      show_startup_error "herdmux: server start lock timed out. Remove $START_LOCK_DIR if no launcher is active."
      return 1
    fi
    sleep 0.1
    attempt=$((attempt + 1))
  done

  printf '%s\n' "$$" >"$START_LOCK_DIR/pid" 2>/dev/null || true
  return 0
}

release_start_lock() {
  rm -rf "$START_LOCK_DIR"
}

ensure_server() {
  unset HERDMUX_WIDTH

  if server_alive; then
    return 0
  fi

  acquire_start_lock
  lock_status=$?
  if [ "$lock_status" -eq 2 ]; then
    return 0
  fi
  if [ "$lock_status" -ne 0 ]; then
    return 1
  fi

  if server_alive; then
    release_start_lock
    return 0
  fi

  if [ -z "$RUST_SERVER_BIN" ]; then
    show_startup_error "herdmux: server binary not found. Reinstall/update herdmux, or build locally with: cd $PLUGIN_DIR && cargo build --release -p herdmux-server"
    release_start_lock
    return 1
  fi

  HERDMUX_RUST=1 \
  HERDMUX_SERVER_KEY="$SERVER_KEY" \
  HERDMUX_HOST="$HOST" \
  HERDMUX_PORT="$PORT" \
  HERDMUX_PID_FILE="$PID_FILE" \
  HERDMUX_DIR="$PLUGIN_DIR" \
    "$RUST_SERVER_BIN" >"$SERVER_LOG" 2>&1 &

  attempt=0
  while [ "$attempt" -lt 30 ]; do
    sleep 0.1
    if server_alive; then
      release_start_lock
      return 0
    fi
    attempt=$((attempt + 1))
  done

  # A concurrent launcher may have won the race between our final poll and the
  # child bind attempt. Treat a healthy endpoint as success; never surface an
  # "address already in use" startup failure when the server is actually up.
  if server_alive; then
    release_start_lock
    return 0
  fi

  show_startup_error "herdmux: server failed to start. See $SERVER_LOG"
  release_start_lock
  return 1
}
