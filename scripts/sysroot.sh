#!/usr/bin/env bash
# sysroot.sh — fetch the Debian library packages needed to link against.
#
# This replaces the old debootstrap/chroot approach entirely.  Everything
# here runs as an ordinary user:
#
#   * apt fetches package indexes into a user-writable state directory
#   * `apt-get download` pulls .deb files without installing them
#   * `dpkg-deb -x` unpacks them into a plain directory tree
#
# Only the library packages are fetched — shared objects, static
# archives, headers and pkg-config files.  No shell, no coreutils, no
# package manager, no operating system: the result is a link-time
# search root, not a root filesystem.
#
# No sudo, no debootstrap, no chroot, no qemu-user-static, no buildroot.
#
# Usage:
#   ./scripts/sysroot.sh                  # libraries for $ARCH (default x86_64)
#   ARCH=aarch64 ./scripts/sysroot.sh
#   ./scripts/sysroot.sh --force          # rebuild even if it exists
#   ./scripts/sysroot.sh --clean          # remove the directory
#
# Environment:
#   ARCH     x86_64 | aarch64          (default: x86_64)
#   SUITE    Debian suite              (default: bookworm)
#   MIRROR   Debian mirror URL         (default: http://deb.debian.org/debian)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

SUITE="${SUITE:-bookworm}"
MIRROR="${MIRROR:-http://deb.debian.org/debian}"
FORCE=0
CLEAN=0

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --arch ARCH      Target architecture (x86_64 or aarch64)
  --suite SUITE    Debian suite (default: bookworm)
  --mirror URL     Debian mirror (default: $MIRROR)
  --force          Rebuild the library directory even if it already exists
  --clean          Remove the library directory and exit
  -h, --help       Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)     ARCH="$2"; shift 2 ;;
        --suite)    SUITE="$2"; shift 2 ;;
        --mirror)   MIRROR="$2"; shift 2 ;;
        --force|-f) FORCE=1; shift ;;
        --clean)    CLEAN=1; shift ;;
        -h|--help)  usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  DEB_ARCH=amd64; DEB_MULTIARCH=x86_64-linux-gnu ;;
    aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64; DEB_MULTIARCH=aarch64-linux-gnu ;;
    *) die "Unsupported arch: $ARCH (use x86_64 or aarch64)" ;;
esac

SYSROOT="$TARGET_DIR/$ARCH-libs"
APT_STATE="$TARGET_DIR/apt-state/$ARCH"
DEB_CACHE="$TARGET_DIR/apt-cache/$ARCH"
KEYRING_DIR="$TARGET_DIR/apt-keyring"

