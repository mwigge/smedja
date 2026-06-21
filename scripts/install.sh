#!/bin/sh
# Install smdjad, smj, and smedja from GitHub Releases.
# Usage:
#   curl -fsSL https://github.com/mwigge/smedja/releases/latest/download/install.sh | sh
#   SMEDJA_VERSION=v0.1.0 ... | sh   # pin a version
#   SMEDJA_DIR=/usr/local/bin ... | sh  # override install dir
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

for bin in smdjad smj smedja; do
  src="$TMP/smedja-$OS-$ARCH/$bin"
  if [ -f "$src" ]; then
    install -m755 "$src" "$INSTALL_DIR/$bin"
    echo "  $bin → $INSTALL_DIR/$bin"
  fi
done

# PATH hint if needed
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "note: add $INSTALL_DIR to your PATH:"
    echo "  export PATH=\"\$PATH:$INSTALL_DIR\""
    ;;
esac

echo ""
echo "done. run 'smdjad --help' to get started."
