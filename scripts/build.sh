#!/usr/bin/env bash
# build.sh — compile the annotator against the Debian library packages.
#
# The libraries are produced by scripts/sysroot.sh: Debian packages are
# downloaded and unpacked into a plain directory, no root required.
#
# The compiled binary is dynamically linked against those libraries.  It
# carries an RPATH pointing at the directory, so it can be copied to a
# machine with the same architecture and run as-is (provided the
# libraries travel with it, or are installed system-wide).
#
# Usage:
#   ./scripts/build.sh                 # host arch (x86_64)
#   ARCH=aarch64 ./scripts/build.sh    # cross-compile
#
# Environment:
#   ARCH      x86_64 | aarch64                (default: x86_64)
#   TARGET    cargo target triple             (default: $ARCH-unknown-linux-gnu)
#   SYSROOT   library directory               (default: target/$ARCH-libs)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

JOBS="$(nproc)"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --arch ARCH       Target architecture (x86_64 or aarch64)
  --target TRIPLE   Cargo target triple (default: $ARCH-unknown-linux-gnu)
  -j, --jobs N      Parallel jobs (default: $JOBS)
  -h, --help        Show this help
EOF
}

TARGET_OVERRIDE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)    ARCH="$2"; shift 2 ;;
        --target)  TARGET_OVERRIDE="$2"; shift 2 ;;
        -j|--jobs) JOBS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  MULTIARCH=x86_64-linux-gnu ;;
    aarch64|arm64) ARCH=aarch64; MULTIARCH=aarch64-linux-gnu ;;
    *) die "Unsupported ARCH '$ARCH' (use x86_64 or aarch64)" ;;
esac

# common.sh derives ARCH / TARGET / SYSROOT from the environment before
# the command line is parsed, so recompute them here from the final ARCH.
# An explicit --target still wins.
if [[ -n "$TARGET_OVERRIDE" ]]; then
    TARGET="$TARGET_OVERRIDE"
else
    case "$ARCH" in
        x86_64)  TARGET=x86_64-unknown-linux-gnu ;;
        aarch64) TARGET=aarch64-unknown-linux-gnu ;;
    esac
fi
SYSROOT="$TARGET_DIR/$ARCH-libs"

if [[ ! -d "$SYSROOT" ]]; then
    die "Sysroot not found at $SYSROOT — run: ARCH=$ARCH ./scripts/sysroot.sh"
fi

LIBDIR="$SYSROOT/usr/lib/$MULTIARCH"
LIBDIR2="$SYSROOT/lib/$MULTIARCH"
# Contains only the shared objects, so the linker prefers them over the
# static archives that sit next to them in LIBDIR.
SHARED_DIR="$SYSROOT/usr/lib/$MULTIARCH-shared"
# The same, but without the C runtime — used by the host build, which
# links against the host's own glibc.
HOST_RT_DIR="$SYSROOT/usr/lib/$MULTIARCH-runtime"

if [[ ! -d "$LIBDIR" ]]; then
    die "No libraries at $LIBDIR — rebuild the library directory"
fi

# ── Link libraries ────────────────────────────────────────────────────────
# SDL2 is built with all of its backends enabled, so libSDL2.so has
# undefined references to ALSA, PulseAudio, Wayland, KMSDRM and friends
# even though only the X11 backend is ever used.  The linker has to
# resolve them, so the full dependency list is passed explicitly.
#
# The order matters: the X11 libraries come before xcb, and the backend
# libraries come before the ones they depend on.
LINK_LIBS=(
    # SDL2's optional backends
    asound pulse samplerate
    gbm wayland-client wayland-egl wayland-cursor
    decor-0 xkbcommon drm
    # X11
    X11 Xext Xrandr Xcursor Xi Xinerama
    Xfixes Xrender Xss xcb Xau Xdmcp xshmfence
    # Text rendering
    freetype png16 z brotlidec brotlicommon
    # GCC support library (the aarch64 atomics live here)
    atomic
    # Text shaping, needed by SDL2_ttf >= 2.20
    harfbuzz
)