# ── Package set ───────────────────────────────────────────────────────────
# Everything needed to compile and link SDL2 + SDL2_ttf + X11 + freetype.
#
# NOTE: the `-dev` packages ship headers and `.so` symlinks but NOT the
# shared objects themselves — those live in the runtime packages
# (e.g. libasound2).  Both sets are needed, so the linker can resolve
# SDL2's own dependencies.
#
# SDL2 is built with the ALSA / PulseAudio / Wayland / KMSDRM backends
# enabled, so libSDL2.so has undefined references to all of them even
# though we only ever use the X11 backend.  The linker needs to resolve
# them, hence the long tail of packages below.
COMMON_DEVS=(
    # SDL2 + SDL2_ttf and their text rendering stack.
    # The runtime packages (libsdl2-2.0-0, libsdl2-ttf-2.0-0) are
    # needed as well: the -dev package's libSDL2.so is a symlink to the
    # runtime library, and without it the symlink dangles.
    libsdl2-dev libsdl2-2.0-0
    libsdl2-ttf-dev libsdl2-ttf-2.0-0
    libfreetype6 libfreetype-dev
    libpng16-16 libpng-dev
    zlib1g zlib1g-dev
    libbz2-1.0 libbz2-dev
    libbrotli1 libbrotli-dev
    # X11 and its dependencies
    libx11-6 libx11-dev
    libxext6 libxext-dev
    libxrandr2 libxrandr-dev
    libxcursor1 libxcursor-dev
    libxi6 libxi-dev
    libxinerama1 libxinerama-dev
    libxss1 libxss-dev
    libxrender1 libxrender-dev
    libxfixes3 libxfixes-dev
    libxau6 libxau-dev
    libxdmcp6 libxdmcp-dev
    libxxf86vm1 libxxf86vm-dev
    libxcb1 libxcb1-dev
    libxcb-render0 libxcb-render0-dev
    libxcb-shm0 libxcb-shm0-dev
    libxcb-xfixes0 libxcb-xfixes0-dev
    libxcb-randr0 libxcb-randr0-dev
    libxcb-shape0 libxcb-shape0-dev
    libxcb-xkb1 libxcb-xkb-dev
    libxshmfence1 libxshmfence-dev
    libx11-xcb1 libx11-xcb-dev
    libxkbcommon-x11-0 libxkbcommon-x11-dev
    # SDL2's optional backends — referenced by libSDL2.so even when unused
    libasound2 libasound2-dev
    libpulse0 libpulse-dev
    libsamplerate0 libsamplerate0-dev
    libgbm1 libgbm-dev
    libwayland-client0 libwayland-dev
    libwayland-egl1 libwayland-egl-backend-dev
    libwayland-cursor0
    libdecor-0-0 libdecor-0-dev
    libxkbcommon0 libxkbcommon-dev
    # Transitive dependencies of the above
    libdbus-1-3 libsystemd0 libcap2 libgcrypt20 libgpg-error0
    liblzma5 libzstd1 liblz4-1 liblz4-1
    # DRM / GBM support used by SDL2's accelerated renderer
    libdrm2 libdrm-dev
    # Video4Linux2 capture support
    libv4l-0 libv4l-dev
    # Misc
    libbsd0 libbsd-dev
    libmd0 libmd-dev
    # Text shaping, needed by SDL2_ttf >= 2.20
    libharfbuzz0b libharfbuzz-dev
    # Transitive dependencies of the libraries above (wayland-server,
    # glib, graphite2, ...) and of the runtime libraries.
    libwayland-server0
    libglib2.0-0
    libgraphite2-3
    libexpat1
    libffi8
    libmount1
    libpcre2-8-0
    libselinux1
    libsndfile1
    libsystemd0
    libcap2
    libgcrypt20
    libgpg-error0
    liblzma5
    libzstd1
    liblz4-1
    libasyncns0
)

# ── APT plumbing ──────────────────────────────────────────────────────────
# apt is pointed at a user-writable state directory so that no root
# privileges are needed for the index/cache/lock files.
#
# `Dir::State::status` MUST be redirected to an empty file: otherwise
# apt sees the packages already installed on the host (Ubuntu versions)
# and tries to download those exact versions, which do not exist in the
# Debian archive.
APT_OPTS=(
    -o "Dir::State=$APT_STATE"
    -o "Dir::State::lists=$APT_STATE/lists"
    -o "Dir::State::status=$APT_STATE/status"
    -o "Dir::Cache=$DEB_CACHE"
    -o "Dir::Etc::sourcelist=$APT_STATE/sources.list"
    -o "Dir::Etc::sourceparts=$APT_STATE/sources.list.d"
    -o "Dir::Etc::preferences=$APT_STATE/preferences"
    -o "Dir::Etc::preferencesparts=$APT_STATE/preferences.d"
    -o "Dir::Etc::trusted=$APT_STATE/trusted.gpg"
    -o "Dir::Etc::trustedparts=$APT_STATE/trusted.gpg.d"
    -o "APT::Architectures=$DEB_ARCH"
    -o "APT::Get::List-Cleanup=0"
)

