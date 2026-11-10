#!/usr/bin/env bash
set -euo pipefail

APP_NAME="Workbench"
BUNDLE_ID="se.codebrain.workbench"
LEGACY_APP_NAME="Claude Workbench"
LEGACY_BUNDLE_ID="se.codebrain.claude-workbench"
BIN_NAME="rust-ui-test"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
LAUNCHER="$BIN_DIR/wbrs"

ORIG_ARGS=("$@")
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
# Prerequisites (git, curl, Rust/cargo, and on Linux a C toolchain + GUI libs)
# =============================================================================

RUSTUP_CMD="curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal"

have() { command -v "$1" >/dev/null 2>&1; }

# rustup installs into ~/.cargo/bin, which may not be on PATH in this shell yet.
use_cargo_env() {
  [[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
  [[ -d "$HOME/.cargo/bin" ]] && case ":$PATH:" in *":$HOME/.cargo/bin:"*) ;; *) PATH="$HOME/.cargo/bin:$PATH" ;; esac
  return 0
}

# Ask on the terminal. Fails without asking when there's no terminal (e.g. when
# the app's self-updater runs this script), so nothing gets installed unattended.
confirm() {
  [[ -t 0 ]] || return 1
  local ans
  read -r -p "$1 [y/N] " ans
  [[ "$ans" =~ ^[Yy] ]]
}

# sudo, or doas where that's what the system uses (e.g. Alpine); nothing as root.
sudo_prefix() {
  if [[ $EUID -eq 0 ]]; then echo ""
  elif have sudo; then echo "sudo "
  elif have doas; then echo "doas "
  else echo "sudo "
  fi
}

linux_pkg_manager() {
  if have pacman; then echo pacman
  elif have apt-get; then echo apt
  elif have dnf; then echo dnf
  elif have zypper; then echo zypper
  elif have apk; then echo apk
  elif have xbps-install; then echo xbps
  else return 1
  fi
}

# Maps a logical dependency to this distro's package name. openSUSE uses
# pkgconfig() capabilities so zypper picks whichever package ships that exact library.
linux_pkg() {
  local mgr="$1" dep="$2"
  case "$mgr:$dep" in
    *:git) echo git ;;
    *:curl) echo curl ;;
    *:make) echo make ;;
    pacman:cc) echo base-devel ;;
    pacman:pkgconfig) echo pkgconf ;;
    pacman:webkit) echo webkit2gtk-4.1 ;;
    pacman:gtk) echo gtk3 ;;
    pacman:soup) echo libsoup3 ;;
    pacman:xdo) echo xdotool ;;
    apt:cc) echo build-essential ;;
    apt:pkgconfig) echo pkg-config ;;
    apt:webkit) echo libwebkit2gtk-4.1-dev ;;
    apt:gtk) echo libgtk-3-dev ;;
    apt:soup) echo libsoup-3.0-dev ;;
    apt:xdo) echo libxdo-dev ;;
    dnf:cc) echo gcc ;;
    dnf:pkgconfig) echo pkgconf-pkg-config ;;
    dnf:webkit) echo webkit2gtk4.1-devel ;;
    dnf:gtk) echo gtk3-devel ;;
    dnf:soup) echo libsoup3-devel ;;
    dnf:xdo) echo libxdo-devel ;;
    zypper:cc) echo gcc ;;
    zypper:pkgconfig) echo pkg-config ;;
    zypper:webkit) echo "'pkgconfig(webkit2gtk-4.1)'" ;;
    zypper:gtk) echo "'pkgconfig(gtk+-3.0)'" ;;
    zypper:soup) echo "'pkgconfig(libsoup-3.0)'" ;;
    zypper:xdo) echo xdotool-devel ;;
    apk:cc) echo build-base ;;
    apk:pkgconfig) echo pkgconf ;;
    apk:webkit) echo webkit2gtk-4.1-dev ;;
    apk:gtk) echo gtk+3.0-dev ;;
    apk:soup) echo libsoup3-dev ;;
    apk:xdo) echo xdotool-dev ;;
    xbps:cc) echo base-devel ;;
    xbps:pkgconfig) echo pkg-config ;;
    xbps:webkit) echo libwebkit2gtk41-devel ;;
    xbps:gtk) echo gtk+3-devel ;;
    xbps:soup) echo libsoup3-devel ;;
    xbps:xdo) echo xdotool-devel ;;
  esac
}

