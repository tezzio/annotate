#!/usr/bin/env bash
# run.sh — boot the annotator ISO in QEMU.
#
# Usage:
#   ./scripts/run.sh                       # target/$ARCH-sysroot.iso
#   ./scripts/run.sh /path/to/foo.iso
#   ./scripts/run.sh --no-usb              # PS/2 mouse only
#   ARCH=aarch64 ./scripts/run.sh          # qemu-system-aarch64 (AAVMF EFI)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

ISO="${1:-$TARGET_DIR/$ARCH-sysroot.iso}"
USB=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            cat <<EOF
Usage: $0 [iso] [--usb|--no-usb]

Arguments:
  iso              Path to ISO (default: target/$ARCH-sysroot.iso)
  --usb, --no-usb  Enable / disable USB tablet (default: enabled)
EOF
            exit 0
            ;;
        --usb)    USB=1; shift ;;
        --no-usb) USB=0; shift ;;
        --*)      shift ;;
        *)        ISO="$1"; shift ;;
    esac
done

case "$ISO" in
    /*) ISO_ABS="$ISO" ;;
    *)  ISO_ABS="$PROJECT_DIR/$ISO" ;;
esac

[ -f "$ISO_ABS" ] || die "ISO not found: $ISO_ABS (run ./scripts/iso.sh)"
info "Booting ISO: $ISO_ABS"

case "$ARCH" in
    x86_64)  QEMU=qemu-system-x86_64 ;;
    aarch64) QEMU=qemu-system-aarch64 ;;
esac
require_cmd "$QEMU"

QEMU_ARGS=(
    -cdrom "$ISO_ABS"
    -m 2048
    -boot d
    -display gtk
    -vga qxl
    -monitor stdio
)
if [ "$USB" -eq 1 ]; then
    # -usb instantiates a PIIX3 UHCI controller; required by usb-tablet.
    QEMU_ARGS+=( -usb -device usb-tablet )
fi

"$QEMU" "${QEMU_ARGS[@]}"
