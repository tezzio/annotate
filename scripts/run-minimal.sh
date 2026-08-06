#!/usr/bin/env bash
# run-minimal.sh — boot the minimal initrd in QEMU.
#
# Usage:
#   ./scripts/run-minimal.sh                                # default arch
#   ARCH=aarch64 ./scripts/run-minimal.sh                   # QEMU raspi3b
#   ./scripts/run-minimal.sh --initrd foo.img --kernel g
#   ./scripts/run-minimal.sh --no-vnc --vnc 1 --kvm
#
# Environment:
#   ARCH     x86_64 | aarch64  (default: x86_64)
#   QEMU     override QEMU binary (default: qemu-system-<arch>)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

# Per-arch defaults
case "$ARCH" in
    x86_64)
        KERNEL_PATTERN="${KERNEL_PATTERN:-$SYSROOT/boot/vmlinuz-*-amd64}"
        INITRD="${INITRD:-$TARGET_DIR/x86_64-minimal-initrd.img}"
        QEMU_DEFAULT="qemu-system-x86_64"
        MACHINE=""  # default machine type
        APPEND="console=ttyS0,115200 console=tty0 init=/init"
        ;;
    aarch64)
        KERNEL_PATTERN="${KERNEL_PATTERN:-$SYSROOT/boot/vmlinuz-*-arm64}"
        INITRD="${INITRD:-$TARGET_DIR/aarch64-minimal-initrd.img}"
        QEMU_DEFAULT="qemu-system-aarch64"
        MACHINE="raspi3b"
        # For raspi3b the canonical console is ttyAMA0; tty1 is the
        # framebuffer console. QEMU also wants `vc: notty=1` on the
        # cmdline so it doesn't hang waiting for a display.
        APPEND="console=ttyAMA0,115200 console=tty1 init=/init"
        DTB_GLOB="$SYSROOT/usr/lib/linux-image-*-arm64/broadcom/bcm2710-rpi-3-b.dtb"
        ;;
esac

USB=1
VNC=1
VNC_DISPLAY=0
KVM=0
QEMU="${QEMU:-$QEMU_DEFAULT}"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --kernel GLOB     Kernel glob (default: \$SYSROOT/boot/vmlinuz-*-<arch>)
  --initrd FILE     Initrd file (default: target/<arch>-minimal-initrd.img)
  --dtb FILE        DTB file (aarch64 only, auto-discovered otherwise)
  --usb, --no-usb   Enable / disable USB tablet (x86_64 only)
  --no-vnc          Run headless, serial-only
  --vnc N           VNC display number (default: 0, i.e. port 5900)
  --kvm             Enable KVM acceleration (x86_64 only)
  -h, --help        Show this help

Examples:
  $0                       # VNC on :0 + USB tablet
  $0 --no-vnc --usb        # headless + USB tablet
  $0 --vnc 2               # VNC on port 5902
  ARCH=aarch64 $0          # boot the aarch64 initrd on QEMU raspi3b
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --kernel)    KERNEL_PATTERN="$2"; shift 2 ;;
        --initrd)    INITRD="$2"; shift 2 ;;
        --dtb)       DTB="$2"; shift 2 ;;
        --usb)       USB=1; shift ;;
        --no-usb)    USB=0; shift ;;
        --no-vnc)    VNC=0; shift ;;
        --vnc)       VNC_DISPLAY="$2"; shift 2 ;;
        --kvm)       KVM=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

# Resolve initrd to absolute
case "$INITRD" in
    /*) INITRD_ABS="$INITRD" ;;
    *)  INITRD_ABS="$PROJECT_DIR/$INITRD" ;;
esac
[ -f "$INITRD_ABS" ] || die "initrd not found: $INITRD_ABS (run ./scripts/initrd.sh)"

# Resolve kernel (last match wins so the most recent kernel is used)
KERNEL_GLOB="$KERNEL_PATTERN"
case "$KERNEL_GLOB" in
    /*) ;;
    *)  KERNEL_GLOB="$PROJECT_DIR/$KERNEL_GLOB" ;;
esac
KERNEL="$(ls -1 $KERNEL_GLOB 2>/dev/null | tail -n 1)"
[ -n "$KERNEL" ] && [ -f "$KERNEL" ] || die "kernel not found matching: $KERNEL_GLOB"

# Resolve DTB (aarch64 only)
DTB_ABS=""
if [ "$ARCH" = "aarch64" ]; then
    if [ -n "${DTB:-}" ]; then
        case "$DTB" in /*) DTB_ABS="$DTB" ;; *) DTB_ABS="$PROJECT_DIR/$DTB" ;; esac
    else
        # Auto-discover a Pi 3B DTB from the sysroot
        for cand in $DTB_GLOB; do
            [ -f "$cand" ] && DTB_ABS="$cand" && break
        done
    fi
    if [ -z "$DTB_ABS" ] || [ ! -f "$DTB_ABS" ]; then
        die "no DTB found (looked for $DTB_GLOB); pass --dtb explicitly"
    fi
fi

info "Booting with:"
info "  KERNEL=$KERNEL"
info "  INITRD=$INITRD_ABS"
[ -n "$DTB_ABS" ] && info "  DTB=$DTB_ABS"
info "  USB=$USB  VNC=$VNC  VNC_DISPLAY=$VNC_DISPLAY  KVM=$KVM  MACHINE=${MACHINE:-default}"

require_cmd "$QEMU"

# VNC port collision check
if [ "$VNC" -eq 1 ]; then
    VNC_PORT=$((5900 + VNC_DISPLAY))
    if ss -tln "( sport = :$VNC_PORT )" 2>/dev/null | grep -q ":$VNC_PORT"; then
        owner="$(ss -tlnp "( sport = :$VNC_PORT )" 2>/dev/null \
            | sed -n 's/.*users:((\"\([^\"]*\)\".*/\1/p' | head -n1)"
        die "port $VNC_PORT already in use by: ${owner:-unknown} (try --vnc $((VNC_DISPLAY+1)) or ./scripts/kill.sh)"
    fi
    info "Connect VNC viewer to: localhost:$VNC_PORT (display :$VNC_DISPLAY)"
fi

# Common args
QEMU_ARGS=(
    -kernel "$KERNEL"
    -initrd "$INITRD_ABS"
    -m 2048
    -append "$APPEND"
    -no-reboot
)
[ -n "$MACHINE" ] && QEMU_ARGS+=( -machine "$MACHINE" )
[ -n "$DTB_ABS" ] && QEMU_ARGS+=( -dtb "$DTB_ABS" )

case "$ARCH" in
    x86_64)
        QEMU_ARGS+=( -device qxl-vga,ram_size=64,vram_size_mb=128
                     -serial file:console.log )
        ;;
    aarch64)
        QEMU_ARGS+=( -serial null -display none )
        ;;
esac

# Display
if [ "$VNC" -eq 1 ]; then
    QEMU_ARGS+=( -vnc ":$VNC_DISPLAY" -monitor none )
else
    QEMU_ARGS+=( -display none -monitor none )
fi

# KVM (x86_64 only)
if [ "$KVM" -eq 1 ] && [ "$ARCH" = "x86_64" ]; then
    if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        QEMU_ARGS+=( -enable-kvm -cpu host )
    else
        warn "/dev/kvm not available; falling back to TCG"
    fi
fi

# USB tablet (x86_64 only — Pi 3B emu has its own USB)
if [ "$USB" -eq 1 ] && [ "$ARCH" = "x86_64" ]; then
    QEMU_ARGS+=( -usb -device usb-tablet )
fi

exec "$QEMU" "${QEMU_ARGS[@]}"
