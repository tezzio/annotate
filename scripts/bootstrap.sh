#!/usr/bin/env bash
# bootstrap.sh — install system dependencies and Rust toolchain.
#
# Usage:
#   ./scripts/bootstrap.sh [--with-aarch64]
#
#   --with-aarch64   Also install aarch64 cross toolchain + QEMU system
#                    emulator + RPi firmware tools. Without this flag
#                    the install is the minimal x86_64 host set.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

WITH_AARCH64=0
for arg in "$@"; do
    case "$arg" in
        --with-aarch64) WITH_AARCH64=1 ;;
        -h|--help)
            echo "Usage: $0 [--with-aarch64]"
            exit 0
            ;;
        *) die "Unknown argument: $arg" ;;
    esac
done

HOST_PKGS=(
    build-essential
    pkg-config
    libsdl2-dev
    libsdl2-ttf-dev
    libv4l-dev
    v4l-utils
    curl
    debootstrap
    rsync
    cpio
    gzip
    fakeroot
    xorriso
    grub-pc-bin
    grub-common
    qemu-system-x86
    qemu-utils
)

AARCH64_PKGS=(
    gcc-aarch64-linux-gnu
    libc6-dev-arm64-cross
    qemu-user-static
    qemu-system-aarch64
    dosfstools
    mtools
    libarchive-tools
)

if [ "$WITH_AARCH64" -eq 1 ]; then
    info "Installing host + aarch64 dependencies..."
    sudo apt install -y "${HOST_PKGS[@]}" "${AARCH64_PKGS[@]}"
else
    info "Installing host dependencies (use --with-aarch64 for aarch64/RPi5)..."
    sudo apt install -y "${HOST_PKGS[@]}"
fi

if ! command -v rustc &>/dev/null; then
    info "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
else
    info "Rust already installed: $(rustc --version)"
fi

info "Building annotator (host release)..."
cargo build --release

info "Bootstrap complete!"
info "Next steps:"
info "  make sysroot          # create $ARCH-sysroot"
info "  make build            # build annotator"
info "  make minimal-initrd   # build initrd (Xorg + annotator)"
info "  make run-minimal      # boot it in QEMU"
if [ "$WITH_AARCH64" -eq 1 ]; then
    info "  ARCH=aarch64 make sysroot   # aarch64 sysroot (cross, needs sudo once)"
    info "  ARCH=aarch64 make build     # cross-compile annotator"
fi
