#!/bin/sh
set -e

REPO="starc007/crev"
BIN="crev"
INSTALL_DIR="/usr/local/bin"

# ── detect OS and arch ────────────────────────────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "Unsupported architecture: $ARCH" && exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      x86_64)       TARGET="x86_64-apple-darwin" ;;
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      *) echo "Unsupported architecture: $ARCH" && exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS"
    exit 1
    ;;
esac

echo "Detected: $OS/$ARCH → $TARGET"

# ── determine install dir ─────────────────────────────────────────────────────
if [ ! -w "$INSTALL_DIR" ]; then
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
  echo "No write access to /usr/local/bin, installing to $INSTALL_DIR"
fi

# ── download ──────────────────────────────────────────────────────────────────
BASE_URL="https://github.com/${REPO}/releases/latest/download"
TARBALL="crev-${TARGET}.tar.gz"
CHECKSUM="crev-${TARGET}.tar.gz.sha256"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $TARBALL ..."
curl -fsSL "$BASE_URL/$TARBALL" -o "$TMP/$TARBALL"
curl -fsSL "$BASE_URL/$CHECKSUM" -o "$TMP/$CHECKSUM"

# ── verify checksum ───────────────────────────────────────────────────────────
echo "Verifying checksum ..."
cd "$TMP"
if command -v sha256sum > /dev/null 2>&1; then
  sha256sum -c "$CHECKSUM"
elif command -v shasum > /dev/null 2>&1; then
  shasum -a 256 -c "$CHECKSUM"
else
  echo "Warning: no sha256 tool found, skipping checksum verification"
fi
cd - > /dev/null

# ── extract and install ───────────────────────────────────────────────────────
echo "Installing to $INSTALL_DIR/$BIN ..."
tar -xzf "$TMP/$TARBALL" -C "$TMP"
chmod +x "$TMP/$BIN"
mv "$TMP/$BIN" "$INSTALL_DIR/$BIN"

# ── verify ────────────────────────────────────────────────────────────────────
if ! command -v crev > /dev/null 2>&1; then
  echo ""
  echo "crev installed to $INSTALL_DIR but it's not in your PATH."
  echo "Add this to your shell profile:"
  echo "  export PATH=\"\$PATH:$INSTALL_DIR\""
  exit 0
fi

echo ""
crev --version

# ── check Ollama ──────────────────────────────────────────────────────────────
if ! curl -s http://localhost:11434 > /dev/null 2>&1; then
  echo ""
  echo "Ollama is not running. To set up:"
  echo "  1. Install: curl -fsSL https://ollama.ai/install.sh | sh"
  echo "  2. Start:   ollama serve"
  echo "  3. Pull:    ollama pull qwen2.5-coder:7b"
fi

echo ""
echo "crev installed. Run 'crev init' in any git repo to get started."