# The loader must find the libraries at runtime.  An RPATH with an
# absolute path to the library directory keeps the binary
# self-contained without needing LD_LIBRARY_PATH.
#
# On aarch64 the cross runtime (libc, libgcc_s) lives under
# usr/<multiarch>/lib rather than usr/lib/<multiarch>, so that
# directory has to be on the path as well.
#
# The x86_64 build links against the HOST's C runtime, so the library
# directory's libc must be kept off the RPATH — otherwise the loader
# would pick the older one and the binary would fail with
# "GLIBC_2.39 not found".
if [[ "$ARCH" == "aarch64" ]]; then
    RPATH="$SHARED_DIR:$LIBDIR:$LIBDIR2:$SYSROOT/usr/lib/$MULTIARCH/pulseaudio:$SYSROOT/usr/$MULTIARCH/lib"
else
    RPATH="$HOST_RT_DIR:$SHARED_DIR"
fi

# The compiler has to find the headers in the library directory too.
export C_INCLUDE_PATH="$SYSROOT/usr/include:$SYSROOT/usr/$MULTIARCH/include${C_INCLUDE_PATH:+:$C_INCLUDE_PATH}"

# ── Environment ───────────────────────────────────────────────────────────
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_PATH="$LIBDIR/pkgconfig:$SYSROOT/usr/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# sdl2-sys 0.37 no longer honours SDL2_LIB_DIR; the include and library
# paths are supplied through RUSTFLAGS instead.
#
# The library search paths must appear BEFORE the `-l` flags that use
# them: GNU ld (and rust-lld) resolve `-lfoo` at the point where it is
# seen, so a `-L` that comes later is ignored.  Cargo appends
# `-C link-arg=` values at the very end of the link line — after the
# `-lSDL2` that sdl2-sys emits — so the search paths are injected
# through a linker wrapper script instead, which puts them first.
#
# NOTE: cargo splits RUSTFLAGS on whitespace, so "-L native=DIR"
# would become two arguments ("-L" and "native=DIR").  The paths are
# therefore passed as "-C link-arg=-L DIR", which has no space.
if [[ "$ARCH" == "aarch64" ]]; then
    # The cross build links against the library directory.
    RUSTFLAGS=(
        "-C link-arg=-L$SHARED_DIR"
        "-C link-arg=-L$LIBDIR"
        "-C link-arg=-L$LIBDIR2"
        "-C link-arg=-Wl,-rpath,$RPATH"
    )
else
    # The host build takes SDL2 and the X11 libraries from the library
    # directory but links against the host's C runtime, so the library
    # directory's libc stays off the search path.
    RUSTFLAGS=(
        "-C link-arg=-L$HOST_RT_DIR"
        "-C link-arg=-L$SHARED_DIR"
        "-C link-arg=-L$LIBDIR"
        "-C link-arg=-Wl,-rpath,$RPATH"
    )
fi

# The libraries are passed through the linker wrapper rather than
# RUSTFLAGS: GNU ld resolves `-lfoo` at the point where it appears, and
# cargo puts `-C link-arg=` values at the very end of the link line —
# after the `-lSDL2` that sdl2-sys emits.  The wrapper places them
# before, where they can resolve SDL2's own dependencies.
#
# The host build has no wrapper, so there the libraries go into
# RUSTFLAGS instead — cargo emits those right after the crate's own.
LINK_LIB_ARGS=""
for lib in "${LINK_LIBS[@]}"; do
    LINK_LIB_ARGS+=" -l$lib"
done

if [[ "$ARCH" != "aarch64" ]]; then
    # The host build links against the system toolchain, so the
    # libraries are named directly.
    for lib in "${LINK_LIBS[@]}"; do
        RUSTFLAGS+=("-l $lib")
    done
fi

