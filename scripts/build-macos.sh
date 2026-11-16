#!/usr/bin/env bash
# build-macos.sh — compile the annotator on macOS.
#
# The Linux build (scripts/build.sh) links against Debian library packages
# unpacked into a sysroot.  macOS has no such thing: the system libraries
# come from the operating system, and SDL2 and OpenSSL come from Homebrew.
# So this script is much simpler — it points the build at the Homebrew
# packages and lets cargo do the rest.
#
# Requirements:
#   * Xcode command line tools (clang, the macOS SDK)
#   * Homebrew with `sdl2-compat`, `sdl2_ttf` and `openssl@3`
#
# Usage:
#   ./scripts/build-macos.sh                 # release build
#   ./scripts/build-macos.sh --debug         # debug build
#   ./scripts/build-macos.sh --no-strip      # keep the debug symbols
#
# Environment:
#   ARCH     x86_64 | aarch64                (default: the host's)
#   TARGET   cargo target triple             (default: $ARCH-apple-darwin)
#   PREFIX   install prefix for the binary   (default: target/macos)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

PROFILE="release"
STRIP=1

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --debug       Build with the dev profile instead of release
  --no-strip    Do not strip the debug symbols from the binary
  -h, --help    Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)    PROFILE="debug"; shift ;;
        --no-strip) STRIP=0; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

# ── Host checks ──────────────────────────────────────────────────────────────

if [[ "$(uname -s)" != "Darwin" ]]; then
    die "this script builds for macOS — run it on a Mac"
fi

# The architecture follows the host unless it is overridden, so a
# universal build is a matter of building twice.
case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  TARGET=x86_64-apple-darwin ;;
    aarch64|arm64) ARCH=aarch64; TARGET=aarch64-apple-darwin ;;
    *) die "Unsupported ARCH '$ARCH' (use x86_64 or aarch64)" ;;
esac

# ── Dependencies ─────────────────────────────────────────────────────────────

if ! command -v pkg-config >/dev/null 2>&1; then
    die "pkg-config not found — install it with: brew install pkg-config"
fi

for pkg in sdl2 SDL2_ttf; do
    if ! pkg-config --exists "$pkg" 2>/dev/null; then
        case "$pkg" in
            sdl2)      die "SDL2 not found — install it with: brew install sdl2-compat" ;;
            SDL2_ttf)  die "SDL2_ttf not found — install it with: brew install sdl2_ttf" ;;
        esac
    fi
done

# The OpenSSL crate does not support the LibreSSL that macOS ships, so
# the Homebrew build of OpenSSL is used.  It has to be pointed at
# explicitly: the pkg-config files in the Homebrew keg are not on the
# default search path.
OPENSSL_PREFIX="$(brew --prefix openssl@3 2>/dev/null || brew --prefix openssl 2>/dev/null || true)"
if [[ -z "$OPENSSL_PREFIX" || ! -d "$OPENSSL_PREFIX/include" ]]; then
    die "OpenSSL not found — install it with: brew install openssl@3"
fi

export OPENSSL_LIB_DIR="$OPENSSL_PREFIX/lib"
export OPENSSL_INCLUDE_DIR="$OPENSSL_PREFIX/include"
export OPENSSL_NO_PKG_CONFIG=1

# The pkg-config files for SDL2 and SDL2_ttf live in the Homebrew kegs,
# which are not on the default search path either.
PKG_CONFIG_PATH="$OPENSSL_PREFIX/lib/pkgconfig:$(brew --prefix sdl2-compat 2>/dev/null || echo /nonexistent)/lib/pkgconfig:$(brew --prefix sdl2_ttf 2>/dev/null || echo /nonexistent)/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
export PKG_CONFIG_PATH

# ── Build ────────────────────────────────────────────────────────────────────

info "Building annotator for $TARGET ($PROFILE)..."
mkdir -p "$TARGET_DIR"

CARGO_ARGS=(--target "$TARGET")
if [[ "$PROFILE" == "release" ]]; then
    CARGO_ARGS+=(--release)
fi

cargo build "${CARGO_ARGS[@]}"

BIN="$TARGET_DIR/$TARGET/$PROFILE/annotator"
[[ -f "$BIN" ]] || die "build did not produce $BIN"

# ── Install ──────────────────────────────────────────────────────────────────

PREFIX="${PREFIX:-$TARGET_DIR/macos}"
DEST="$PREFIX/bin"
mkdir -p "$DEST"
cp -f "$BIN" "$DEST/annotator"

if [[ "$PROFILE" == "release" && "$STRIP" == "1" ]]; then
    # The binary carries a full symbol table, which makes it several
    # times larger.  The symbols are only useful for debugging, so they
    # are stripped from the released copy.
    strip -x "$DEST/annotator" 2>/dev/null || true
fi

info "Binary: $DEST/annotator"
if command -v file >/dev/null 2>&1; then
    file "$DEST/annotator" | sed 's/^/  /'
fi

# The camera has to be allowed for the application to see it — macOS
# asks the user the first time an application opens a camera, and the
# answer is remembered per bundle identifier.  A command-line binary has
# no bundle identifier, so the permission is granted to the terminal
# that started it.
info "Note: the first capture run will ask for camera access."
