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

# ── Validate caller-controlled inputs before they reach a URL or path ─────────
# SMEDJA_VERSION must be "latest" or a version like v0.12.0 / 0.12.0.
case "$VERSION" in
  latest) ;;
  *[!v0-9.]*)
    echo "error: invalid SMEDJA_VERSION '$VERSION' (expected 'latest' or a version like v0.12.0)" >&2
    exit 1 ;;
  *[0-9]*) ;;
  *)
    echo "error: invalid SMEDJA_VERSION '$VERSION' (expected 'latest' or a version like v0.12.0)" >&2
    exit 1 ;;
esac

# SMEDJA_DIR must be an absolute path.
case "$INSTALL_DIR" in
  /*) ;;
  *)
    echo "error: SMEDJA_DIR must be an absolute path (got '$INSTALL_DIR')" >&2
    exit 1 ;;
esac

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

# Download <url> to <dest-file> (no piping, so the artifact can be verified).
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "error: curl or wget required" >&2; exit 1
  fi
}

# Print the SHA-256 hex digest of <file>, using whichever tool is present.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "error: sha256sum or shasum -a 256 required for integrity verification" >&2
    exit 1
  fi
}

SUMS_URL="${URL%/"$TARBALL"}/SHA256SUMS"
fetch "$URL" "$TMP/$TARBALL"
fetch "$SUMS_URL" "$TMP/SHA256SUMS"

# Verify the tarball against the published manifest BEFORE extracting.
EXPECTED=$(awk -v f="$TARBALL" '$2 == f || $2 == "*"f {print $1}' "$TMP/SHA256SUMS" | head -1)
if [ -z "$EXPECTED" ]; then
  echo "error: $TARBALL not listed in SHA256SUMS — refusing to install" >&2; exit 1
fi
ACTUAL=$(sha256_of "$TMP/$TARBALL")
if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "error: checksum mismatch for $TARBALL" >&2
  echo "  expected: $EXPECTED" >&2
  echo "  actual:   $ACTUAL" >&2
  exit 1
fi

tar -xzf "$TMP/$TARBALL" -C "$TMP"

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

    # Escape XML metacharacters in interpolated values so a path containing
    # <, >, or & cannot inject markup into the generated plist.
    xml_escape() {
      printf '%s' "$1" | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g'
    }
    ESC_BIN=$(xml_escape "$INSTALL_DIR/smdjad")
    ESC_PATH=$(xml_escape "$INSTALL_DIR:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    ESC_OUT=$(xml_escape "$LOG_DIR/smdjad.log")
    ESC_ERR=$(xml_escape "$LOG_DIR/smdjad-error.log")
    ESC_HOME=$(xml_escape "$HOME")

    cat > "$LAUNCHAGENT_PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>nu.wigge.smedja.smdjad</string>
  <key>ProgramArguments</key>
  <array>
    <string>$ESC_BIN</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>$ESC_PATH</string>
    <key>XDG_RUNTIME_DIR</key>
    <string>/tmp</string>
  </dict>
  <key>StandardOutPath</key>
  <string>$ESC_OUT</string>
  <key>StandardErrorPath</key>
  <string>$ESC_ERR</string>
  <key>WorkingDirectory</key>
  <string>$ESC_HOME</string>
</dict>
</plist>
PLIST_EOF
    # Bootstrap the service into the user session.
    # launchctl load works on macOS < 11; bootstrap on 11+. Surface failure.
    if launchctl load -w "$LAUNCHAGENT_PLIST" 2>/dev/null \
       || launchctl bootstrap "gui/$(id -u)" "$LAUNCHAGENT_PLIST" 2>/dev/null; then
      echo "  smdjad LaunchAgent → $LAUNCHAGENT_PLIST (loaded)"
    else
      echo "  warning: could not register the smdjad LaunchAgent; load it manually:" >&2
      echo "    launchctl load -w \"$LAUNCHAGENT_PLIST\"" >&2
    fi
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

  # Pre-create dirs required by the service's ReadWritePaths= sandbox.
  # systemd bind-mounts these before exec, so they must exist at start time.
  mkdir -p "$HOME/.config/smedja" "$HOME/.local/share/smedja"

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
