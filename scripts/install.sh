#!/usr/bin/env bash
set -euo pipefail

APP_NAME="Workbench"
BUNDLE_ID="se.codebrain.workbench"
LEGACY_APP_NAME="Claude Workbench"
LEGACY_BUNDLE_ID="se.codebrain.claude-workbench"
BIN_NAME="rust-ui-test"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
LAUNCHER="$BIN_DIR/wbrs"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OS="$(uname -s)"
case "$OS" in
  Darwin) PLATFORM=macos ;;
  Linux)  PLATFORM=linux ;;
  *) echo "error: unsupported OS: $OS" >&2; exit 1 ;;
esac

# --- macOS paths -------------------------------------------------------------
INSTALL_DIR="${INSTALL_DIR:-$HOME/Applications}"
APP_PATH="$INSTALL_DIR/$APP_NAME.app"
LEGACY_APP_PATH="$INSTALL_DIR/$LEGACY_APP_NAME.app"

# --- Linux paths -------------------------------------------------------------
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
LINUX_LIB_DIR="${LINUX_LIB_DIR:-$HOME/.local/lib/workbench}"
LINUX_BIN="$LINUX_LIB_DIR/$BIN_NAME"
DESKTOP_FILE="$DATA_HOME/applications/$BUNDLE_ID.desktop"

# =============================================================================
# macOS
# =============================================================================

macos_remove_legacy() {
  if [[ -d "$LEGACY_APP_PATH" ]]; then
    osascript -e "quit app id \"$LEGACY_BUNDLE_ID\"" >/dev/null 2>&1 || true
    rm -rf "$LEGACY_APP_PATH"
    echo "Removed legacy $LEGACY_APP_PATH"
  fi
}

macos_uninstall() {
  osascript -e "quit app id \"$BUNDLE_ID\"" >/dev/null 2>&1 || true
  rm -rf "$APP_PATH" "$LAUNCHER"
  echo "Removed $APP_PATH and $LAUNCHER"
  macos_remove_legacy
}

macos_install() {
  local stage
  stage="$(mktemp -d)/$APP_NAME.app"
  mkdir -p "$stage/Contents/MacOS"
  cp "target/release/$BIN_NAME" "$stage/Contents/MacOS/$BIN_NAME"
  cat > "$stage/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$BIN_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION+$BUILD</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  codesign --force --deep --sign - "$stage" >/dev/null 2>&1 || echo "warning: ad-hoc codesign failed"

  echo "==> Installing to $APP_PATH"
  osascript -e "quit app id \"$BUNDLE_ID\"" >/dev/null 2>&1 || true
  pkill -f "$APP_PATH/Contents/MacOS/$BIN_NAME" 2>/dev/null || true
  macos_remove_legacy
  mkdir -p "$INSTALL_DIR"
  rm -rf "$APP_PATH"
  mv "$stage" "$APP_PATH"

  mkdir -p "$BIN_DIR"
  cat > "$LAUNCHER" <<SH
#!/usr/bin/env bash
DIR="\$(cd "\${1:-.}" && pwd)" || exit 1
if [[ -n "\${2:-}" ]]; then
  FILE="\$(cd "\$(dirname "\$2")" && pwd)/\$(basename "\$2")"
  exec open -n -a "$APP_PATH" --args "\$DIR" "\$FILE"
fi
exec open -n -a "$APP_PATH" --args "\$DIR"
SH
  chmod +x "$LAUNCHER"

  echo "==> Installed $APP_NAME $VERSION+$BUILD"
  echo "    App:      $APP_PATH"
  echo "    Launcher: $LAUNCHER  (usage: wbrs [dir] [file])"
}

# =============================================================================
# Linux
# =============================================================================

