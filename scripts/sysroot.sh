#!/usr/bin/env bash
# sysroot.sh — create a reproducible Debian sysroot for cross-compilation.
#
# Usage:
#   ./scripts/sysroot.sh                  # x86_64 sysroot (host)
#   ARCH=aarch64 ./scripts/sysroot.sh     # aarch64 sysroot
#   ./scripts/sysroot.sh --refresh        # apt-get update inside existing sysroot
#   ./scripts/sysroot.sh --force          # wipe and recreate without prompting
#   YES=1 ./scripts/sysroot.sh            # same as --force
#
# Environment:
#   ARCH       x86_64 | aarch64  (default: x86_64)
#   SUITE      Debian release    (default: bookworm)
#   MIRROR     Debian mirror URL (default: http://deb.debian.org/debian)
#   SYSROOT    override sysroot path
#   YES        non-empty => recreate without prompting

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

SUITE="${SUITE:-bookworm}"
MIRROR="${MIRROR:-http://deb.debian.org/debian}"
FORCE=0
REFRESH=0

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --arch ARCH      Target architecture (x86_64 or aarch64)
                   Default: x86_64 (or \$ARCH from env)
  --suite SUITE    Debian suite (default: bookworm)
  --mirror URL     Debian mirror (default: http://deb.debian.org/debian)
  --target DIR     Sysroot subdir under target/ (default: <arch>-sysroot)
  --refresh        Re-run apt-get update inside an existing sysroot
  --force, -f      Wipe and recreate even if sysroot already exists
  -h, --help       Show this help

Examples:
  $0                       # x86_64 sysroot
  ARCH=aarch64 $0          # aarch64 sysroot
  $0 --force               # wipe and recreate x86_64 sysroot (no prompt)
  $0 --refresh             # apt update in existing sysroot
  YES=1 $0                 # same as --force
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)     ARCH="$2"; shift 2 ;;
        --suite)    SUITE="$2"; shift 2 ;;
        --mirror)   MIRROR="$2"; shift 2 ;;
        --target)   SYSROOT="$TARGET_DIR/$2"; shift 2 ;;
        --refresh)  REFRESH=1; shift ;;
        --force|-f) FORCE=1; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

case "$ARCH" in
    x86_64)
        DEBOOTSTRAP_ARCH=amd64; NEED_QEMU=0
        EXTRA_PKGS="libx11-dev libxrandr-dev libxinerama-dev libxcursor-dev libxi-dev libxss-dev libxrender-dev libxext-dev xinit xauth linux-image-amd64 initramfs-tools busybox-static xserver-xorg-core xserver-xorg-input-libinput xserver-xorg-input-evdev xserver-xorg-video-fbdev xserver-xorg-video-qxl xkb-data curl libdrm-dev libgbm-dev libsdl2-dev libsdl2-ttf-dev libv4l-dev"
        ;;
    aarch64)
        DEBOOTSTRAP_ARCH=arm64; NEED_QEMU=1
        EXTRA_PKGS="libx11-dev libxrandr-dev libxinerama-dev libxcursor-dev libxi-dev libxss-dev libxrender-dev libxext-dev xinit xauth linux-image-arm64 initramfs-tools busybox-static xserver-xorg-core xserver-xorg-input-libinput xserver-xorg-input-evdev xserver-xorg-video-fbdev xserver-xorg-video-vesa xkb-data curl libdrm-dev libgbm-dev libsdl2-dev libsdl2-ttf-dev libv4l-dev"
        ;;
    *) die "Unsupported arch: $ARCH (use x86_64 or aarch64)" ;;
esac

require_cmd debootstrap
if [ "$NEED_QEMU" -eq 1 ]; then
    require_cmd qemu-aarch64-static
fi

info "Sysroot target: $SYSROOT (arch: $ARCH, suite: $SUITE, mirror: $MIRROR)"

# debootstrap needs root — re-exec under sudo exactly once, preserving
# the env vars the rest of this script cares about. The rest of the
# script runs as root and chowns the result back to the user.
if [ "$(id -u)" -ne 0 ]; then
    exec sudo --preserve-env=ARCH,TARGET_DIR,SYSROOT,SUITE,MIRROR,YES bash "$0" "$@"
fi

# Refresh path: update package lists in an existing sysroot
if [ "$REFRESH" -eq 1 ]; then
    [ -d "$SYSROOT" ] || die "cannot --refresh: $SYSROOT does not exist"
    info "Refreshing package lists in $SYSROOT"
    chroot "$SYSROOT" /usr/bin/apt-get update
    fix_target_ownership
    info "Refresh complete"
    exit 0
fi

# Decide whether to recreate
if [ -d "$SYSROOT" ]; then
    if [ "$FORCE" -eq 0 ] && [ -z "${YES:-}" ]; then
        die "Sysroot already exists at $SYSROOT. Re-run with --force (or YES=1) to recreate, --refresh to update."
    fi
    info "Removing existing sysroot $SYSROOT"
    rm -rf "$SYSROOT"
fi

mkdir -p "$(dirname "$SYSROOT")"
# Make sure the parent is owned by the user so debootstrap output
# inherits sensible perms.
if [ -n "${SUDO_USER:-}" ]; then
    chown "$SUDO_USER":"$(id -g "$SUDO_USER")" "$(dirname "$SYSROOT")" 2>/dev/null || true
fi

info "Running debootstrap (this may take a few minutes)..."
debootstrap --arch="$DEBOOTSTRAP_ARCH" --include="$EXTRA_PKGS" "$SUITE" "$SYSROOT" "$MIRROR"

if [ "$NEED_QEMU" -eq 1 ]; then
    info "Copying qemu-aarch64-static into sysroot for chroot/apt..."
    mkdir -p "$SYSROOT/usr/bin"
    cp /usr/bin/qemu-aarch64-static "$SYSROOT/usr/bin/" 2>/dev/null || true
fi

# Hand the sysroot back to the original user.
fix_target_ownership

info "Sysroot ready: $SYSROOT"
