#!/bin/sh
# Install smedja: smdjad, smj, smedja (GPU terminal), and smedja-tui (agent dashboard).
# Usage:
#   curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
#   SMEDJA_VERSION=v0.1.0 ... | sh   # pin a version
#   SMEDJA_DIR=/usr/local/bin ... | sh  # override install dir (default: ~/.local/bin)
set -e

REPO="mwigge/smedja"
INSTALL_DIR="${SMEDJA_DIR:-$HOME/.local/bin}"
VERSION="${SMEDJA_VERSION:-latest}"

# detect OS
OS=$(uname -s)
case "$OS" in
  Linux)  OS=linux ;;
  Darwin) OS=darwin ;;
  *) echo "error: unsupported OS: $OS" >&2; exit 1 ;;
esac

# detect arch
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)         ARCH=x86_64 ;;
  aarch64|arm64)  ARCH=aarch64 ;;
  *) echo "error: unsupported arch: $ARCH" >&2; exit 1 ;;
esac

# detect WSL2
IS_WSL=false
if [ "$OS" = "linux" ] && [ -f /proc/version ]; then
  case "$(cat /proc/version)" in
    *[Mm]icrosoft*) IS_WSL=true ;;
  esac
fi

TARBALL="smedja-$OS-$ARCH.tar.gz"

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/$TARBALL"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$TARBALL"
fi

echo "installing smedja $VERSION ($OS/$ARCH) → $INSTALL_DIR"

mkdir -p "$INSTALL_DIR"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" | tar -xz -C "$TMP"
elif command -v wget >/dev/null 2>&1; then
  wget -qO- "$URL" | tar -xz -C "$TMP"
else
  echo "error: curl or wget required" >&2; exit 1
fi

EXTRACT_DIR="$TMP/smedja-$OS-$ARCH"

for bin in smdjad smj smedja smedja-tui; do
  src="$EXTRACT_DIR/$bin"
  if [ -f "$src" ]; then
    install -m755 "$src" "$INSTALL_DIR/$bin"
    echo "  $bin → $INSTALL_DIR/$bin"
  fi
done

# ── macOS: remove Gatekeeper quarantine + register LaunchAgent ────────────────
if [ "$OS" = "darwin" ]; then
  # Remove com.apple.quarantine so macOS does not block the binaries.
  for bin in smdjad smj smedja smedja-tui; do
    if [ -f "$INSTALL_DIR/$bin" ]; then
      xattr -dr com.apple.quarantine "$INSTALL_DIR/$bin" 2>/dev/null || true
    fi
  done

  if [ -f "$INSTALL_DIR/smdjad" ]; then
    LAUNCHAGENT_DIR="$HOME/Library/LaunchAgents"
    LAUNCHAGENT_PLIST="$LAUNCHAGENT_DIR/nu.wigge.smedja.smdjad.plist"
    LOG_DIR="$HOME/Library/Logs/smedja"
    mkdir -p "$LAUNCHAGENT_DIR" "$LOG_DIR"
    cat > "$LAUNCHAGENT_PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>nu.wigge.smedja.smdjad</string>
  <key>ProgramArguments</key>
  <array>
    <string>$INSTALL_DIR/smdjad</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>$INSTALL_DIR:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>XDG_RUNTIME_DIR</key>
    <string>/tmp</string>
  </dict>
  <key>StandardOutPath</key>
  <string>$LOG_DIR/smdjad.log</string>
  <key>StandardErrorPath</key>
  <string>$LOG_DIR/smdjad-error.log</string>
  <key>WorkingDirectory</key>
  <string>$HOME</string>
</dict>
</plist>
PLIST_EOF
    # Bootstrap the service into the user session.
    # launchctl load works on macOS < 11; bootstrap on 11+.  Try both.
    launchctl load -w "$LAUNCHAGENT_PLIST" 2>/dev/null || \
      launchctl bootstrap "gui/$(id -u)" "$LAUNCHAGENT_PLIST" 2>/dev/null || true
    echo "  smdjad LaunchAgent → $LAUNCHAGENT_PLIST"
    echo "  logs → $LOG_DIR/"
  fi