if [[ "$ARCH" == "aarch64" ]]; then
    # Cross-compiling.  ld.lld (shipped with the Rust toolchain) is used
    # as the linker and the C runtime startup objects are passed
    # explicitly, so no cross binutils or cross gcc are needed on the
    # host.
    CROSS_LIBC="$SYSROOT/usr/$MULTIARCH/lib"
    # The libgcc version directory varies with the Debian suite, so
    # discover it.  Several versions can be present at once; pick the
    # newest one, which is the one that matches the compiler.
    CROSS_GCCLIB="$(find "$SYSROOT/usr/lib/gcc-cross/$MULTIARCH" \
        -maxdepth 1 -type d -name '[0-9]*' 2>/dev/null | sort -V | tail -1)"
    CROSS_GCCLIB="${CROSS_GCCLIB:-}"

    if [[ ! -f "$CROSS_LIBC/crt1.o" ]]; then
        die "aarch64 libc not found in $CROSS_LIBC — rebuild the sysroot"
    fi
    if [[ -z "$CROSS_GCCLIB" || ! -f "$CROSS_GCCLIB/crtbegin.o" ]]; then
        die "aarch64 libgcc not found under $SYSROOT — rebuild the sysroot"
    fi

    # ld.lld is the GNU-flavoured linker that ships with the Rust
    # toolchain.
    LINKER="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/ld.lld"
    [[ -x "$LINKER" ]] || die "ld.lld not found at $LINKER"

    # The C runtime startup objects and the sysroot flag also have to be
    # in front of the `-l` flags, so they go into the wrapper too.
    #
    # rust-lld is invoked through a wrapper script because cargo's
    # CARGO_TARGET_<TRIPLE>_LINKER may only name a program, not a
    # program plus arguments.
    #
    # crt1.o (not Scrt1.o) is the one for a dynamically linked
    # executable: Scrt1.o is the static-PIE variant, which produces a
    # binary with no INTERP segment and no dynamic loader.
    #
    # crtbeginS.o / crtendS.o are the position-independent variants;
    # the plain ones carry absolute relocations that a PIE cannot use.
    CRT_ARGS="$CROSS_LIBC/crt1.o $CROSS_LIBC/crti.o \
$CROSS_GCCLIB/crtbeginS.o $CROSS_GCCLIB/crtendS.o $CROSS_LIBC/crtn.o"

    # glibc 2.36 (Debian bookworm) does not export the aarch64 atomic
    # helpers (__aarch64_cas4_sync and friends) from any shared
    # library — they only exist in libgcc.a.  Linking that archive in
    # statically provides them.
    EXTRA_LDFLAGS="-L$CROSS_LIBC -L$CROSS_GCCLIB --sysroot=$SYSROOT"
    if [[ -f "$CROSS_GCCLIB/libgcc.a" ]]; then
        # Plain ld flags: the wrapper unwraps "-Wl," options, but these
        # are added by the script itself, so they are written directly.
        EXTRA_LDFLAGS="$EXTRA_LDFLAGS --whole-archive $CROSS_GCCLIB/libgcc.a --no-whole-archive"
    fi

    # The dynamic loader has to be named explicitly: rustc links with
    # -nodefaultlibs, which drops the usual ld-linux entry, and the
    # binary would then have no INTERP segment at all.
    LOADER="$CROSS_LIBC/ld-linux-aarch64.so.1"
    if [[ -f "$LOADER" ]]; then
        EXTRA_LDFLAGS="$EXTRA_LDFLAGS --dynamic-linker=$LOADER"
    fi

    RUSTFLAGS+=("-C link-self-contained=no")
else
    # The x86_64 build links against the HOST's C runtime but takes
    # SDL2 and the X11 libraries from the library directory.
    #
    # The host's glibc is newer than the one in the library directory,
    # so the binary is linked against the host's.  The library
    # directory's libc must therefore stay OFF the RPATH, otherwise
    # the loader would pick the older one and the binary would fail
    # with "GLIBC_2.39 not found".
    LINKER="${CC:-cc}"
    CRT_ARGS=""
    EXTRA_LDFLAGS=""
fi

