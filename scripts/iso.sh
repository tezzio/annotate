#!/usr/bin/env bash
# iso.sh — create a bootable ISO from the per-arch sysroot.
#
# Usage:
#   ./scripts/iso.sh                       # ISO for current \$ARCH (x86_64)
#   ARCH=aarch64 ./scripts/iso.sh          # aarch64 ISO (requires grub-efi)
#   ./scripts/iso.sh --output foo.iso
#
# Environment:
#   ARCH       x86_64 | aarch64
#   SYSROOT    default: target/$ARCH-sysroot
#   WORKDIR    default: target/$ARCH-iso-root
#   OUT        default: target/$ARCH-sysroot.iso

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

WORKDIR="${WORKDIR:-$TARGET_DIR/$ARCH-iso-root}"
OUT="${OUT:-$TARGET_DIR/$ARCH-sysroot.iso}"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --sysroot DIR     Sysroot directory (default: target/$ARCH-sysroot)
  --output FILE     Output ISO file (default: target/$ARCH-sysroot.iso)
  --workdir DIR     Working directory (default: target/$ARCH-iso-root)
  -h, --help        Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sysroot) SYSROOT="$2"; shift 2 ;;
        --output)  OUT="$2";     shift 2 ;;
        --workdir) WORKDIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

SYSROOT="$(abspath "$SYSROOT")"
WORKDIR="$(abspath "$WORKDIR")"
OUT="$(abspath "$OUT")"

[ -d "$SYSROOT" ] || die "sysroot not found: $SYSROOT (run ./scripts/sysroot.sh)"

# Per-arch kernel/initrd glob + GRUB binary name
case "$ARCH" in
    x86_64)
        KERNEL_GLOB="/boot/vmlinuz-*-amd64"
        INITRD_GLOB="/boot/initrd.img-*-amd64"
        GRUB_TARGET="x86_64-efi"
        require_cmd grub-mkrescue
        ;;
    aarch64)
        KERNEL_GLOB="/boot/vmlinuz-*-arm64"
        INITRD_GLOB="/boot/initrd.img-*-arm64"
        GRUB_TARGET="arm64-efi"
        # grub-mkrescue supports arm64-efi on Debian/Ubuntu
        require_cmd grub-mkrescue
        ;;
esac

require_cmd xorriso
require_cmd rsync

info "Building ISO:"
info "  SYSROOT=$SYSROOT"
info "  WORKDIR=$WORKDIR"
info "  OUTPUT=$OUT"

ensure_target_dir
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR/boot/grub"

# Copy the sysroot tree first, then overlay /boot from the sysroot on
# top so the kernel/initrd land in the ISO root's /boot.
rsync -a --exclude='/boot' --exclude='/proc' --exclude='/sys' \
          --exclude='/dev' --exclude='/run' --exclude='/tmp' \
          "$SYSROOT/" "$WORKDIR/"

if [ -d "$SYSROOT/boot" ]; then
    rsync -a "$SYSROOT/boot/" "$WORKDIR/boot/"
fi

# Find the actual kernel and initrd files in the workdir to embed in
# the GRUB config (avoid glob mismatches).
KERNEL_REL="$(cd "$WORKDIR" && ls $KERNEL_GLOB 2>/dev/null | head -n1)"
INITRD_REL="$(cd "$WORKDIR" && ls $INITRD_GLOB 2>/dev/null | head -n1)"

if [ -z "$KERNEL_REL" ]; then
    die "no kernel found in sysroot matching $KERNEL_GLOB"
fi

# Build a per-arch GRUB config
if [ "$ARCH" = "x86_64" ]; then
    cat > "$WORKDIR/boot/grub/grub.cfg" <<GRUBEOF
set default=0
set timeout=5

menuentry "Annotator" {
    linux   /$KERNEL_REL root=/dev/sda1 ro quiet loglevel=3 console=tty0 console=ttyS0,115200n8
    initrd  /$INITRD_REL
}
GRUBEOF
else
    cat > "$WORKDIR/boot/grub/grub.cfg" <<GRUBEOF
set default=0
set timeout=5

menuentry "Annotator" {
    linux   /$KERNEL_REL root=/dev/sda1 ro quiet loglevel=3 console=ttyAMA0,115200
    initrd  /$INITRD_REL
}
GRUBEOF
fi

info "Creating ISO with grub-mkrescue (target: $GRUB_TARGET)..."
if grub-mkrescue -o "$OUT" -- "$WORKDIR" 2>/dev/null; then
    :
else
    # Fallback: pure xorriso mkisofs emulation (no EFI). Useful when
    # grub-mkrescue EFI modules are not installed.
    warn "grub-mkrescue failed; falling back to xorriso BIOS image"
    xorriso -as mkisofs -R -J \
        -b boot/grub/bios.img -no-emul-boot \
        -boot-load-size 4 -boot-info-table \
        -o "$OUT" "$WORKDIR"
fi

info "ISO ready: $OUT"
