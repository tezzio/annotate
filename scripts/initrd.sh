#!/usr/bin/env bash
# initrd.sh — build a minimal initrd (Xorg + annotator) from the
# per-arch sysroot. The result is the small initrd booted by
# scripts/run-minimal.sh.
#
# Usage:
#   ./scripts/initrd.sh
#   ./scripts/initrd.sh --output file --workdir dir --sysroot dir
#   ARCH=aarch64 ./scripts/initrd.sh
#
# Environment:
#   ARCH            x86_64 | aarch64 (default: x86_64)
#   SYSROOT         Path to sysroot (default: target/$ARCH-sysroot)
#   OUT             Output initrd file (default: target/$ARCH-minimal-initrd.img)
#   WORKDIR         Build directory  (default: target/$ARCH-minimal-initrd-root)
#   ANNOTATOR_BIN   Path to annotator binary
#                   (default: target/annotator, then <sysroot>/usr/bin/annotator)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

SYSROOT="${SYSROOT:-$TARGET_DIR/$ARCH-sysroot}"
OUT="${OUT:-$TARGET_DIR/$ARCH-minimal-initrd.img}"
WORKDIR="${WORKDIR:-$TARGET_DIR/$ARCH-minimal-initrd-root}"
ANNOTATOR_BIN="${ANNOTATOR_BIN:-$TARGET_DIR/annotator}"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --sysroot DIR     Sysroot directory (default: target/$ARCH-sysroot)
  --output FILE     Output initrd file (default: target/$ARCH-minimal-initrd.img)
  --workdir DIR     Working directory (default: target/$ARCH-minimal-initrd-root)
  -h, --help        Show this help

Environment:
  ARCH=x86_64|aarch64
  SYSROOT=DIR         Override sysroot path
  OUT=FILE            Override output file
  WORKDIR=DIR         Override workdir
  ANNOTATOR_BIN=FILE  Path to annotator binary
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --sysroot) SYSROOT="$2"; shift 2 ;;
        --output)  OUT="$2"; shift 2 ;;
        --workdir) WORKDIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

# Resolve to absolute paths (use the common.sh abspath helper).
SYSROOT_ABS="$(abspath "$SYSROOT")"
WORKDIR_ABS="$(abspath "$WORKDIR")"
OUT_ABS="$(abspath "$OUT")"
ANNOTATOR_ABS="$(abspath "$ANNOTATOR_BIN")"

# We deliberately do NOT re-exec under sudo here. Every file we
# produce in the workdir is owned by the current user; only the
# final cpio+gzip step needs elevated privileges, and we handle
# that with `fakeroot` when available (preferred) or by requiring
# the user to invoke us under sudo explicitly.

if [ ! -d "$SYSROOT_ABS" ]; then
    error "Sysroot not found at $SYSROOT_ABS"
    error "Run: ./scripts/sysroot.sh"
    exit 1
fi

for cmd in readelf cpio gzip cp rsync mkdir find ln; do
    require_cmd "$cmd"
done

info "Building minimal initrd:"
info "  SYSROOT=$SYSROOT_ABS"
info "  WORKDIR=$WORKDIR_ABS"
info "  OUTPUT=$OUT_ABS"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

declare -A COPIED=()

