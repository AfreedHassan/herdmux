#!/usr/bin/env sh

set -eu

PLUGIN_DIR="${1:-$(cd "$(dirname "$0")/../../.." && pwd)}"
VERSION="${HERDMUX_RELEASE_VERSION:-$(grep -o '"version": *"[^"]*"' "$PLUGIN_DIR/package.json" 2>/dev/null | head -1 | cut -d'"' -f4)}"
RELEASE_BASE="${HERDMUX_RELEASE_BASE:-https://github.com/AfreedHassan/herdmux/releases/download}"
BIN_DIR="$PLUGIN_DIR/bin"

if [ "${HERDMUX_SKIP_BINARY_DOWNLOAD:-}" = "1" ]; then
  exit 0
fi

if [ -z "$VERSION" ]; then
  echo "herdmux: could not read package version from $PLUGIN_DIR/package.json" >&2
  exit 1
fi

target_triple() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os:$arch" in
    Darwin:arm64|Darwin:aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
    Darwin:x86_64|Darwin:amd64) printf '%s\n' "x86_64-apple-darwin" ;;
    Linux:x86_64|Linux:amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    *)
      echo "herdmux: unsupported platform for prebuilt binary: $os/$arch" >&2
      return 1
      ;;
  esac
}

download() {
  url="$1"
  dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    echo "herdmux: curl or wget is required to download prebuilt binaries" >&2
    return 1
  fi
}

TRIPLE="$(target_triple)"
ARTIFACT="herdmux-sidebar-${TRIPLE}.tar.gz"
URL="$RELEASE_BASE/v$VERSION/$ARTIFACT"
SIDEBAR_BIN="$BIN_DIR/herdmux-sidebar"
SERVER_BIN="$BIN_DIR/herdmux-server"
LAZYDIFF_BIN="$BIN_DIR/lazydiff"
VERSION_FILE="$BIN_DIR/.herdmux-version"

if [ -x "$SIDEBAR_BIN" ] && [ -x "$SERVER_BIN" ] && [ -x "$LAZYDIFF_BIN" ] && [ "$(cat "$VERSION_FILE" 2>/dev/null || true)" = "$VERSION" ]; then
  exit 0
fi

mkdir -p "$BIN_DIR"
TMP="$BIN_DIR/.herdmux-download.$$.$ARTIFACT"
cleanup() {
  rm -f "$TMP"
}
trap cleanup EXIT INT TERM

echo "herdmux: downloading prebuilt binaries for $TRIPLE (v$VERSION)" >&2
if ! download "$URL" "$TMP"; then
  echo "herdmux: failed to download $URL" >&2
  echo "herdmux: install from a released tag, or build locally with: cargo build --release" >&2
  exit 1
fi

tar -xzf "$TMP" -C "$BIN_DIR"
chmod +x "$SIDEBAR_BIN" "$SERVER_BIN" "$LAZYDIFF_BIN" 2>/dev/null || true
printf '%s\n' "$VERSION" >"$VERSION_FILE"
