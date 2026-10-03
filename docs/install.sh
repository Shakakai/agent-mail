#!/usr/bin/env bash
# agentmail installer
#
# Downloads the latest production build for your platform from GitHub
# Releases, verifies its SHA-256 checksum against the release's SHA256SUMS,
# and installs it to a directory on your PATH (default: ~/.local/bin).
#
# Usage:
#   curl -fsSL https://shakakai.github.io/agent-mail/install.sh | bash
#
# Environment overrides:
#   INSTALL_PREFIX   install directory (default: $HOME/.local/bin)
#   INSTALL_VERSION  exact release tag, e.g. v0.1.0-prod.3 (default: latest)
#
# The script never runs sudo: pick a writable INSTALL_PREFIX if the default
# doesn't suit you (e.g. INSTALL_PREFIX=/usr/local/bin sudo -E bash install.sh).

set -euo pipefail

REPO="Shakakai/agent-mail"
API="https://api.github.com/repos/${REPO}"
DEFAULT_PREFIX="${HOME}/.local/bin"
PREFIX="${INSTALL_PREFIX:-$DEFAULT_PREFIX}"

say()  { printf 'agentmail install: %s\n' "$*"; }
fail() { printf 'agentmail install: error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "required tool not found: $1 (install it and retry)"; }
need curl
need grep
if command -v sha256sum >/dev/null 2>&1; then
  SHA256() { sha256sum "$@"; }
  SHA256_CHECK() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
  SHA256() { shasum -a 256 "$@"; }
  SHA256_CHECK() { shasum -a 256 -c -; }
else
  fail "no SHA-256 tool found (need sha256sum or shasum)"
fi

# ---------------------------------------------------------------- platform
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux)  TARGET_OS="unknown-linux-gnu" ;;
  Darwin) TARGET_OS="apple-darwin" ;;
  *)      fail "unsupported OS '$OS' — grab a binary from https://github.com/${REPO}/releases
       or build from source: cargo build --release" ;;
esac
case "$ARCH" in
  x86_64|amd64)   TARGET_ARCH="x86_64" ;;
  arm64|aarch64)  TARGET_ARCH="aarch64" ;;
  *)              fail "unsupported architecture '$ARCH' — see https://github.com/${REPO}/releases" ;;
esac
TARGET="${TARGET_ARCH}-${TARGET_OS}"

# Not every target has CI binaries yet; fail with guidance rather than
# installing the wrong architecture.
case "$TARGET" in
  x86_64-unknown-linux-gnu|aarch64-apple-darwin) ;;
  *)
    fail "no prebuilt binary for '$TARGET' yet.
       Build from source (Rust toolchain): git clone https://github.com/${REPO} && cargo build --release
       or pick another artifact: https://github.com/${REPO}/releases" ;;
esac

# ---------------------------------------------------------------- release
if [ -n "${INSTALL_VERSION:-}" ]; then
  TAG="$INSTALL_VERSION"
  say "using pinned release ${TAG}"
else
  say "resolving latest production release"
  TAG="$(curl -fsSL "${API}/releases/latest" | grep -m1 '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  [ -n "$TAG" ] || fail "could not determine the latest release (GitHub API unreachable or rate-limited)"
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
cd "$TMPDIR"

say "release ${TAG}: looking for ${TARGET} artifact"
RELEASE_JSON="$(curl -fsSL "${API}/releases/tags/${TAG}" 2>/dev/null)" \
  || fail "release ${TAG} not found — check the tag: https://github.com/${REPO}/releases"
ASSET_URL="$(printf '%s' "$RELEASE_JSON" \
  | grep -o '"browser_download_url": *"[^"]*"' \
  | sed 's/.*: *"//; s/"$//' \
  | grep -- "-${TARGET}$" | head -n1)"
[ -n "$ASSET_URL" ] || fail "no ${TARGET} binary in release ${TAG} — see https://github.com/${REPO}/releases"

ASSET="${ASSET_URL##*/}"
curl -fsSLO "$ASSET_URL"
curl -fsSLO "${ASSET_URL%/*}/SHA256SUMS"

# ---------------------------------------------------------------- verify
say "verifying checksum"
grep -- " ${ASSET}$" SHA256SUMS | SHA256_CHECK >/dev/null \
  || fail "checksum mismatch for ${ASSET} — refusing to install"
say "checksum OK"

# ---------------------------------------------------------------- install
[ -w "$PREFIX" ] 2>/dev/null || mkdir -p "$PREFIX" 2>/dev/null \
  || fail "cannot write to ${PREFIX} — set INSTALL_PREFIX to a writable directory"
DEST="${PREFIX}/agentmail"
mv "$ASSET" "$DEST"
chmod 0755 "$DEST"

# Sanity: the installed binary must run and report its version.
VERSION="$("$DEST" --version 2>/dev/null | awk '{print $2}')" \
  || fail "installed binary does not execute — checksum passed but binary is broken?"
[ -n "$VERSION" ] || fail "installed binary did not report a version"

# ---------------------------------------------------------------- PATH
case ":${PATH}:" in
  *":${PREFIX}:"*) ;;
  *)
    say "${PREFIX} is not on your PATH in this shell"
    SHELL_NAME="$(basename "${SHELL:-}")"
    case "$SHELL_NAME" in
      zsh)  RC="${HOME}/.zshrc" ;;
      bash) RC="${HOME}/.bashrc" ;;
      *)    RC="${HOME}/.profile" ;;
    esac
    LINE="export PATH=\"${PREFIX}:\$PATH\""
    if [ -f "$RC" ] && ! grep -qsF "$LINE" "$RC"; then
      printf '\n# agent-mail\n%s\n' "$LINE" >> "$RC"
      say "added ${PREFIX} to PATH in ${RC} — restart your shell or: export PATH=\"${PREFIX}:\$PATH\""
    else
      say "add this to your shell profile: export PATH=\"${PREFIX}:\$PATH\""
    fi
    ;;
esac

say "installed agent-mail ${VERSION} to ${DEST}"
# ---- am alias (never clobber an existing tool) ----
if command -v am >/dev/null 2>&1; then
  say "alias skipped: 'am' is already provided by $(command -v am)"
elif [ -e "${PREFIX}/am" ] || [ -L "${PREFIX}/am" ]; then
  say "alias skipped: ${PREFIX}/am already exists"
else
  ln -s agentmail "${PREFIX}/am"
  say "installed alias: am -> agentmail"
fi

say "next: agentmail init --home ~/mail   # create your first node"