# Packages are fetched for $DEB_ARCH; the cross toolchain (aarch64) is
# always an amd64 (host) package.
apt_update() {
    local suite="$1"
    mkdir -p "$APT_STATE/lists/partial" "$DEB_CACHE/archives/partial" \
             "$APT_STATE/sources.list.d" "$APT_STATE/preferences.d" \
             "$APT_STATE/trusted.gpg.d"

    # An empty dpkg status file makes apt treat every package as
    # not-installed, so it resolves versions from the Debian index
    # rather than from the host's Ubuntu installation.
    : > "$APT_STATE/status"

    # Verify the repository against the Debian archive keyring we fetched
    # ourselves.  The keyring is a symlink farm in the unpacked package,
    # so point apt at the .pgp file directly.
    local keyring="$KEYRING_DIR/usr/share/keyrings/debian-archive-keyring.gpg"
    if [[ -f "$keyring" ]]; then
        echo "deb [arch=$DEB_ARCH signed-by=$keyring] $MIRROR $suite main" \
            > "$APT_STATE/sources.list"
    else
        warn "No archive keyring available — falling back to trusted=yes"
        echo "deb [arch=$DEB_ARCH trusted=yes] $MIRROR $suite main" \
            > "$APT_STATE/sources.list"
    fi

    info "Fetching $suite/$DEB_ARCH package indexes (user-writable apt state)..."
    apt-get "${APT_OPTS[@]}" update >/dev/null
}

apt_download() {
    # apt_download <pkg> <arch>
    # `apt-get download` writes into the current directory, so run it from
    # the package cache directory.
    ( cd "$DEB_CACHE/debs" && apt-get "${APT_OPTS[@]}" download "$1:$2" >/dev/null 2>&1 )
}

# ── Keyring ───────────────────────────────────────────────────────────────
# The Debian archive signing key, fetched and unpacked without root.
ensure_keyring() {
    if [[ -f "$KEYRING_DIR/debian-archive-keyring.gpg" ]]; then
        return
    fi
    info "Fetching Debian archive keyring..."
    local tmp
    tmp="$(mktemp -d)"
    local base="${MIRROR%/debian}"
    (
        cd "$tmp" || exit 1
        curl -sSLO "$base/debian/pool/main/d/debian-archive-keyring/debian-archive-keyring_2025.1_all.deb" 2>/dev/null
    ) || true
    if [[ -f "$tmp/debian-archive-keyring_2025.1_all.deb" ]]; then
        mkdir -p "$KEYRING_DIR"
        dpkg-deb -x "$tmp/debian-archive-keyring_2025.1_all.deb" "$KEYRING_DIR"
        info "Keyring ready"
    else
        warn "Could not fetch the Debian keyring; continuing with trusted=yes"
    fi
    rm -rf "$tmp"
}

# ── Clean ─────────────────────────────────────────────────────────────────
if [[ $CLEAN -eq 1 ]]; then
    rm -rf "$SYSROOT"
    info "Removed $SYSROOT"
    exit 0
fi

ensure_target_dir

# The path is baked into the binaries' RUNPATH, so it has to be absolute.
# TARGET_DIR is guaranteed to exist now, so resolve it once and anchor
# every other path to it.
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"
SYSROOT="$TARGET_DIR/$ARCH-libs"
APT_STATE="$TARGET_DIR/apt-state/$ARCH"
DEB_CACHE="$TARGET_DIR/apt-cache/$ARCH"
KEYRING_DIR="$TARGET_DIR/apt-keyring"

if [[ -d "$SYSROOT" ]]; then
    if [[ $FORCE -eq 0 ]]; then
        info "Sysroot already present at $SYSROOT (use --force to rebuild)"
        exit 0
    fi
    info "Removing existing sysroot $SYSROOT"
    rm -rf "$SYSROOT"
fi

mkdir -p "$SYSROOT" "$DEB_CACHE"

ensure_keyring
apt_update "$SUITE"

# ── Fetch ─────────────────────────────────────────────────────────────────
DEBDIR="$DEB_CACHE/debs"
mkdir -p "$DEBDIR"

fetch_pkg() {
    # fetch_pkg <pkg> <arch> — downloads into $DEBDIR, returns non-zero if
    # the package could not be fetched.  apt-get download names the file
    # after the package, so we can check for that name directly instead
    # of counting files.
    local pkg="$1" arch="$2"
    apt_download "$pkg" "$arch" || return 1
    compgen -G "$DEBDIR/${pkg}_*.deb" >/dev/null
}

