#!/usr/bin/env bash
# common.sh — shared functions and variables for the annotate build system.
#
# Source from a build script with:
#   source "$SCRIPT_DIR/common.sh"
#
# Conventions
#   * Scripts run as the project owner by default. They do NOT
#     auto-re-exec under sudo. Operations that genuinely need root
#     (debootstrap, mkfs, losetup) live in scripts that explicitly
#     call `re_exec_sudo` and always chown back to the original user
#     on exit.
#   * PROJECT_DIR, TARGET_DIR, SYSROOT, ARCH, MULTIARCH are set here
#     and can be overridden from the environment before sourcing.
#
# Environment overrides (set before sourcing, or `export` then run):
#   ARCH=x86_64|aarch64
#   TARGET=x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu
#   TARGET_DIR=/opt/annotate/target
#   SYSROOT=/opt/annotate/target/<arch>-sysroot

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
: "${SYSROOT:=$TARGET_DIR/$ARCH-sysroot}"

case "$ARCH" in
    x86_64)  KERNEL_GLOB="/boot/vmlinuz-*-amd64" ;;
    aarch64) KERNEL_GLOB="/boot/vmlinuz-*-arm64" ;;
esac

# ── User environment ───────────────────────────────────────────────────────
# When run under sudo, fall back to the original user's HOME for cargo etc.
if [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
    ORIG_HOME="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
    ORIG_USER="$SUDO_USER"
else
    ORIG_HOME="${HOME:-}"
    ORIG_USER="${USER:-$(id -un)}"
fi
if [ -n "$ORIG_HOME" ] && [ -d "$ORIG_HOME/.cargo/bin" ]; then
    export PATH="$ORIG_HOME/.cargo/bin:$PATH"
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

# ── Privilege helpers ──────────────────────────────────────────────────────

# re_exec_sudo [args...] — re-exec the current script under sudo
# (preserving the small set of env vars that affect behaviour). The
# script is expected to call fix_target_ownership before exit so the
# next user-runs can write to TARGET_DIR.
re_exec_sudo() {
    if [ "$(id -u)" -ne 0 ]; then
        if command -v sudo >/dev/null 2>&1; then
            exec sudo --preserve-env=ARCH,TARGET,TARGET_DIR,SYSROOT,PATH,HOME,USER,SUITE,MIRROR,YES bash "$0" "$@"
        fi
        die "root access required (sudo not available)"
    fi
}

# ensure_target_dir — make sure $TARGET_DIR exists and is writable
# by the current user. If it's not writable (e.g. root-owned from a
# previous sudo run) try to chown it. Aborts with a clear message
# if the dir is fundamentally unwritable.
ensure_target_dir() {
    if [ ! -d "$TARGET_DIR" ]; then
        mkdir -p "$TARGET_DIR" || die "cannot create $TARGET_DIR"
    fi
    if [ ! -w "$TARGET_DIR" ]; then
        if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
            chown -R "$SUDO_USER":"$(id -g "$SUDO_USER")" "$TARGET_DIR" 2>/dev/null || true
        else
            die "target dir not writable: $TARGET_DIR (owned by $(stat -c '%U:%G' "$TARGET_DIR"))"
        fi
    fi
    [ -w "$TARGET_DIR" ] || die "target dir still not writable: $TARGET_DIR"
}

# fix_target_ownership — chown TARGET_DIR recursively back to the
# invoking user. Call this at the END of any script that re-exec'd
# under sudo, so subsequent user-runs can write without privileges.
fix_target_ownership() {
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] && [ -d "$TARGET_DIR" ]; then
        info "Restoring ownership of $TARGET_DIR to $SUDO_USER"
        chown -R "$SUDO_USER":"$(id -g "$SUDO_USER")" "$TARGET_DIR" 2>/dev/null || true
    fi
}

# ── Command helpers ────────────────────────────────────────────────────────

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        die "$1 is required. Install with: sudo apt install $1"
    fi
}