# resolve_in_sysroot <relpath> — follow symlinks inside the sysroot and
# return the absolute path to the real file.
resolve_in_sysroot() {
    local path="$SYSROOT_ABS/${1#/}"
    local link
    while [ -h "$path" ]; do
        link="$(readlink "$path")"
        if [[ "$link" == /* ]]; then
            path="$SYSROOT_ABS$link"
        else
            path="$(dirname "$path")/$link"
        fi
    done
    printf '%s' "$path"
}

# copy_to_initrd <relpath> — copy a sysroot path (preserving the
# versioned symlink structure) into the initrd workdir. Recurses for
# symlinks so that SONAME-style library links resolve correctly.
copy_to_initrd() {
    local rel="${1#/}"
    rel="/$rel"
    if [ -n "${COPIED[$rel]:-}" ]; then
        return 0
    fi
    local src="$SYSROOT_ABS$rel"
    local resolved
    resolved="$(resolve_in_sysroot "$rel")"
    if [ ! -f "$resolved" ] && [ ! -h "$resolved" ]; then
        return 1
    fi
    local dst="$WORKDIR_ABS$rel"
    mkdir -p "$(dirname "$dst")"

    # If the source is a symlink, copy the real file AND recreate the
    # versioned symlink in the initrd so the dynamic linker can find the
    # library by its SONAME (e.g. libdbus-1.so.3).
    if [ -h "$src" ]; then
        local real_rel="${resolved#$SYSROOT_ABS}"
        copy_to_initrd "$real_rel"
        local target
        target="$(readlink "$src")"
        ln -sfn "$target" "$dst"
        COPIED[$rel]=1
        return 0
    fi

    cp -a "$resolved" "$dst"
    COPIED[$rel]=1
}

# find_library_in_sysroot <soname> — locate a library by name inside
# the sysroot, returning the path that preserves any symlink so the
# SONAME is available at runtime. Honors $MULTIARCH.
find_library_in_sysroot() {
    local name="$1"
    local libpath dir
    for dir in \
        /lib/$MULTIARCH \
        /usr/lib/$MULTIARCH \
        /lib \
        /usr/lib \
        /lib64 \
        /usr/lib64; do
        libpath="$SYSROOT_ABS$dir/$name"
        if [ -f "$(resolve_in_sysroot "$dir/$name")" ]; then
            printf '%s' "$libpath"
            return 0
        fi
    done
    find "$SYSROOT_ABS" -name "$name" 2>/dev/null | head -n 1
}

# extract_needed <file> — print the NEEDED library SONAMEs and the
# program interpreter path of an ELF file, one per line.
extract_needed() {
    local file="$1"
    if [ ! -f "$file" ]; then
        return 0
    fi
    readelf -d "$file" 2>/dev/null \
        | awk '/Requesting program interpreter/ {gsub(/\[|\]/, "", $NF); print $NF}
               /NEEDED/ {gsub(/\[|\]/, "", $NF); print $NF}' || true
}

# copy_deps_for <file> — recursively copy the dynamic dependencies of
# an ELF binary or shared library into the initrd. The argument may be
# a path inside the sysroot or inside the initrd workdir.
copy_deps_for() {
    local file="$1"
    if [ ! -f "$file" ]; then
        return 0
    fi
    local needed
    needed="$(extract_needed "$file")"
    for lib in $needed; do
        local libpath
        if [[ "$lib" == /* ]]; then
            # Absolute path (e.g. interpreter) — resolve inside sysroot
            # so versioned symlinks are followed correctly.
            local abs_resolved
            abs_resolved="$(resolve_in_sysroot "$lib")"
            if [ -f "$abs_resolved" ] || [ -h "$abs_resolved" ]; then
                libpath="$abs_resolved"
            else
                libpath="$SYSROOT_ABS$lib"
            fi
        else
            libpath="$(find_library_in_sysroot "$lib")"
        fi
        if [ -n "$libpath" ] && [ -e "$libpath" ]; then
            copy_to_initrd "${libpath#$SYSROOT_ABS}"
            copy_deps_for "$libpath"
        fi
    done
}

# sweep_all_deps — walk NEEDED entries for every ELF file already in
# the initrd workdir. This catches transitive deps that were not
# reached from the initial seed binaries (e.g. shared libraries
# brought in by rsync'd module trees) and ensures versioned SONAME
# symlinks exist for every library that another file requests.
sweep_all_deps() {
    info "Sweeping all ELF files in initrd for missing dependencies..."
    local changed=1 iterations=0
    while [ "$changed" -eq 1 ] && [ "$iterations" -lt 20 ]; do
        changed=0
        iterations=$((iterations + 1))
        while IFS= read -r f; do
            local needed
            needed="$(extract_needed "$f")"
            for lib in $needed; do
                local libpath
                if [[ "$lib" == /* ]]; then
                    local abs_resolved
                    abs_resolved="$(resolve_in_sysroot "$lib")"
                    if [ -f "$abs_resolved" ] || [ -h "$abs_resolved" ]; then
                        libpath="$abs_resolved"
                    else
                        libpath="$SYSROOT_ABS$lib"
                    fi
                else
                    libpath="$(find_library_in_sysroot "$lib")"
                fi
                local rel="${libpath#$SYSROOT_ABS}"
                if [ -n "$libpath" ] && [ -e "$libpath" ] \
                    && [ -z "${COPIED[$rel]:-}" ]; then
                    copy_to_initrd "$rel" || true
                    changed=1
                fi
            done
        done < <(find "$WORKDIR_ABS" -type f \( -name '*.so' -o -name '*.so.*' \) -print0 \
                 | xargs -0 -r -n1 sh -c 'readelf -h "$1" >/dev/null 2>&1 && echo "$1"' _ )
    done
    info "  sweep converged after $iterations iteration(s)"
}

is_elf_file() {
    local file="$1"
    if [ ! -f "$file" ]; then
        return 1
    fi
    if readelf -h "$file" >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

copy_dir_to_initrd() {
    local rel="${1#/}"
    local src="$SYSROOT_ABS/$rel"
    if [ ! -d "$src" ]; then
        return 1
    fi
    mkdir -p "$WORKDIR_ABS/$rel"
    rsync -aH "$src/" "$WORKDIR_ABS/$rel/"
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

rm -rf "$WORKDIR_ABS"
mkdir -p "$WORKDIR_ABS"

# Prefer the annotator binary shipped inside the sysroot when the
# caller did not provide an explicit override. This matches the legacy
# build-minimal-initrd.sh behaviour.
if [ "$ANNOTATOR_BIN" = "$TARGET_DIR/annotator" ] && [ -x "$SYSROOT_ABS/usr/bin/annotator" ]; then
    ANNOTATOR_ABS="$SYSROOT_ABS/usr/bin/annotator"
fi

if [ ! -f "$ANNOTATOR_ABS" ]; then
    error "annotator binary not found: $ANNOTATOR_ABS"
    error "Run: ./scripts/cross.sh"
    exit 1
fi

# BusyBox must come from the sysroot; never copy host binaries into
# the image.
SYSROOT_BUSYBOX="$SYSROOT_ABS/usr/bin/busybox"
if [ ! -x "$SYSROOT_BUSYBOX" ]; then
    error "busybox is missing from the sysroot."
    error "Run: ARCH=x86_64 ./scripts/sysroot.sh"
    exit 1
fi

# Minimal runtime files
for required in "/bin/sh" "/bin/dash" "/bin/mkdir" "/bin/mount" "/bin/ln" "/bin/chmod" "/bin/sleep" "/usr/bin/Xorg" "/usr/bin/xkbcomp"; do
    if ! copy_to_initrd "$required"; then
        error "required runtime file missing from sysroot: $required"
        exit 1
    fi
done

# Install the statically linked sysroot BusyBox and create applet
# symlinks. The set covers everything /init and the runtime may need.
cp -a "$SYSROOT_BUSYBOX" "$WORKDIR_ABS/bin/busybox"
for applet in sh mount umount mkdir ln chmod sleep ls cat find \
              kill killall ps mknod grep awk sed head tail; do
    ln -sf busybox "$WORKDIR_ABS/bin/$applet"
done

# Copy annotator and its dependencies
mkdir -p "$WORKDIR_ABS/usr/bin"
cp -a "$ANNOTATOR_ABS" "$WORKDIR_ABS/usr/bin/annotator"
copy_deps_for "$ANNOTATOR_ABS"
copy_deps_for "$WORKDIR_ABS/usr/bin/annotator"

# Xorg may be a wrapper script; follow it to the real binary.
if is_elf_file "$WORKDIR_ABS/usr/bin/Xorg"; then
    copy_deps_for "$WORKDIR_ABS/usr/bin/Xorg"
else
    info "Detected Xorg wrapper; copying actual Xorg binary and its deps"
    if ! copy_to_initrd "/usr/lib/xorg/Xorg"; then
        error "expected Xorg binary missing from sysroot: /usr/lib/xorg/Xorg"
        exit 1
    fi
    copy_deps_for "$WORKDIR_ABS/usr/lib/xorg/Xorg"
fi

# Xorg module and keyboard data
copy_dir_to_initrd "/usr/lib/xorg/modules"
copy_dir_to_initrd "/usr/share/X11/xkb"
copy_dir_to_initrd "/usr/share/X11/xorg.conf.d" || true

# Walk NEEDED deps for every shared object inside the Xorg module
# tree. Modules are rsync'd wholesale so their individual library
# dependencies are not picked up by the per-binary walk above.
while IFS= read -r mod; do
    [ -n "$mod" ] || continue
    copy_deps_for "$mod"
done < <(find "$WORKDIR_ABS/usr/lib/xorg/modules" -type f \
         \( -name '*.so' -o -name '*.so.*' \) 2>/dev/null)

# Force 1920x1080 display resolution via Xorg config
mkdir -p "$WORKDIR_ABS/etc/X11/xorg.conf.d"
cat > "$WORKDIR_ABS/etc/X11/xorg.conf.d/10-monitor.conf" <<'XORG'
Section "Monitor"
    Identifier "Default Monitor"
    Option "PreferredMode" "1920x1080"
EndSection

Section "Screen"
    Identifier "Default Screen"
    Monitor "Default Monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1920x1080"
    EndSubSection
EndSection
XORG

# Copy dynamic linker configuration if present so the runtime linker
# can locate libraries via the standard search paths in addition to
# the LD_LIBRARY_PATH we set in /init.
mkdir -p "$WORKDIR_ABS/etc"
for f in /etc/ld.so.conf /etc/ld.so.conf.d; do
    if [ -e "$SYSROOT_ABS$f" ]; then
        copy_to_initrd "$f" || true
    fi
done

# Common SDL2 and GL runtime files if present in sysroot
for lib in /usr/lib/$MULTIARCH/libSDL2-2.0.so.0 /usr/lib/$MULTIARCH/libSDL2_ttf-2.0.so.0 /usr/lib/$MULTIARCH/libGL.so.1; do
    if [ -e "$SYSROOT_ABS$lib" ]; then
        copy_to_initrd "$lib"
        copy_deps_for "$SYSROOT_ABS$lib"
    fi
done

# Copy binary dependencies for key runtime commands
copy_deps_for "$WORKDIR_ABS/bin/sh"
copy_deps_for "$WORKDIR_ABS/bin/mkdir"
copy_deps_for "$WORKDIR_ABS/bin/mount"
copy_deps_for "$WORKDIR_ABS/bin/ln"
copy_deps_for "$WORKDIR_ABS/bin/chmod"
copy_deps_for "$WORKDIR_ABS/bin/sleep"
copy_deps_for "$WORKDIR_ABS/bin/ps"
copy_deps_for "$WORKDIR_ABS/usr/bin/xkbcomp"

# Bring in ldconfig (and its dynamic deps) so /init can build
# /etc/ld.so.cache. /usr/sbin/ldconfig is not pulled in by any of
# the above walks because nothing in the seed set depends on it.
for f in /usr/sbin/ldconfig /sbin/ldconfig; do
    if [ -e "$SYSROOT_ABS$f" ]; then
        copy_to_initrd "$f" || true
        copy_deps_for "$SYSROOT_ABS$f"
    fi
done

# Final dep sweep: walk NEEDED entries for every ELF file already
# in the initrd and copy any libraries that are still missing.
# This guarantees that every SONAME any binary requests resolves
# to a real file with a matching symlink in the workdir.
sweep_all_deps

# Kernel modules needed for input + display. We auto-detect the
# kernel version (last installed) and only pull the modules we
# actually need — never the whole tree.
#
# On x86_64 we want QXL (QEMU's paravirt GPU) plus the USB/HID
# stack. On aarch64 (Pi 3B emu) we want vc4 (DRM for the Pi GPU)
# plus the same input stack.
MODSRC=""
MODVER="$(ls -1 "$SYSROOT_ABS/lib/modules" 2>/dev/null | sort -V | tail -n 1 || true)"
if [ -n "$MODVER" ]; then
    MODSRC="$SYSROOT_ABS/lib/modules/$MODVER"
fi
if [ -n "$MODVER" ] && [ -d "$MODSRC" ]; then
    info "kernel module version: $MODVER"
    MODDST="$WORKDIR_ABS/lib/modules/$MODVER"
    mkdir -p "$MODDST"
    # Build the module list based on architecture
    MODS=(
        kernel/drivers/usb/common/usb-common.ko
        kernel/drivers/usb/core/usbcore.ko
        kernel/drivers/usb/host/uhci-hcd.ko
        kernel/drivers/usb/host/ehci-hcd.ko
        kernel/drivers/usb/host/ehci-pci.ko
        kernel/drivers/hid/hid.ko
        kernel/drivers/hid/hid-generic.ko
        kernel/drivers/hid/usbhid/usbhid.ko
        kernel/drivers/input/input-core.ko
        kernel/drivers/input/evdev.ko
        kernel/drivers/input/mousedev.ko
        kernel/drivers/gpu/drm/drm.ko
        kernel/drivers/gpu/drm/ttm/ttm.ko
        kernel/drivers/gpu/drm/drm_ttm_helper.ko
        kernel/drivers/gpu/drm/drm_kms_helper.ko
    )
    if [ "$ARCH" = "x86_64" ]; then
        MODS+=( kernel/drivers/gpu/drm/qxl/qxl.ko )
    fi
    if [ "$ARCH" = "aarch64" ]; then
        MODS+=(
            kernel/drivers/gpu/drm/vc4/vc4.ko
        )
    fi
    for mod in "${MODS[@]}"; do
        if [ -f "$MODSRC/$mod" ]; then
            mkdir -p "$MODDST/$(dirname "$mod")"
            cp -a "$MODSRC/$mod" "$MODDST/$mod"
        fi
    done
    # Copy module dependency info so modprobe/insmod can resolve deps
    if [ -f "$MODSRC/modules.dep" ]; then
        grep -E "(drm\.ko|ttm\.ko|drm_ttm_helper|drm_kms_helper|qxl|vc4|hid\.ko|hid-generic|usbhid|usbcore|uhci-hcd|ehci-hcd|evdev|mousedev|input-core)" \
            "$MODSRC/modules.dep" > "$MODDST/modules.dep" 2>/dev/null || true
    fi
    # Persist the version so /init can auto-detect
    echo "$MODVER" > "$WORKDIR_ABS/.kernelmodver"
fi

# Add a robust init script. This runs as PID 1 inside the initrd and
# is responsible for:
#   - mounting pseudo filesystems
#   - setting up devices and runtime directories
#   - loading DRM kernel modules
#   - launching Xorg and waiting for it to be ready
#   - exec'ing annotator (so annotator inherits PID 1 on its exit path)
#   - reaping zombies and surviving signal storms so the kernel does
#     not panic when child processes die.
cat > "$WORKDIR_ABS/init" <<'INIT'
#!/bin/sh
# init — PID 1 for the minimal annotator initrd.
#
# Design notes:
#   * Do NOT use `set -e`. PID 1 dying is fatal (kernel panic), so any
#     non-critical failure must be tolerated.
#   * All mounts and module loads are best-effort. The system should
#     still come up if, for example, devtmpfs is unavailable.
#   * Background processes are waited on and zombies are reaped so
#     PID 1 never blocks on a defunct child.
#   * annotator is exec'd (not backgrounded) so it inherits PID 1; if
#     it exits, the shell loop restarts it instead of panicking.

# ---- Console / output ----------------------------------------------------
# Open stdin/stdout/stderr onto /dev/console so messages are visible
# on the currently-active console (initially ttyS0, then the QXL
# framebuffer once the display driver takes over). We ALSO write a
# copy to /dev/ttyS0 so headless QEMU runs with `-serial file:...`
# can see init's progress.
exec 0</dev/console 1>/dev/console 2>/dev/console
# Tee every line to the serial console. This is run in the
# background so a stuck ttyS0 write can never deadlock the init
# loop; we don't care about lost log lines.
(
    while IFS= read -r line; do
        printf '%s\n' "$line" >/dev/ttyS0 2>/dev/null || true
    done
) </proc/self/fd/1 >/dev/null 2>&1 &

# Early banner so we know /init actually started
printf '[init] ==================================================\n'
printf '[init] /init started (PID 1, uname=%s, multiarch=%s)\n' "$(uname -m)" "$(cat /.multiarch 2>/dev/null || echo unknown)"
printf '[init] ==================================================\n'

log() {
    printf '[init] %s\n' "$*"
}

# ---- Mount points and pseudo filesystems ---------------------------------
mkdir -p /proc /sys /dev /dev/pts /run /tmp /var/log /var/run 2>/dev/null

# /proc and /sys are required for almost everything.
if ! mount -t proc proc /proc 2>/dev/null; then
    log 'WARN: failed to mount proc'
fi
if ! mount -t sysfs sysfs /sys 2>/dev/null; then
    log 'WARN: failed to mount sysfs'
fi
# devtmpfs is preferred; if unavailable, we'll fall back to mdev-style
# mknod for the few devices we need.
if ! mount -t devtmpfs devtmpfs /dev 2>/dev/null; then
    log 'WARN: devtmpfs unavailable, creating minimal /dev manually'
    # Ensure the absolute minimum device nodes exist
    [ -c /dev/null ] || mknod -m 666 /dev/null c 1 3 2>/dev/null
    [ -c /dev/zero ] || mknod -m 666 /dev/zero c 1 5 2>/dev/null
    [ -c /dev/console ] || mknod -m 600 /dev/console c 5 1 2>/dev/null
    [ -c /dev/tty ] || mknod -m 666 /dev/tty c 5 0 2>/dev/null
    [ -c /dev/random ] || mknod -m 666 /dev/random c 1 8 2>/dev/null
    [ -c /dev/urandom ] || mknod -m 666 /dev/urandom c 1 9 2>/dev/null
fi
# devpts is optional; only useful if a getty is started.
mount -t devpts devpts /dev/pts 2>/dev/null || true
# tmpfs for /run and /tmp is cheap and lets many programs write logs.
mount -t tmpfs tmpfs /run 2>/dev/null || true
mount -t tmpfs tmpfs /tmp 2>/dev/null || true
chmod 1777 /tmp 2>/dev/null

# Standard /dev symlinks used by many programs
[ -e /dev/fd ] || ln -sf /proc/self/fd /dev/fd
[ -e /dev/stdin ] || ln -sf /proc/self/fd/0 /dev/stdin
[ -e /dev/stdout ] || ln -sf /proc/self/fd/1 /dev/stdout
[ -e /dev/stderr ] || ln -sf /proc/self/fd/2 /dev/stderr

# ---- Environment ---------------------------------------------------------
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Multiarch library path — read from /.multiarch (set at build time)
# or derive from uname -m as a last-ditch default.
MULTIARCH_DIR="$(cat /.multiarch 2>/dev/null || true)"
if [ -z "$MULTIARCH_DIR" ]; then
    case "$(uname -m)" in
        x86_64)  MULTIARCH_DIR=x86_64-linux-gnu  ;;
        aarch64) MULTIARCH_DIR=aarch64-linux-gnu ;;
        *)       MULTIARCH_DIR="$(uname -m)-linux-gnu" ;;
    esac
fi
export LD_LIBRARY_PATH="/usr/lib/${MULTIARCH_DIR}:/lib/${MULTIARCH_DIR}"
export DISPLAY=:0
export SDL_VIDEODRIVER=x11
export SDL_AUDIODRIVER=dummy
export HOME=/root
export USER=root
export TERM=linux

# Run ldconfig if /etc/ld.so.conf was copied and ldconfig is available.
# This populates /etc/ld.so.cache so the dynamic linker uses the
# standard search paths, not just LD_LIBRARY_PATH.
if [ -x /sbin/ldconfig ] || [ -x /usr/sbin/ldconfig ] || command -v ldconfig >/dev/null 2>&1; then
    ldconfig 2>/dev/null || true
fi

# ---- Kernel modules (DRM/QXL/VC4) ---------------------------------------
# Auto-detect the kernel module directory. The build-time copy
# writes the version to /.kernelmodver; fall back to glob otherwise.
MODDIR=
if [ -f /.kernelmodver ]; then
    MODDIR="/lib/modules/$(cat /.kernelmodver)"
fi
if [ -z "$MODDIR" ] || [ ! -d "$MODDIR" ]; then
    detected="$(ls -d /lib/modules/*-* 2>/dev/null | head -n 1)"
    if [ -n "$detected" ]; then
        MODDIR="$detected"
        log "using kernel module dir: $MODDIR"
    else
        log 'WARN: no kernel module directory found, skipping DRM load'
        MODDIR=
    fi
fi

if [ -n "$MODDIR" ]; then
    log 'loading kernel modules'
    # Order matters for some of these. We load:
    #   1. USB stack (usbcore, then UHCI/EHCI host controllers)
    #   2. HID layer (hid, hid-generic)
    #   3. usbhid (binds USB HID devices and creates /dev/input/event*)
    #   4. evdev / mousedev (consumers of HID input events)
    #   5. DRM/QXL (display) — qxl on x86, vc4 on aarch64
    #
    # Prime modprobe's internal state for uhci-hcd so the kernel's
    # automatic EHCI->UHCI fallback wiring is set up before we do
    # explicit insmod calls below.  This silences the benign
    # "Warning! ehci_hcd should always be loaded before uhci_hcd"
    # warning that BusyBox modprobe emits.
    modprobe -q uhci-hcd 2>/dev/null || true
    BASE_MODS="
        kernel/drivers/usb/common/usb-common.ko
        kernel/drivers/usb/core/usbcore.ko
        kernel/drivers/usb/host/uhci-hcd.ko
        kernel/drivers/usb/host/ehci-hcd.ko
        kernel/drivers/usb/host/ehci-pci.ko
        kernel/drivers/hid/hid.ko
        kernel/drivers/hid/hid-generic.ko
        kernel/drivers/hid/usbhid/usbhid.ko
        kernel/drivers/input/input-core.ko
        kernel/drivers/input/evdev.ko
        kernel/drivers/input/mousedev.ko
        kernel/drivers/gpu/drm/drm.ko
        kernel/drivers/gpu/drm/ttm/ttm.ko
        kernel/drivers/gpu/drm/drm_ttm_helper.ko
        kernel/drivers/gpu/drm/drm_kms_helper.ko"
    # Per-arch display drivers — QXL is x86 QEMU only, vc4 is Pi.
    case "$(uname -m)" in
        x86_64)
            DISP_MODS="kernel/drivers/gpu/drm/qxl/qxl.ko" ;;
        aarch64)
            DISP_MODS="kernel/drivers/gpu/drm/vc4/vc4.ko" ;;
        *)
            DISP_MODS="" ;;
    esac
    for mod in $BASE_MODS $DISP_MODS; do
        if [ -f "$MODDIR/$mod" ]; then
            insmod "$MODDIR/$mod" 2>/dev/null \
                || log "WARN: insmod $mod failed"
        fi
    done
fi

# ---- Zombie reaper -------------------------------------------------------
# PID 1 must reap its children, otherwise zombies accumulate. POSIX
# sh has no real signal handlers, so we use a background reaper that
# polls `wait` periodically. The SIGCHLD trap nudges the reaper to
# run sooner.
REAP_FLAG=/run/.reap
: > "$REAP_FLAG"
trap 'echo > "$REAP_FLAG"' CHLD

(
    while :; do
        # Reap every currently-dead child without blocking. `wait`
        # returns immediately with status 0 when there are no
        # children to wait for, so we must not loop on its return
        # value — loop on the explicit exit instead.
        n=0
        while wait 2>/dev/null; do
            n=$((n + 1))
            # Cap each reap batch so a flurry of exits doesn't
            # monopolise the CPU.
            [ $n -ge 16 ] && break
        done
        # Wait for a SIGCHLD nudge or a 1s safety timeout
        if [ -e "$REAP_FLAG" ]; then
            : > "$REAP_FLAG"
            continue
        fi
        # 1s sleep — interrupted by SIGCHLD via the trap
        sleep 1 2>/dev/null || true
    done
) &
REAPER_PID=$!

cleanup() {
    log 'shutting down'
    kill -TERM "$REAPER_PID" 2>/dev/null
    kill -TERM "$XORG_PID" 2>/dev/null
    sleep 1
    kill -KILL "$REAPER_PID" 2>/dev/null
    kill -KILL "$XORG_PID" 2>/dev/null
    sync
    # Try to reboot via the kernel sysrq-trigger if available; otherwise
    # just halt.
    if [ -w /proc/sysrq-trigger ]; then
        echo s > /proc/sysrq-trigger 2>/dev/null
        sleep 1
        echo b > /proc/sysrq-trigger 2>/dev/null
    fi
    exit 0
}
trap cleanup TERM INT HUP
# BusyBox sh does not support a CAD (Ctrl-Alt-Del) trap, so we
# simply ignore the signal so PID 1 stays alive on reboot requests.
# (On systems that do support it, you could add: trap ':' CAD)

# ---- Xorg ----------------------------------------------------------------
start_xorg() {
    log 'starting Xorg'
    /usr/bin/Xorg -nolisten tcp -novtswitch -noreset \
        -modulepath /usr/lib/xorg/modules \
        -xkbdir /usr/share/X11/xkb :0 >/run/xorg.log 2>&1 &
    XORG_PID=$!
    log "Xorg PID: $XORG_PID"
    # Wait up to ~10s for the X server socket to appear
    count=0
    while [ ! -e /tmp/.X11-unix/X0 ] && [ $count -lt 100 ]; do
        sleep 0.1
        count=$((count + 1))
    done
    if [ -e /tmp/.X11-unix/X0 ]; then
        log 'Xorg is ready'
        return 0
    fi
    log 'WARN: Xorg did not become ready in time'
    return 1
}

# ---- Main loop -----------------------------------------------------------
# Xorg is started once; if it dies, restart it. annotator is exec'd
# so it inherits PID 1. If annotator returns (which would only happen
# if it exec fails), we restart it after a short delay.

mkdir -p /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix

start_xorg

attempts=0
while :; do
    # If Xorg died, restart it
    if ! kill -0 "$XORG_PID" 2>/dev/null; then
        log "Xorg (pid $XORG_PID) exited; restarting"
        sleep 1
        start_xorg || log 'Xorg restart failed, continuing anyway'
    fi

    if [ "$attempts" -gt 0 ]; then
        log "annotator restart attempt $attempts"
        sleep 2
    fi
    attempts=$((attempts + 1))

    # exec annotator. This replaces the shell with annotator. On
    # exec failure (shouldn't happen — we verified the binary exists)
    # the shell continues, we sleep, and retry.
    if ! exec /usr/bin/annotator; then
        log 'ERROR: exec /usr/bin/annotator failed'
    fi
done
INIT
chmod +x "$WORKDIR_ABS/init"

# Ensure the dynamic loader is a real file inside the initrd.
# Some sysroot packages install the ld-linux interpreter as a
# symlink that points to an absolute path outside the sysroot.
# That leaves the initrd with a dangling symlink and the kernel
# fails with ENOENT when running interpreted /init.
#
# The interpreter SONAME differs per arch; resolve it from a
# known-ELF file (annotator or Xorg) via readelf.
INTERP=""
for probe in "$WORKDIR_ABS/usr/bin/annotator" "$WORKDIR_ABS/usr/lib/xorg/Xorg"; do
    if [ -f "$probe" ]; then
        INTERP="$(readelf -l "$probe" 2>/dev/null \
            | awk '/interpreter:/ {gsub(/\[|\]/, "", $NF); print $NF; exit}')"
        [ -n "$INTERP" ] && break
    fi
done
if [ -n "$INTERP" ]; then
    INTERP_NAME="$(basename "$INTERP")"
    ld_in_sysroot="$(find_library_in_sysroot "$INTERP_NAME" || true)"
    if [ -n "$ld_in_sysroot" ]; then
        dst="$WORKDIR_ABS${ld_in_sysroot#$SYSROOT_ABS}"
        mkdir -p "$(dirname "$dst")"
        [ -e "$dst" ] && rm -f "$dst"
        cp -a "$ld_in_sysroot" "$dst"
    fi
    # Provide a /lib64/ld-linux-* shim at the ABI-required location
    # when applicable. x86_64 binaries request /lib64/ld-linux-x86-64.so.2
    # as their interpreter, and aarch64 binaries use /lib/ld-linux-aarch64.so.1.
    case "$INTERP" in
        /lib64/ld-linux-*)
            base="$(basename "$INTERP")"
            target="$WORKDIR_ABS/lib/$MULTIARCH/$base"
            if [ -f "$target" ]; then
                mkdir -p "$WORKDIR_ABS/lib64"
                ln -sfn "../lib/$MULTIARCH/$base" "$WORKDIR_ABS/lib64/$base"
            fi
            ;;
    esac
fi

# Drop a file /init reads to know its multiarch dir for LD_LIBRARY_PATH
echo "$MULTIARCH" > "$WORKDIR_ABS/.multiarch"

# ---------------------------------------------------------------------------
# Verify symlinks before packing
# ---------------------------------------------------------------------------

# Walk every symlink in the workdir and report any that dangle. A
# dangling symlink means a SONAME link whose real file was not
# copied — the dynamic linker would fail to resolve it at runtime.
broken_count=0
while IFS= read -r link; do
    [ -n "$link" ] || continue
    target="$(readlink "$link")"
    # Resolve relative targets against the symlink's directory
    if [[ "$target" != /* ]]; then
        check="$(dirname "$link")/$target"
    else
        check="$WORKDIR_ABS$target"
    fi
    if [ ! -e "$check" ]; then
        warn "dangling symlink in initrd: $link -> $target"
        broken_count=$((broken_count + 1))
    fi
done < <(find "$WORKDIR_ABS" -type l)

if [ "$broken_count" -gt 0 ]; then
    warn "$broken_count dangling symlink(s) remain in the initrd"
else
    info "All symlinks resolve correctly"
fi

# ---------------------------------------------------------------------------
# Create the cpio archive. We use fakeroot when available so the
# resulting archive has sane root:root ownership without needing to
# be the actual root user (no permission mess on the workdir).
# ---------------------------------------------------------------------------
info "Creating cpio archive..."
rm -f "$OUT_ABS"

# Sanity: refuse to overwrite the host's own / when WORKDIR is "/".
[ "$WORKDIR_ABS" = "/" ] && die "WORKDIR is / — refusing to pack"

# gzip is mandatory (we output .img.gz by default and the kernel
# expects gzip-compressed initrds).
require_cmd gzip

if command -v fakeroot >/dev/null 2>&1; then
    # fakeroot fakes root privileges for the subprocess, letting
    # cpio write root:root ownership into the archive. The workdir
    # itself stays owned by the user.
    info "  using fakeroot for root ownership in archive"
    ( cd "$WORKDIR_ABS" && \
      fakeroot -- find . -print0 | sort -z \
        | fakeroot cpio -o --null --format=newc \
        | gzip -9 ) > "$OUT_ABS"
else
    if [ "$(id -u)" -ne 0 ]; then
        die "fakeroot is not installed and we are not root.
Install fakeroot:  sudo apt install fakeroot
Or re-run under sudo:  sudo $0 $*"
    fi
    info "  using real root (no fakeroot available)"
    ( cd "$WORKDIR_ABS" && \
      find . -print0 | sort -z \
        | cpio -o --null --format=newc \
        | gzip -9 ) > "$OUT_ABS"
fi

# Sanity check the resulting archive
if [ ! -s "$OUT_ABS" ]; then
    die "cpio archive is empty: $OUT_ABS"
fi
info "  size: $(du -h "$OUT_ABS" | cut -f1)"

info "Initrd ready: $OUT_ABS"
info "Boot it with: ./scripts/run-minimal.sh"
