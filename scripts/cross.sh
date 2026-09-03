#!/usr/bin/env bash
# cross.sh — build annotator using cargo.
#
# Usage:
#   ./cross.sh                              # build x86_64 (host)
#   ARCH=aarch64 ./cross.sh                 # cross-build aarch64
#   ./cross.sh --target aarch64-unknown-linux-gnu
#
# Environment:
#   ARCH       x86_64 | aarch64  (default: x86_64)
#   TARGET     cargo target triple (default: $ARCH-unknown-linux-gnu)
#   SYSROOT    sysroot path       (default: target/$ARCH-sysroot)
#   JOBS       parallel jobs      (default: nproc)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

JOBS="$(nproc)"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --target TRIPLE   Cargo target triple (default: $ARCH-unknown-linux-gnu)
  -j, --jobs N      Parallel jobs (default: $JOBS)
  -h, --help        Show this help

Environment:
  ARCH=x86_64|aarch64
  SYSROOT=path/to/sysroot
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)   TARGET="$2"; shift 2 ;;
        -j|--jobs)  JOBS="$2"; shift 2 ;;
        -h|--help)  usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

ensure_target_dir
info "Building annotator for $TARGET (arch: $ARCH, multiarch: $MULTIARCH)"

# Export sysroot paths so the rustc-sdl2 build script can find
# headers and .so symlinks.
if [ -d "$SYSROOT" ]; then
    export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
    export PKG_CONFIG_PATH="$SYSROOT/usr/lib/$MULTIARCH/pkgconfig:$SYSROOT/usr/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export SDL2_LIB_DIR="$SYSROOT/usr/lib/$MULTIARCH"
    export SDL2_TTF_LIB_DIR="$SYSROOT/usr/lib/$MULTIARCH"
fi

# For aarch64, force the cross toolchain.
if [ "$ARCH" = "aarch64" ]; then
    require_cmd aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu="aarch64-linux-gnu-gcc"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="aarch64-linux-gnu-gcc"
    # Pass --sysroot to the linker so /lib/... paths resolve inside our sysroot.
    # Cargo passes link-args after the libraries, which is fine for --sysroot.
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-L$SYSROOT/usr/lib/$MULTIARCH -L$SYSROOT/lib/$MULTIARCH -C link-args=--sysroot=$SYSROOT"
fi

cargo build --release --target "$TARGET" -j "$JOBS"

ANNOTATOR_OUT="$TARGET_DIR/$TARGET/release/annotator"
if [ ! -f "$ANNOTATOR_OUT" ]; then
    die "Build failed: $ANNOTATOR_OUT not found"
fi

# Convenience copy at the top of target/
cp -f "$ANNOTATOR_OUT" "$TARGET_DIR/annotator"

info "Build complete:"
info "  $TARGET_DIR/annotator"
info "  $ANNOTATOR_OUT"
