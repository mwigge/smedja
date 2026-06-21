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

for bin in smdjad smj smedja smedja-tui; do
  src="$TMP/smedja-$OS-$ARCH/$bin"
  if [ -f "$src" ]; then
    install -m755 "$src" "$INSTALL_DIR/$bin"
    echo "  $bin → $INSTALL_DIR/$bin"
  fi
done

# register smedja as a .desktop app (Linux only)
if [ "$OS" = "linux" ] && [ -f "$INSTALL_DIR/smedja" ]; then
  DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  mkdir -p "$DESKTOP_DIR"
  cat > "$DESKTOP_DIR/smedja.desktop" <<EOF
[Desktop Entry]
Name=smedja
Comment=smedja GPU terminal
Exec=$INSTALL_DIR/smedja
Icon=utilities-terminal
Type=Application
Categories=System;TerminalEmulator;
StartupNotify=true
EOF
  # register as default terminal handler if update-alternatives is available
  if command -v update-alternatives >/dev/null 2>&1; then
    update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator "$INSTALL_DIR/smedja" 50 2>/dev/null || true
  fi
  echo "  smedja.desktop → $DESKTOP_DIR"
fi

# add to PATH if not already there
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "note: $INSTALL_DIR is not in PATH. Add it:"
    echo ""
    # detect shell and suggest the right rc file
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
echo "quickstart: smdjad & && smedja"