linux_check_deps() {
  command -v pkg-config >/dev/null 2>&1 || {
    echo "error: pkg-config not found; install it first (e.g. 'sudo pacman -S pkgconf')" >&2
    exit 1
  }
  local missing=()
  pkg-config --exists webkit2gtk-4.1 || missing+=("webkit2gtk-4.1")
  pkg-config --exists gtk+-3.0       || missing+=("gtk3")
  pkg-config --exists libsoup-3.0    || missing+=("libsoup3")
  command -v xdotool >/dev/null 2>&1 || pkg-config --exists libxdo 2>/dev/null || missing+=("xdotool")
  [[ ${#missing[@]} -eq 0 ]] && return 0

  echo "error: missing system libraries: ${missing[*]}" >&2
  if command -v pacman >/dev/null 2>&1; then
    echo "  install with: sudo pacman -S --needed webkit2gtk-4.1 gtk3 libsoup3 xdotool" >&2
  elif command -v apt-get >/dev/null 2>&1; then
    echo "  install with: sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libxdo-dev" >&2
  elif command -v dnf >/dev/null 2>&1; then
    echo "  install with: sudo dnf install webkit2gtk4.1-devel gtk3-devel libsoup3-devel libxdo-devel" >&2
  fi
  exit 1
}

linux_uninstall() {
  pkill -f "$LINUX_BIN" 2>/dev/null || true
  rm -rf "$LINUX_LIB_DIR" "$LAUNCHER" "$DESKTOP_FILE"
  command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true
  echo "Removed $LINUX_LIB_DIR, $LAUNCHER and $DESKTOP_FILE"
}

linux_install() {
  echo "==> Installing to $LINUX_BIN"
  pkill -f "$LINUX_BIN" 2>/dev/null || true
  mkdir -p "$LINUX_LIB_DIR"
  # Copy to a temp name then rename, so a running binary isn't overwritten in place.
  cp "target/release/$BIN_NAME" "$LINUX_BIN.new"
  chmod +x "$LINUX_BIN.new"
  mv -f "$LINUX_BIN.new" "$LINUX_BIN"
  echo "$VERSION+$BUILD" > "$LINUX_LIB_DIR/VERSION"

  mkdir -p "$BIN_DIR"
  cat > "$LAUNCHER" <<SH
#!/usr/bin/env bash
DIR="\$(cd "\${1:-.}" && pwd)" || exit 1
ARGS=("\$DIR")
if [[ -n "\${2:-}" ]]; then
  ARGS+=("\$(cd "\$(dirname "\$2")" && pwd)/\$(basename "\$2")")
fi
# Detach from the terminal, like 'open -n' on macOS.
nohup "$LINUX_BIN" "\${ARGS[@]}" >/dev/null 2>&1 &
disown
SH
  chmod +x "$LAUNCHER"

  mkdir -p "$(dirname "$DESKTOP_FILE")"
  cat > "$DESKTOP_FILE" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$APP_NAME
Comment=Shape any codebase through an architecture graph, project rules and Claude Code
Exec=$LINUX_BIN
Path=$HOME
Icon=utilities-terminal
Terminal=false
Categories=Development;IDE;
StartupWMClass=$APP_NAME
DESKTOP
  command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true

  echo "==> Installed $APP_NAME $VERSION+$BUILD"
  echo "    Binary:   $LINUX_BIN"
  echo "    Desktop:  $DESKTOP_FILE"
  echo "    Launcher: $LAUNCHER  (usage: wbrs [dir] [file])"
}

# =============================================================================
# Main
# =============================================================================

if [[ "${1:-}" == "--uninstall" ]]; then
  "${PLATFORM}_uninstall"
  exit 0
fi

OPEN_AFTER=0
[[ "${1:-}" == "--open" ]] && OPEN_AFTER=1

[[ $PLATFORM == linux ]] && linux_check_deps

echo "==> Building release binary ($PLATFORM)"
cargo build --release

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
BUILD="$(git rev-parse --short HEAD 2>/dev/null || echo dev)$(git diff --quiet 2>/dev/null || echo -dirty)"

"${PLATFORM}_install"

case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "    note: $BIN_DIR is not on your PATH";; esac

if [[ $OPEN_AFTER == 1 ]]; then
  "$LAUNCHER" "$ROOT"
fi
