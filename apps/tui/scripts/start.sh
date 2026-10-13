#!/usr/bin/env bash
# Start the herdmux TUI sidebar (Rust/Ratatui).

if [ -n "${TMUX:-}" ]; then
    HERDMUX_DIR="$(tmux show-environment -g HERDMUX_DIR 2>/dev/null | cut -d= -f2)"
fi
HERDMUX_DIR="${HERDMUX_DIR:-$(cd "$(dirname "$0")/../../.." && pwd)}"

RUST_BIN=""
if [ -x "$HERDMUX_DIR/bin/herdmux-sidebar" ]; then
    RUST_BIN="$HERDMUX_DIR/bin/herdmux-sidebar"
elif [ -x "$HERDMUX_DIR/target/release/herdmux-sidebar" ]; then
    RUST_BIN="$HERDMUX_DIR/target/release/herdmux-sidebar"
elif [ -x "$HERDMUX_DIR/target/debug/herdmux-sidebar" ]; then
    RUST_BIN="$HERDMUX_DIR/target/debug/herdmux-sidebar"
fi

if [ -z "$RUST_BIN" ]; then
    echo "herdmux: sidebar binary not found. Reinstall/update herdmux, or build locally with: cd $HERDMUX_DIR && cargo build --release -p herdmux-sidebar" >&2
    exit 1
fi

export REFOCUS_WINDOW
export HERDMUX_DIR

printf '\n--- launch %s pid=%s ---\n' "$(date +%s)" "$$" >>/tmp/herdmux-err.log
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
exec "$RUST_BIN" 2>>/tmp/herdmux-err.log