fi

# ── Linux: install icon + .desktop + systemd user unit ────────────────────────
if [ "$OS" = "linux" ] && [ -f "$INSTALL_DIR/smedja" ]; then
  # Icon
  ICON_SRC="$EXTRACT_DIR/smedja-256.png"
  if [ -f "$ICON_SRC" ]; then
    ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"
    mkdir -p "$ICON_DIR"
    install -m644 "$ICON_SRC" "$ICON_DIR/smedja.png"
    gtk-update-icon-cache "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor" 2>/dev/null || true
  fi

  # .desktop entry
  DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  mkdir -p "$DESKTOP_DIR"
  DESKTOP_SRC="$EXTRACT_DIR/smedja.desktop"
  if [ -f "$DESKTOP_SRC" ]; then
    install -m644 "$DESKTOP_SRC" "$DESKTOP_DIR/smedja.desktop"
  else
    # Fallback: generate inline
    cat > "$DESKTOP_DIR/smedja.desktop" <<EOF
[Desktop Entry]
Name=smedja
Comment=GPU-accelerated terminal emulator and AI orchestration forge
Exec=$INSTALL_DIR/smedja
Icon=smedja
Type=Application
Categories=System;TerminalEmulator;
StartupNotify=true
EOF
  fi
  # Register as default terminal handler if update-alternatives is available
  if command -v update-alternatives >/dev/null 2>&1; then
    update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator "$INSTALL_DIR/smedja" 50 2>/dev/null || true
  fi
  echo "  smedja.desktop → $DESKTOP_DIR"

  # systemd user unit
  SERVICE_SRC="$EXTRACT_DIR/smdjad.service"
  if [ -f "$SERVICE_SRC" ]; then
    SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
    mkdir -p "$SYSTEMD_USER_DIR"
    install -m644 "$SERVICE_SRC" "$SYSTEMD_USER_DIR/smdjad.service"
    if command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload 2>/dev/null; then
      systemctl --user enable --now smdjad 2>/dev/null && \
        echo "  smdjad systemd unit → enabled" || \
        echo "  smdjad systemd unit installed (enable manually: systemctl --user enable --now smdjad)"
    else
      if [ "$IS_WSL" = "true" ]; then
        echo ""
        echo "note: systemd not detected in this WSL2 environment."
        echo "  To start smdjad automatically, add to ~/.bashrc or ~/.zshrc:"
        echo "    pgrep -u \"\$USER\" smdjad >/dev/null || smdjad &"
      else
        echo ""
        echo "note: systemd --user not available. Start smdjad manually or add to your shell RC:"
        echo "    pgrep -u \"\$USER\" smdjad >/dev/null || smdjad &"
      fi
    fi
  fi
fi

# ── PATH note ─────────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "note: $INSTALL_DIR is not in PATH. Add it:"
    echo ""
    case "${SHELL:-}" in
      */fish)  echo "  fish_add_path $INSTALL_DIR" ;;
      */zsh)   echo "  echo 'export PATH=\"\$PATH:$INSTALL_DIR\"' >> ~/.zshrc && source ~/.zshrc" ;;
      *)       echo "  echo 'export PATH=\"\$PATH:$INSTALL_DIR\"' >> ~/.bashrc && source ~/.bashrc" ;;
    esac
    ;;
esac

echo ""
echo "installed:"
echo "  smdjad      — AI orchestration daemon"
echo "  smj         — control CLI (smj --help)"
echo "  smedja      — GPU terminal emulator"
echo "  smedja-tui  — agent dashboard TUI (run inside smedja)"
echo ""
if [ "$OS" = "darwin" ]; then
  echo "quickstart: smedja  (smdjad starts automatically via LaunchAgent)"
elif [ "$IS_WSL" = "true" ]; then
  echo "quickstart: smdjad & && smedja"
  echo ""
  echo "note: smedja renders via WSLg. Ensure WSLg is enabled in your Windows setup."
else
  echo "quickstart: smdjad & && smedja"
fi
