#!/usr/bin/env bash
# common.sh — shared functions and variables for the annotate build system.
#
# Source from a build script with:
#   source "$SCRIPT_DIR/common.sh"
#
# Conventions
#   * Scripts run as the project owner.  Nothing in the build needs
#     root: the Debian packages are downloaded and unpacked as an
#     ordinary user.
#   * PROJECT_DIR, TARGET_DIR, SYSROOT, ARCH, MULTIARCH are set here
#     and can be overridden from the environment before sourcing.
#
# Environment overrides (set before sourcing, or `export` then run):
#   ARCH=x86_64|aarch64
#   TARGET=x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu
#   TARGET_DIR=/opt/annotate/target
#   SYSROOT=/opt/annotate/target/<arch>-libs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Architecture & multiarch ────────────────────────────────────────────────
: "${ARCH:=x86_64}"
case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  MULTIARCH=x86_64-linux-gnu  ;;
    aarch64|arm64) ARCH=aarch64; MULTIARCH=aarch64-linux-gnu ;;
    *) echo "ERROR: unsupported ARCH '$ARCH' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac
: "${TARGET:=$ARCH-unknown-linux-gnu}"

# ── Paths ──────────────────────────────────────────────────────────────────
: "${TARGET_DIR:=$PROJECT_DIR/target}"
case "$TARGET_DIR" in
    /*) TARGET_DIR="$TARGET_DIR" ;;
    *)  TARGET_DIR="$PROJECT_DIR/$TARGET_DIR" ;;
esac

# The directory holding the Debian library packages that the build links
# against.  It is produced by scripts/sysroot.sh.
: "${SYSROOT:=$TARGET_DIR/$ARCH-libs}"

# ── User environment ───────────────────────────────────────────────────────
# Make sure cargo is on PATH — rustup installs it under $HOME/.cargo/bin,
# which is not always in a non-login shell's PATH.
if [ -d "$HOME/.cargo/bin" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# ── Colours (TTY only) ─────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; NC=''
fi

info()  { echo -e "${GREEN}==>${NC} $*"; }
warn()  { echo -e "${YELLOW}WARN:${NC} $*"; }
error() { echo -e "${RED}ERROR:${NC} $*" >&2; }
die()   { error "$*"; exit 1; }

# ── Path helpers ───────────────────────────────────────────────────────────

# abspath <rel> — convert a project-relative path to absolute.
# Absolute paths pass through unchanged.
abspath() {
    case "$1" in
        /*) printf '%s' "$1" ;;
        "")  printf '%s' "$PROJECT_DIR" ;;
        *)   printf '%s' "$PROJECT_DIR/$1" ;;
    esac
}

# ── Directory helpers ──────────────────────────────────────────────────────

# ensure_target_dir — make sure $TARGET_DIR exists and is writable
# by the current user.  Aborts with a clear message if the directory
# is fundamentally unwritable.
#
# Nothing in the build needs root: the Debian packages are downloaded
# and unpacked as an ordinary user.
ensure_target_dir() {
    if [ ! -d "$TARGET_DIR" ]; then
        mkdir -p "$TARGET_DIR" || die "cannot create $TARGET_DIR"
    fi
    if [ ! -w "$TARGET_DIR" ]; then
        die "target dir not writable: $TARGET_DIR (owned by $(stat -c '%U:%G' "$TARGET_DIR"))"
    fi
}