linux_install_cmd() {
  local mgr="$1"; shift
  local s; s="$(sudo_prefix)"
  case "$mgr" in
    pacman) echo "${s}pacman -S --needed $*" ;;
    apt) echo "${s}apt-get update && ${s}apt-get install -y $*" ;;
    dnf) echo "${s}dnf install -y $*" ;;
    zypper) echo "${s}zypper --non-interactive install $*" ;;
    apk) echo "${s}apk add $*" ;;
    xbps) echo "${s}xbps-install -Sy $*" ;;
  esac
}

# Debian/Ubuntu releases before Debian 12 / Ubuntu 22.04 don't package WebKitGTK 4.1,
# which the app's webview needs. Only judged when apt has package lists to look in.
apt_check_webkit_available() {
  ls /var/lib/apt/lists/*Packages* >/dev/null 2>&1 || return 0
  apt-cache show libwebkit2gtk-4.1-dev >/dev/null 2>&1 && return 0
  local name="this release"
  [[ -r /etc/os-release ]] && name="$(. /etc/os-release && echo "${PRETTY_NAME:-this release}")"
  echo "error: $name has no libwebkit2gtk-4.1-dev package." >&2
  echo "  Workbench's window needs WebKitGTK 4.1, which ships with Ubuntu 22.04+ and Debian 12+." >&2
  echo "  Upgrade the system (or use a newer release) and rerun this script." >&2
  exit 1
}

ensure_prereqs() {
  use_cargo_env
  local missing=() cmds=() deps=() need_rust=0 clt=0

  have git || missing+=("git")
  have curl || missing+=("curl")
  have cargo || { missing+=("Rust (cargo)"); need_rust=1; }

  if [[ $PLATFORM == linux ]]; then
    have git || deps+=(git)
    have curl || deps+=(curl)
    have make || { missing+=("make"); deps+=(make); }
    have cc || { missing+=("C compiler"); deps+=(cc); }
    if ! have pkg-config; then
      # Can't check the GUI libraries without pkg-config, so install them all.
      missing+=("pkg-config" "GUI libraries (webkit2gtk-4.1, gtk3, libsoup3, libxdo)")
      deps+=(pkgconfig webkit gtk soup xdo)
    else
      pkg-config --exists webkit2gtk-4.1 || { missing+=("webkit2gtk-4.1"); deps+=(webkit); }
      pkg-config --exists gtk+-3.0       || { missing+=("gtk3"); deps+=(gtk); }
      pkg-config --exists libsoup-3.0    || { missing+=("libsoup3"); deps+=(soup); }
      # Debian/Ubuntu's libxdo-dev ships no .pc file, so also look for the library itself.
      have xdotool || pkg-config --exists libxdo 2>/dev/null \
        || { ldconfig -p 2>/dev/null || /sbin/ldconfig -p 2>/dev/null; } | grep -q 'libxdo\.so' \
        || { missing+=("libxdo"); deps+=(xdo); }
    fi
    if [[ ${#deps[@]} -gt 0 ]]; then
      local mgr
      if ! mgr="$(linux_pkg_manager)"; then
        echo "error: missing: ${missing[*]}" >&2
        echo "  No supported package manager found (pacman, apt, dnf, zypper, apk, xbps)." >&2
        echo "  Install these with your system's tools (the libraries as development packages), then rerun." >&2
        exit 1
      fi
      [[ $mgr == apt && " ${deps[*]} " == *" webkit "* ]] && apt_check_webkit_available
      local pkgs=() d
      for d in "${deps[@]}"; do pkgs+=("$(linux_pkg "$mgr" "$d")"); done
      cmds+=("$(linux_install_cmd "$mgr" "${pkgs[@]}")")
    fi
  else
    # macOS: git, make and the C toolchain come with the Xcode Command Line Tools
    # (/usr/bin/git exists as a stub even without them). curl ships with macOS.
    if ! xcode-select -p >/dev/null 2>&1; then
      missing+=("Xcode Command Line Tools (git, make, C compiler)")
      clt=1
    fi
  fi
  [[ $need_rust == 1 ]] && cmds+=("$RUSTUP_CMD")
  [[ $clt == 1 ]] && cmds+=("xcode-select --install")

  [[ ${#missing[@]} -eq 0 ]] && return 0

  echo "Workbench needs some tools that aren't installed: ${missing[*]}"
  echo "To install them, this script will run:"
  local c
  for c in ${cmds[@]+"${cmds[@]}"}; do echo "    $c"; done
  [[ $need_rust == 1 ]] && echo "  (rustup installs Rust into ~/.cargo and adds it to your shell profile)"
  if ! confirm "Install them now?"; then
    echo "Not installing. Run the commands above yourself, then rerun this script." >&2
    exit 1
  fi
  for c in ${cmds[@]+"${cmds[@]}"}; do
    echo "==> $c"
    bash -c "$c" || { echo "error: '$c' failed" >&2; exit 1; }
  done
  use_cargo_env

  if [[ $clt == 1 ]]; then
    echo "==> Finish the Command Line Tools install in the dialog that opened, then rerun this script."
    exit 1
  fi
  local still=()
  have git || still+=("git")
  have curl || still+=("curl")
  have cargo || still+=("cargo")
  if [[ ${#still[@]} -gt 0 ]]; then
    echo "error: still missing after install: ${still[*]}" >&2
    exit 1
  fi
  echo "==> Prerequisites installed"
}

# =============================================================================
# Git checkout (a ZIP download has no history, so the app can't check for updates)
# =============================================================================

REPO_URL="https://github.com/KarlOlofA/workbench.git"

# Identity for the one stash this may create, so it works before git is configured.
git_local() { git -c user.name=Workbench -c user.email=workbench@localhost "$@"; }

ensure_git_checkout() {
  [[ -e "$ROOT/.git" ]] && return 0
  echo "This copy of Workbench isn't a git checkout (it was probably downloaded as a ZIP),"
  echo "so the app can't tell which version it is and won't check GitHub for updates."
  if ! confirm "Link this folder to $REPO_URL (main) so updates work?"; then
    echo "    note: continuing without update checks"
    return 0
  fi
  local undo='rm -rf "$ROOT/.git"'
  git init -q && git symbolic-ref HEAD refs/heads/main && git remote add origin "$REPO_URL" \
    && git fetch -q origin main && git reset -q origin/main \
    && git branch -q --set-upstream-to=origin/main main \
    || { eval "$undo"; echo "    note: couldn't link to GitHub; continuing without update checks" >&2; return 0; }

  local changed
  changed="$(git status --porcelain | wc -l | tr -d ' ')"
  if [[ $changed -eq 0 ]]; then
    echo "==> Linked to $REPO_URL"
    return 0
  fi
  echo "$changed file(s) here differ from the latest main (an older download, or local edits)."
  if confirm "Replace them with the latest main? Your versions are kept in 'git stash'."; then
    git_local stash push -q -u -m "Files from the downloaded copy, before linking to GitHub"
    echo "==> Updated to the latest main ('git stash pop' brings your old files back)"
    # This script may itself have changed; continue with the new version.
    exec "$ROOT/scripts/install.sh" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"}
  fi
  eval "$undo"
  echo "    note: left the files as they are; continuing without update checks"
}

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

ensure_prereqs
ensure_git_checkout

echo "==> Building release binary ($PLATFORM)"
cargo build --release

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
BUILD="$(git rev-parse --short HEAD 2>/dev/null || echo dev)$(git diff --quiet 2>/dev/null || echo -dirty)"

"${PLATFORM}_install"

case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "    note: $BIN_DIR is not on your PATH";; esac

if [[ $OPEN_AFTER == 1 ]]; then
  "$LAUNCHER" "$ROOT"
fi