# The cross toolchain (aarch64) is an amd64 (host) package: it provides
# the C runtime, the linker and libgcc for the TARGET architecture.
# Only the pieces needed for linking are required — we never compile C
# for the target.
#
# The libgcc package is versioned to match the host gcc (12 on bookworm,
# 10 on bullseye), so the exact name is discovered from the index.
CROSS_PKGS=(
    libc6-dev-arm64-cross
    linux-libc-dev-arm64-cross
    libgcc-s1-arm64-cross
)

# The runtime libraries for the target.  These are native arm64 packages
# (not the "cross" variants), so they match the suite being used and
# therefore carry the same glibc version as the rest of the sysroot.
TARGET_RUNTIME=(
    libc6
    libatomic1
)

find_libgcc_cross() {
    # Echo the name of the libgcc-*-dev-arm64-cross package, if any.
    apt-cache "${APT_OPTS[@]}" search 'libgcc-[0-9]*-dev-arm64-cross' 2>/dev/null \
        | awk '/^libgcc-[0-9]+-dev-arm64-cross /{print $1; exit}'
}

info "Downloading packages for $DEB_ARCH..."
FAILED=()
n=0
total=${#COMMON_DEVS[@]}
for pkg in "${COMMON_DEVS[@]}"; do
    n=$((n + 1))
    printf '  [%2d/%2d] %-24s ' "$n" "$total" "$pkg"
    if fetch_pkg "$pkg" "$DEB_ARCH"; then
        echo "ok"
    else
        echo "MISSING"
        FAILED+=("$pkg")
    fi
done

if [[ ${#FAILED[@]} -gt 0 ]]; then
    warn "Not available in $SUITE: ${FAILED[*]}"
    warn "Falling back to bullseye for those packages."
    apt_update bullseye
    for pkg in "${FAILED[@]}"; do
        fetch_pkg "$pkg" "$DEB_ARCH" || warn "still missing: $pkg"
    done
    # Restore the primary suite so that anything fetched afterwards
    # (the target runtime, for instance) comes from it and not from the
    # older fallback.
    apt_update "$SUITE"
fi

# SDL2_ttf is needed for the text rendering; take it from the same suite.
if ! fetch_pkg libsdl2-ttf-dev "$DEB_ARCH"; then
    apt_update bullseye
    fetch_pkg libsdl2-ttf-dev "$DEB_ARCH" \
        || die "libsdl2-ttf-dev is required but could not be downloaded"
fi

# Cross-compiling needs the target's libc and libgcc.  These are host
# (amd64) packages that carry the aarch64 files.
if [[ "$ARCH" == "aarch64" ]]; then
    info "Fetching the aarch64 cross runtime (libc + libgcc)..."
    for pkg in "${CROSS_PKGS[@]}"; do
        fetch_pkg "$pkg" amd64 \
            || die "$pkg is required for cross-compiling but could not be downloaded"
    done

    # The libgcc cross package is versioned; try the current suite first,
    # then bullseye, then whatever the index offers.
    LIBGCC_PKG="$(find_libgcc_cross || true)"
    if [[ -z "$LIBGCC_PKG" ]]; then
        apt_update bullseye
        LIBGCC_PKG="$(find_libgcc_cross || true)"
        # Back to the primary suite — the libgcc version has to match
        # the C runtime that is fetched below.
        apt_update "$SUITE"
    fi
    [[ -n "$LIBGCC_PKG" ]] \
        || die "no libgcc-*-dev-arm64-cross package found for cross-compiling"
    info "  using $LIBGCC_PKG"
    fetch_pkg "$LIBGCC_PKG" amd64 \
        || die "$LIBGCC_PKG could not be downloaded"

    # The target's runtime libraries (libc.so.6 and friends).  These are
    # native arm64 packages so that they match the suite's glibc.
    for pkg in "${TARGET_RUNTIME[@]}"; do
        fetch_pkg "$pkg" "$DEB_ARCH" \
            || die "$pkg is required for cross-compiling but could not be downloaded"
    done
fi

# ── Unpack ────────────────────────────────────────────────────────────────
info "Unpacking packages into $SYSROOT..."
cd "$DEBDIR" || die "no package cache at $DEBDIR"
debs=( ./*.deb )
n=0
total=${#debs[@]}
for deb in "${debs[@]}"; do
    [[ -e "$deb" ]] || continue
    n=$((n + 1))
    printf '\r  unpacking [%2d/%2d] %-40s' "$n" "$total" "$(basename "$deb")"
    dpkg-deb -x "$deb" "$SYSROOT" 2>/dev/null || warn "failed to unpack $deb"
done
printf '\r%-60s\r' ""

# ── Resolve missing shared libraries ──────────────────────────────────────
# The libraries we unpacked have dependencies of their own, and those
# are not all pulled in by the -dev packages.  Rather than listing
# every one of them by hand, find what is still missing and fetch it.
#
# The Debian Contents index maps every file to the package that ships
# it, so the package for a missing library can be looked up directly.
missing_libs() {
    # Print the names of the shared libraries that are referenced but
    # not present in the library directory.
    #
    # The set of available libraries is collected once with a single
    # find; looking each one up separately would be quadratic and takes
    # minutes.
    local tmp avail needed
    tmp="$(mktemp)"
    avail="$(mktemp)"
    needed="$(mktemp)"

    # Everything that is present.
    find "$SYSROOT/usr/lib/$DEB_MULTIARCH" "$SYSROOT/lib/$DEB_MULTIARCH" \
         "$SYSROOT/usr/$DEB_MULTIARCH/lib" \
         -name '*.so*' -printf '%f\n' 2>/dev/null | sort -u > "$avail"

    # Everything that is referenced.
    while IFS= read -r f; do
        readelf -d "$f" 2>/dev/null | sed -n 's/.*NEEDED.*\[\(.*\)\].*/\1/p'
    done < <(
        find "$SYSROOT/usr/lib/$DEB_MULTIARCH" "$SYSROOT/lib/$DEB_MULTIARCH" \
             "$SYSROOT/usr/$DEB_MULTIARCH/lib" \
             -name '*.so*' -type f 2>/dev/null
    ) | sort -u > "$needed"

    comm -23 "$needed" "$avail"

    rm -f "$tmp" "$avail" "$needed"
}

# Map a shared library name to the package that ships it, using the
# Debian Contents index.
find_package_for_lib() {
    local lib="$1" contents
    contents="$APT_STATE/Contents-$DEB_ARCH"
    if [[ ! -s "$contents" ]]; then
        info "Fetching the Contents index to resolve library dependencies..."
        local base="${MIRROR%/debian}"
        local gz="$APT_STATE/Contents-$DEB_ARCH.gz"
        if [[ ! -s "$gz" ]]; then
            curl -sSL -o "$gz" \
                "$base/debian/dists/$SUITE/main/Contents-$DEB_ARCH.gz" \
                2>/dev/null || true
        fi
        if [[ -s "$gz" ]]; then
            zcat "$gz" > "$contents" 2>/dev/null || true
        else
            warn "could not fetch the Contents index"
        fi
    fi
    [[ -s "$contents" ]] || return 1
    # The Contents file lists the path in the first column and the
    # package name — prefixed by its section — in the last one.
    #
    # grep with an early exit is used rather than awk: the index is
    # over a hundred megabytes and scanning it once per library is far
    # too slow.
    local line
    line="$(grep -m1 -E "/${lib//./\\.}[[:space:]]" "$contents" 2>/dev/null || true)"
    [[ -n "$line" ]] || return 1
    printf '%s\n' "$line" | awk '
        {
            split($NF, a, ",")
            for (i in a) {
                n = a[i]
                sub(/^[^/]*\//, "", n)
                if (n ~ /^[a-z0-9][a-z0-9+.-]*$/) { print n; exit }
            }
        }
    '
}

for round in 1 2 3 4 5 6; do
    MISSING="$(missing_libs)"
    [[ -n "$MISSING" ]] || break
    info "Resolving missing shared libraries (round $round): $(echo "$MISSING" | tr '\n' ' ')"
    RESOLVED=()
    for lib in $MISSING; do
        pkg="$(find_package_for_lib "$lib" || true)"
        if [[ -n "$pkg" ]]; then
            RESOLVED+=("$pkg")
        else
            warn "no package found for $lib"
        fi
    done
    [[ ${#RESOLVED[@]} -gt 0 ]] || break
    # Deduplicate.
    mapfile -t RESOLVED < <(printf '%s\n' "${RESOLVED[@]}" | sort -u)
    for pkg in "${RESOLVED[@]}"; do
        printf '  fetching %-28s ' "$pkg"
        if fetch_pkg "$pkg" "$DEB_ARCH"; then
            deb="$(compgen -G "$DEBDIR/${pkg}_*.deb" | head -1)"
            if [[ -n "$deb" ]]; then
                dpkg-deb -x "$deb" "$SYSROOT" 2>/dev/null || warn "failed to unpack $deb"
            fi
            echo "ok"
        else
            echo "FAILED"
            warn "could not fetch $pkg"
        fi
    done
done

# The unpacked tree needs a sane layout for the linker:
#  * libs live under usr/lib/<multiarch>
#  * aarch64 cross libs live under usr/aarch64-linux-gnu/lib
#  * the dynamic loader lives under lib/<multiarch>
for dir in \
    "$SYSROOT/usr/lib/$DEB_MULTIARCH" \
    "$SYSROOT/lib/$DEB_MULTIARCH" \
    "$SYSROOT/usr/$DEB_MULTIARCH/lib"
do
    mkdir -p "$dir"
done

# On a real Debian system the shared libraries live in /lib/<multiarch>
# and /lib is a symlink to /usr/lib.  The libc.so linker script refers
# to them by their /usr/<multiarch>/lib path, so the shared objects have
# to be reachable there as well.  The crt objects live alongside them
# in the same directory, so link the individual libraries rather than
# replacing the directory.
if [[ "$ARCH" == "aarch64" ]]; then
    mkdir -p "$SYSROOT/usr/$DEB_MULTIARCH/lib"
    for so in "$SYSROOT/lib/$DEB_MULTIARCH"/*.so*; do
        [[ -e "$so" ]] || continue
        ln -sfn "$so" "$SYSROOT/usr/$DEB_MULTIARCH/lib/$(basename "$so")"
    done
fi

# A directory that contains ONLY the shared libraries.
#
# The -dev packages ship both a static archive (libfoo.a) and a shared
# object (libfoo.so) side by side.  GNU ld prefers the archive, which
# would link SDL2 statically and then fail on the libraries Debian does
# not ship as archives (wayland, gbm, pulse, ...).  Putting the shared
# objects in their own directory and searching that first keeps the
# link dynamic.
SHARED_DIR="$SYSROOT/usr/lib/$DEB_MULTIARCH-shared"
mkdir -p "$SHARED_DIR"
for so in "$SYSROOT/usr/lib/$DEB_MULTIARCH"/*.so*; do
    [[ -e "$so" ]] || continue
    ln -sfn "$so" "$SHARED_DIR/$(basename "$so")"
done

# Some packages install their libraries in a subdirectory — PulseAudio
# puts libpulsecommon there.  Those are needed at runtime as well.
for sub in "$SYSROOT/usr/lib/$DEB_MULTIARCH"/*/; do
    [[ -d "$sub" ]] || continue
    for so in "$sub"*.so*; do
        [[ -e "$so" ]] || continue
        ln -sfn "$so" "$SHARED_DIR/$(basename "$so")"
    done
done
# A runtime directory for the host build, without the C runtime.
#
# The x86_64 binary is linked against the HOST's glibc, which is newer
# than the one in this directory.  If the loader found this directory's
# older libc first it would fail with "GLIBC_2.39 not found", so the
# host build gets a runtime path that leaves the C runtime out.
HOST_RT_DIR="$SYSROOT/usr/lib/$DEB_MULTIARCH-runtime"
mkdir -p "$HOST_RT_DIR"
for so in "$SHARED_DIR"/*.so*; do
    [[ -e "$so" ]] || continue
    base="$(basename "$so")"
    case "$base" in
        libc.so*|libm.so*|libdl.so*|libpthread.so*|librt.so*|libgcc_s.so*|\
        ld-linux*.so*|libresolv.so*|libutil.so*|libnsl.so*|libcrypt.so*)
            continue ;;
    esac
    ln -sfn "$(readlink -f "$so")" "$HOST_RT_DIR/$base"
done
# Some libraries keep their own dependencies in a subdirectory and
# carry an absolute RUNPATH that points at the host filesystem — for
# example libpulse.so.0 looks for libpulsecommon in
# /usr/lib/<multiarch>/pulseaudio.  A library's own RUNPATH takes
# priority over the executable's, so the dependency would be looked for
# on the host and not found.
#
# Rewriting those RUNPATHs to point at the sysroot fixes it.  patchelf
# is a small standalone tool; it is downloaded and unpacked into the
# target directory rather than installed system-wide.
for sub in "$SYSROOT/usr/lib/$DEB_MULTIARCH"/*/; do
    [[ -d "$sub" ]] || continue
    for so in "$sub"*.so*; do
        [[ -e "$so" ]] || continue
        ln -sfn "$(readlink -f "$so")" "$SHARED_DIR/$(basename "$so")"
    done
done

PATCHELF="$TARGET_DIR/tools/patchelf"
if [[ ! -x "$PATCHELF" ]]; then
    info "Fetching patchelf (needed to fix library RUNPATHs)..."
    mkdir -p "$TARGET_DIR/tools"
    tmp="$(mktemp -d)"
    (
        cd "$tmp" || exit 1
        curl -sSLO "$MIRROR/../debian/pool/main/p/patchelf/patchelf_0.18.0-1.4_amd64.deb" 2>/dev/null
    ) || true
    if [[ -f "$tmp/patchelf_0.18.0-1.4_amd64.deb" ]]; then
        dpkg-deb -x "$tmp/patchelf_0.18.0-1.4_amd64.deb" "$tmp/x"
        cp -f "$tmp/x/usr/bin/patchelf" "$PATCHELF" 2>/dev/null || true
        chmod +x "$PATCHELF" 2>/dev/null || true
    fi
    rm -rf "$tmp"
fi

if [[ -x "$PATCHELF" ]]; then
    # Patch the real files.  The .so entries are often symlinks to a
    # versioned file, so resolve each one before patching it — and
    # patch each real file only once, otherwise the prefix would be
    # applied repeatedly.
    patched=0
    while IFS= read -r real; do
        [[ -f "$real" ]] || continue
        rp="$(readelf -d "$real" 2>/dev/null | sed -n 's/.*RUNPATH.*\[\(.*\)\].*/\1/p' || true)"
        [[ -n "$rp" ]] || continue
        # Skip anything that already points inside the sysroot.  The
        # test is written as an if because a bare "[[ ... ]] && continue"
        # returns non-zero under `set -e` and would abort the script.
        if [[ "$rp" == "$SYSROOT"* ]]; then
            continue
        fi
        new="$rp"
        for p in ${rp//:/ }; do
            [[ "$p" == /* ]] || continue
            new="${new//$p/$SYSROOT$p}"
        done
        "$PATCHELF" --set-rpath "$new" "$real" 2>/dev/null || true
        patched=$((patched + 1))
    done < <(find "$SYSROOT/usr/lib/$DEB_MULTIARCH" "$SYSROOT/lib/$DEB_MULTIARCH" \
                  -name '*.so*' -type f 2>/dev/null | sort -u)
    info "  patched RUNPATHs in $patched libraries"
else
    warn "patchelf unavailable — libraries with absolute RUNPATHs may not resolve"
fi

# ── Sanity check ──────────────────────────────────────────────────────────
missing=()
for lib in libSDL2.so libSDL2_ttf.so libfreetype.so libX11.so libxcb.so; do
    if ! find "$SYSROOT" -name "$lib*" -print -quit | grep -q .; then
        missing+=("$lib")
    fi
done
if [[ ${#missing[@]} -gt 0 ]]; then
    die "sysroot incomplete, missing shared libraries: ${missing[*]}"
fi

info "Sysroot ready: $SYSROOT"