WRAPPER="$TARGET_DIR/linker-$ARCH.sh"
mkdir -p "$TARGET_DIR"

# The cross build drives ld.lld directly, so the "-Wl," options that
# rustc emits have to be unwrapped.  The host build goes through the
# system compiler driver, which understands them as they are.
if [[ "$ARCH" == "aarch64" ]]; then
    UNWRAP_WL=1
    RPATH_ARG="-rpath $RPATH"
    # DT_RPATH is inherited by the libraries the executable loads,
    # whereas DT_RUNPATH is not.  The dependencies of libSDL2 (libXss
    # and friends) have to be found through the sysroot, so the
    # executable has to carry an RPATH rather than a RUNPATH.
    DTAGS_ARG="--disable-new-dtags"
else
    UNWRAP_WL=0
    RPATH_ARG="-Wl,-rpath,$RPATH"
    DTAGS_ARG="-Wl,--disable-new-dtags"
fi

cat > "$WRAPPER" <<WRAPPER_EOF
#!/usr/bin/env bash
# Generated by scripts/build.sh — do not edit.
set -euo pipefail
UNWRAP_WL=$UNWRAP_WL
args=()
for a in "\$@"; do
    if [[ "\$UNWRAP_WL" == "1" ]]; then
        case "\$a" in
            -Wl,*)
                # "-Wl,-rpath,/x" becomes "-rpath /x".  Options that
                # carry several comma-separated values ("-z
                # relro,-z now") are split at every comma so each
                # becomes its own argument.
                rest="\${a#-Wl,}"
                IFS=',' read -ra parts <<< "\$rest"
                for p in "\${parts[@]}"; do
                    args+=("\$p")
                done
                ;;
            # rustc points at its bundled gcc-ld directory and asks for
            # the default libraries to be left out; both are meaningless
            # when the linker is driven directly.
            -B*/gcc-ld) ;;
            -nodefaultlibs) ;;
            *) args+=("\$a") ;;
        esac
    else
        args+=("\$a")
    fi
done
exec "$LINKER" \
    -L$SHARED_DIR \
    -L$LIBDIR \
    -L$LIBDIR2 \
    $EXTRA_LDFLAGS \
    $DTAGS_ARG \
    $CRT_ARGS \
    $LINK_LIB_ARGS \
    $RPATH_ARG \
    "\${args[@]}"
WRAPPER_EOF
chmod +x "$WRAPPER"

# Cargo's per-target linker variable names a PROGRAM, so it is used to
# select the wrapper.  The `-C linker=` flag alone is not enough:
# rustc's built-in default for the target wins over it.
TARGET_UPPER="$(echo "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
export "CARGO_TARGET_${TARGET_UPPER}_LINKER=$WRAPPER"

if [[ "$ARCH" == "aarch64" ]]; then
    # v4l2-sys generates its bindings with bindgen, which runs clang
    # against the C headers.  Point clang at the target's headers so it
    # does not pick up the host's (which are the wrong architecture).
    #
    # The target's libc headers live in the multiarch directory
    # (usr/aarch64-linux-gnu/include); the kernel headers are in
    # usr/include.
    export BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-unknown-linux-gnu --sysroot=$SYSROOT -I$SYSROOT/usr/include -I$SYSROOT/usr/$MULTIARCH/include"
fi

# ── Build ─────────────────────────────────────────────────────────────────
info "Building annotator for $TARGET..."
export RUSTFLAGS="${RUSTFLAGS[*]}"
cargo build --release --target "$TARGET" -j "$JOBS"

BIN="$TARGET_DIR/$TARGET/release/annotator"
[[ -f "$BIN" ]] || die "build did not produce $BIN"

mkdir -p "$TARGET_DIR/$ARCH"
cp -f "$BIN" "$TARGET_DIR/$ARCH/annotator"

info "Binary: $TARGET_DIR/$ARCH/annotator"
if command -v file >/dev/null 2>&1; then
    file "$TARGET_DIR/$ARCH/annotator" | sed 's/^/  /'
fi
