#!/usr/bin/env bash
# build-android.sh — cross-compile the annotator for Android.
#
# Android needs a different toolchain from every other target:
#
#   * The NDK provides the C compiler, the linker and the sysroot.
#   * SDL2 is built from source against the NDK — there is no system
#     SDL2 to link against, and the Homebrew copy is for the wrong
#     platform.
#   * The result is a shared library, `libmain.so`, not an executable:
#     SDL loads it and calls `SDL_main` in it.
#
# Usage:
#   ./scripts/build-android.sh                  # build the library
#   ./scripts/build-android.sh --apk            # build the APK as well
#   ./scripts/build-android.sh --abi armeabi-v7a
#
# Environment:
#   ANDROID_HOME    the SDK directory       (default: ~/Library/Android/sdk)
#   ANDROID_NDK_HOME the NDK directory      (default: $ANDROID_HOME/ndk/<latest>)
#   TARGET          the Rust target triple  (default: aarch64-linux-android)
#   API_LEVEL       the minimum Android API  (default: 24)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

BUILD_APK=0
: "${API_LEVEL:=24}"

# The arguments as they were given, so that a build for several ABIs
# can be run once per ABI with the same options.
ORIGINAL_ARGS=("$@")

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --apk            Also build the APK with Gradle
  --abi ABI[,ABI]  The Android ABI(s) to build (default: arm64-v8a).
                   Several ABIs may be given, separated by commas.
  --api LEVEL      The minimum Android API level (default: 24)
  -h, --help       Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apk)  BUILD_APK=1; shift ;;
        --api)  API_LEVEL="$2"; shift 2 ;;
        --abi)  ABIS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

# ── The toolchain ────────────────────────────────────────────────────────────

: "${ANDROID_HOME:=$HOME/Library/Android/sdk}"
if [[ ! -d "$ANDROID_HOME" ]]; then
    die "the Android SDK was not found at $ANDROID_HOME — set ANDROID_HOME"
fi

# The NDK is versioned, so the newest one is used unless the caller
# names a specific directory.
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
if [[ -z "$ANDROID_NDK_HOME" || ! -d "$ANDROID_NDK_HOME" ]]; then
    die "no NDK found under $ANDROID_HOME/ndk — install one with the SDK manager"
fi

# The ABIs to build.  Several may be given, separated by commas.
: "${ABIS:=arm64-v8a}"

# The Rust target triple for an ABI.
target_for_abi() {
    case "$1" in
        arm64-v8a)   echo "aarch64-linux-android" ;;
        armeabi-v7a) echo "armv7-linux-androideabi" ;;
        x86_64)      echo "x86_64-linux-android" ;;
        x86)         echo "i686-linux-android" ;;
        *) die "Unsupported ABI '$1'" ;;
    esac
}

# Everything below builds one ABI at a time, because the NDK's
# toolchain, SDL2's cmake build and the linker wrapper are all
# configured for a single ABI.
#
# A request for several ABIs is therefore handled by running this
# script once per ABI.  Each run uses its own build directories, so
# they do not interfere with one another.
if [[ "$ABIS" == *,* ]]; then
    IFS=',' read -r -a abi_list <<< "$ABIS"
    for abi in "${abi_list[@]}"; do
        [[ -n "$abi" ]] || continue
        info "Building for $abi..."
        # The child's own output is shown, because a failure here is
        # usually in the compiler or the linker.
        bash "${BASH_SOURCE[0]}" "${ORIGINAL_ARGS[@]}" --abi "$abi" || {
            die "the build for $abi failed"
        }
    done
    exit 0
fi

ABI="$ABIS"
TARGET="$(target_for_abi "$ABI")"

# The NDK ships one clang wrapper per API level and ABI.  They are
# what the CMake toolchain file looks for by name, so they have to be
# on PATH.
#
# The wrappers live under a directory named after the *host* the NDK
# was built for — `darwin-x86_64`, `darwin-arm64`, `linux-x86_64` —
# so the directory has to be found rather than assumed.  The CI
# runners are Linux, and the NDK they have is `linux-x86_64`.
NDK_PREBUILT="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt"
if [[ ! -d "$NDK_PREBUILT" ]]; then
    die "the NDK's prebuilt toolchains were not found at $NDK_PREBUILT"
fi
HOST_TAG=""
# Prefer the host's own architecture: a machine that can run
# `linux-x86_64` may also have an `linux-arm64` directory.
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)        prefer="darwin-arm64" ;;
    Darwin-x86_64)       prefer="darwin-x86_64" ;;
    Linux-aarch64|Linux-arm64) prefer="linux-arm64" ;;
    *)                   prefer="linux-x86_64" ;;
esac

# The exact match for this host comes first.
if [[ -x "$NDK_PREBUILT/$prefer/bin/clang" ]]; then
    HOST_TAG="$prefer"
fi

# Otherwise, take the first directory that has a working compiler.
if [[ -z "$HOST_TAG" ]]; then
    for tag in "$NDK_PREBUILT"/*; do
        [[ -x "$tag/bin/clang" ]] || continue
        HOST_TAG="$(basename "$tag")"
        break
    done
fi
[[ -n "$HOST_TAG" ]] || die "no usable NDK toolchain was found under $NDK_PREBUILT"

NDK_BIN="$NDK_PREBUILT/$HOST_TAG/bin"
if [[ ! -d "$NDK_BIN" ]]; then
    die "the NDK toolchain was not found at $NDK_BIN"
fi
info "NDK toolchain: $HOST_TAG"
export PATH="$NDK_BIN:$PATH"

export ANDROID_NDK_HOME
export ANDROID_HOME

# The ABI and the API level have to reach the cmake toolchain file.
#
# Without ANDROID_ABI the NDK's toolchain file falls back to
# `armeabi-v7a`, so SDL2 is built as 32-bit ARM even for an arm64
# target — and the application then fails to link against it with
# "is incompatible with aarch64linux".
#
# The value has to arrive as a *cmake variable*, not as an environment
# variable: the toolchain file reads `ANDROID_ABI`, and cmake only
# falls back to the environment for variables it was not given on the
# command line — which it does not do here.
#
# `sdl2-sys` passes `-DANDROID_NDK` but not the ABI, and there is no
# hook for extra cmake arguments.  So a small wrapper toolchain file
# is generated that sets the ABI and then includes the NDK's own.
mkdir -p "$TARGET_DIR/android"
TOOLCHAIN="$TARGET_DIR/android/android-toolchain-$ABI.cmake"
cat > "$TOOLCHAIN" <<EOF
# Generated by scripts/build-android.sh — do not edit.
#
# The NDK's toolchain file picks the ABI from ANDROID_ABI, which has
# to be set as a cmake variable.  This wrapper sets it and then defers
# to the real one.
set(ANDROID_ABI "$ABI")
set(ANDROID_PLATFORM "android-$API_LEVEL")
include("$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake")
EOF

export CMAKE_TOOLCHAIN_FILE="$TOOLCHAIN"
# SDL2's CMakeLists asks for a minimum CMake version that CMake 4 no
# longer accepts.  The policy escape hatch keeps the old behaviour.
export CMAKE_POLICY_VERSION_MINIMUM=3.5

# The Rust standard library for the target.
if ! rustup target list --installed | grep -qx "$TARGET"; then
    info "Installing the Rust target $TARGET..."
    rustup target add "$TARGET"
fi

# The SDL2 sources are fetched into the cargo registry by cargo
# itself, so they have to be there before they can be patched.  On a
# machine where the crate is already cached they are, and the patch
# below is a no-op; on a fresh one they are not, which is why the
# fetch comes first.
info "Fetching the crates..."
cargo fetch --target "$TARGET" 2>/dev/null || true

# ── Patch SDL2 for a modern NDK ──────────────────────────────────────────────
#
# SDL 2.0.26 calls `ALooper_pollAll`, which the NDK marks unavailable
# from API level 24 onwards.  The NDK's own advice is to use
# `ALooper_pollOnce` instead; the two have identical signatures, and
# for the sensor queue SDL polls with a zero timeout, so the
# substitution is behaviour-preserving.
#
# The SDL2 sources live inside the `sdl2-sys` crate in the cargo
# registry, and that crate builds them itself, so there is no hook to
# patch a copy.  The file is patched in place instead.  The change is
# idempotent — it looks for the symbol before replacing it — so it is
# safe to apply to the shared registry cache: other projects using the
# same SDL2 version simply find the fix already in place.
patch_sdl2() {
    local patched=0
    local registry="${CARGO_HOME:-$HOME/.cargo}/registry/src"
    [[ -d "$registry" ]] || return 0

    while IFS= read -r sensor; do
        if grep -q "ALooper_pollAll" "$sensor"; then
            # `sed -i` takes the backup suffix as an argument on macOS
            # but not on Linux, so the file is rewritten instead of
            # edited in place: the result is written to a temporary
            # file and moved over the original.
            if sed 's/ALooper_pollAll/ALooper_pollOnce/g' "$sensor" > "$sensor.new" 2>/dev/null \
                && mv "$sensor.new" "$sensor" 2>/dev/null; then
                info "Patched SDL2 for the Android NDK: $(basename "$(dirname "$(dirname "$(dirname "$(dirname "$sensor")")")")")"
                patched=1
            else
                rm -f "$sensor.new" 2>/dev/null
                warn "could not patch $sensor — the registry may be read-only"
            fi
        else
            info "SDL2 is already patched for the Android NDK"
            patched=1
        fi
    done < <(find "$registry" -path "*sdl2-sys-*/SDL/src/sensor/android/SDL_androidsensor.c" 2>/dev/null)

    [[ $patched -eq 1 ]] || warn "no SDL2 sources were found to patch"
}

# ── HIDAPI ───────────────────────────────────────────────────────────────────
#
# SDL2's Android build needs a separate `libhidapi.so`: the HIDAPI
# sources are C++ and are built with ndk-build, not with the CMake
# build that the rest of SDL2 uses.  The `sdl2-sys` crate links against
# it unconditionally on Android.
#
# SDL2 ships the ndk-build project for it, so the library is built from
# the same sources SDL itself would use.
build_hidapi() {
    # The SDL2 version the lockfile uses, so the HIDAPI sources match
    # the SDL2 they are linked into.
    local sdl_version
    sdl_version="$(grep -A1 'name = "sdl2-sys"' "$PROJECT_DIR/Cargo.lock" \
        | grep version | head -1 | cut -d'"' -f2)"
    [[ -n "$sdl_version" ]] || die "cannot determine the sdl2-sys version"

    local sdl_root
    sdl_root="$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" \
        -maxdepth 3 -type d -path "*sdl2-sys-$sdl_version/SDL" 2>/dev/null | head -1)"
    if [[ -z "$sdl_root" ]]; then
        die "no SDL2 $sdl_version sources were found — cannot build libhidapi"
    fi

    local jni="$sdl_root/src/hidapi/android/jni"
    [[ -f "$jni/Android.mk" ]] || die "no HIDAPI ndk-build project found"

    local ndk_build="$ANDROID_NDK_HOME/ndk-build"
    [[ -x "$ndk_build" ]] || die "ndk-build was not found in the NDK"

    # The HIDAPI sources include SDL_internal.h, which includes the
    # generated SDL_config.h.  The CMake build produced it; point the
    # compiler at it.
    local config_inc
    config_inc="$(find "$TARGET_DIR/$TARGET/release/build" -type d \
        -name "include-config-release" 2>/dev/null | head -1)"
    if [[ -z "$config_inc" ]]; then
        die "no generated SDL_config.h was found — run the cargo build first"
    fi

    info "Building libhidapi.so for $ABI..."
    # The Application.mk in SDL's tree pins a single 32-bit ABI, so it
    # is overridden with the one being built.
    #
    # APP_CFLAGS is where the include paths go: the HIDAPI sources
    # include SDL_internal.h, which includes the generated
    # SDL_config.h.  `NDK_CFLAGS` is not a thing — ndk-build only
    # reads the variables from Application.mk.
    #
    # The HIDAPI library is built as a *static* library rather than the
    # shared one SDL's own project produces.
    #
    # The shared library would have to resolve SDL2's string helpers
    # at its own link time, but SDL2 in turn needs HIDAPI's symbols —
    # a circular dependency that cannot be satisfied.  A static
    # library sidesteps it: both are linked into the application
    # library, and the linker resolves the cycle there.
    #
    # SDL's own Android.mk lives in the cargo registry and is
    # read-only in a clean checkout, so the project is written into the
    # build directory instead.
    local project="$TARGET_DIR/android/hidapi/$ABI"
    rm -rf "$project"
    mkdir -p "$project"

    cat > "$project/Android.mk" <<EOF
LOCAL_PATH := $(call my-dir)

include \$(CLEAR_VARS)
LOCAL_CPPFLAGS := -std=c++11 -I$config_inc -I$sdl_root/include
LOCAL_SRC_FILES := $sdl_root/src/hidapi/android/hid.cpp
LOCAL_MODULE := hidapi
LOCAL_LDLIBS := -llog
include \$(BUILD_STATIC_LIBRARY)
EOF

    cat > "$project/Application.mk" <<EOF
APP_STL := c++_static
APP_ABI := $ABI
APP_PLATFORM := android-$API_LEVEL
EOF

    if "$ndk_build" NDK_PROJECT_PATH="$project" APP_BUILD_SCRIPT="$project/Android.mk" \
        NDK_APPLICATION_MK="$project/Application.mk" \
        NDK_OUT="$TARGET_DIR/android/obj/$ABI" NDK_LIBS_OUT="$TARGET_DIR/android/ndk-libs/$ABI" \
        2>&1 | tail -15; then
        # ndk-build puts a static library under its object directory
        # rather than the libraries directory.
        local built="$TARGET_DIR/android/obj/$ABI/local/$ABI/libhidapi.a"
        if [[ ! -f "$built" ]]; then
            built="$(find "$TARGET_DIR/android/obj/$ABI" -name "libhidapi.a" 2>/dev/null | head -1)"
        fi
        if [[ -n "$built" && -f "$built" ]]; then
            # The linker looks for `libhidapi`; a copy named that makes
            # the -lhidapi in the link line resolve.
            mkdir -p "$TARGET_DIR/android/lib/$ABI"
            cp "$built" "$TARGET_DIR/android/lib/$ABI/libhidapi.a"
            info "HIDAPI: $built"
        else
            die "ndk-build succeeded but produced no libhidapi.a"
        fi
    else
        die "could not build libhidapi"
    fi
}

# ── Build the shared library ────────────────────────────────────────────────
#
# The order matters: the cargo build has to run first, because it is
# what produces the generated SDL_config.h that the HIDAPI build
# includes.  The HIDAPI library is then built and the application is
# linked again against it.


info "Building the annotator for $TARGET (API $API_LEVEL)..."
mkdir -p "$TARGET_DIR/android"

# The `sdl2-sys` build script copies the SDL2 libraries it built into
# the release directory and into `deps/`, so that a test binary can find
# them at run time.  On a fresh build `deps/` does not exist yet — it
# is created by cargo when it compiles the crate — and the copy fails
# with "Failed to copy SDL2 dynamic library".  Creating it here avoids
# that.
mkdir -p "$TARGET_DIR/$TARGET/release/deps" "$TARGET_DIR/$TARGET/release"

# The NDK's compiler wrappers are named after the architecture, but
# not always the same way the Rust target triple names it: the 32-bit
# ARM target is `armv7-linux-androideabi` in Rust and
# `armv7a-linux-androideabi` in the NDK.
ndk_arch_for_abi() {
    case "$1" in
        arm64-v8a)   echo "aarch64-linux-android" ;;
        armeabi-v7a) echo "armv7a-linux-androideabi" ;;
        x86_64)      echo "x86_64-linux-android" ;;
        x86)         echo "i686-linux-android" ;;
        *)           die "Unsupported ABI '$1'" ;;
    esac
}

NDK_ARCH="$(ndk_arch_for_abi "$ABI")"
CARGO_TARGET_LINKER="$NDK_BIN/${NDK_ARCH}${API_LEVEL}-clang"
if [[ ! -x "$CARGO_TARGET_LINKER" ]]; then
    # The NDK's wrappers are named after the API level; find the
    # closest one at or below the requested level.
    CARGO_TARGET_LINKER="$(ls "$NDK_BIN/${NDK_ARCH}"*-clang 2>/dev/null | sort -V | tail -1)"
fi
[[ -x "$CARGO_TARGET_LINKER" ]] || die "no Android linker found for $TARGET in $NDK_BIN"
info "Linker: $(basename "$CARGO_TARGET_LINKER")"

# Cargo names the per-target linker after the target triple with the
# dashes turned into underscores.
TARGET_UPPER="$(echo "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
export "CARGO_TARGET_${TARGET_UPPER}_LINKER=$CARGO_TARGET_LINKER"

# ── The linker wrapper ───────────────────────────────────────────────────────
#
# Two things have to be added to the link line that cargo does not
# provide:
#
#   * The shared C++ runtime.  SDL2's Android support is written in C++
#     (`SDL_android.c` is compiled as C++), so the application needs
#     libc++.  Naming it is what puts it in the library's NEEDED list:
#     the loader only loads a shared library that is actually needed,
#     so leaving it out means the symbols are missing at run time and
#     the application fails with "cannot locate symbol
#     __gxx_personality_v0".
#
#   * SDL2_ttf and the freetype and harfbuzz it needs.  The
#     `sdl2-sys` crate only emits a link directive for SDL2_ttf on
#     Linux, Windows and macOS — on Android it emits nothing, so the
#     library is never linked and the text rendering symbols are
#     missing.
#
# The static archives have to come *after* the objects that need them:
# a linker resolves an archive in the order it is given, and an
# archive that comes first contributes nothing.  Cargo appends
# `-C link-arg` values at the end of the link line, which is where they
# belong — but they are not applied until cargo notices that the flags
# changed, which is why the first build pass is allowed to fail.
#
# The wrapper puts the archives at the very end, after everything
# else, which is both correct and independent of cargo's caching.
WRAPPER="$TARGET_DIR/android/linker-wrapper-$ABI.sh"
mkdir -p "$TARGET_DIR/android"
cat > "$WRAPPER" <<'WRAPPER_EOF'
#!/usr/bin/env bash
# Generated by scripts/build-android.sh — do not edit.
#
# The SDL2_ttf archive and the freetype and harfbuzz archives it needs
# are built after the first cargo pass, so they are not there yet when
# that pass runs.
#
# The link has to *fail* when they are missing.  A shared library may
# have undefined symbols — the linker does not object to them — so a
# link without the archives succeeds and produces a library that fails
# to load at run time with "cannot locate symbol
# TTF_RenderText_Blended".  Cargo would then consider the artifact up
# to date and never link it again.
#
# Failing loudly makes the first pass fail, which is what it is
# expected to do; the second pass, after the archives have been built,
# is the one that links.
for lib in SDL2_ttf freetype harfbuzz; do
    if [[ ! -f "$LIB_DIR/lib$lib.a" ]]; then
        echo "linker wrapper: $LIB_DIR/lib$lib.a is missing" >&2
        exit 1
    fi
done

exec "${LINKER}" \
    -L$LIB_DIR \
    "$@" \
    -lSDL2_ttf \
    -lfreetype \
    -lharfbuzz \
    -llog \
    -lc++_shared
WRAPPER_EOF

# The wrapper needs to know where the libraries are and which linker
# to call; the environment carries both.  The archives are per-ABI —
# they are built for one architecture and cannot be used for another.
LIB_DIR="$TARGET_DIR/android/lib/$ABI"
LINKER="$CARGO_TARGET_LINKER"
export LIB_DIR LINKER
chmod +x "$WRAPPER"

export LINKER="$CARGO_TARGET_LINKER"
TARGET_UPPER="$(echo "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
export "CARGO_TARGET_${TARGET_UPPER}_LINKER=$WRAPPER"


# The Android build produces a shared library, not an executable, so
# the `desktop` feature — which gates the binary target — is off.
#
# The SDL2 sources have to be patched before they are compiled.  The
# patch is applied here rather than earlier because the sources are
# only in the registry once cargo has fetched them, and the fetch
# above may not have covered them if the lockfile changed.
patch_sdl2

#
# The first build produces SDL2 and the generated SDL_config.h but
# fails to link, because `libhidapi` does not exist yet.  That is
# expected; the second build, after the HIDAPI library has been built,
# is the one that links.
info "Building SDL2 and the application objects..."
cargo build --release --target "$TARGET" --no-default-features --features android || true

# ── SDL2_ttf ─────────────────────────────────────────────────────────────────
#
# The application renders its text with SDL2_ttf, and the `ttf` feature
# makes the `sdl2-sys` crate link against it — but SDL2_ttf is a
# separate project and is not built by SDL2's own cmake build.  It has
# to be built here.
#
# The sources are fetched rather than vendored: they are several
# megabytes, and the version is pinned here so that the build is
# reproducible.  The freetype and harfbuzz copies SDL2_ttf ships with
# are used, so no other libraries have to be found.
build_sdl2_ttf() {
    local version="${SDL2_TTF_VERSION:-2.24.0}"
    local src="$TARGET_DIR/android/SDL2_ttf-$version"
    # The build directory is per-ABI: the archives it produces are for
    # one architecture only.
    local out="$TARGET_DIR/android/sdl2-ttf-build/$ABI"

    if [[ ! -d "$src" ]]; then
        info "Fetching SDL2_ttf $version..."
        local tarball="$TARGET_DIR/android/SDL2_ttf-$version.tar.gz"
        local url="https://github.com/libsdl-org/SDL_ttf/releases/download/release-$version/SDL2_ttf-$version.tar.gz"
        if ! curl -sL --max-time 300 -o "$tarball" "$url"; then
            die "could not download SDL2_ttf"
        fi
        tar -xzf "$tarball" -C "$TARGET_DIR/android" || {
            die "could not unpack SDL2_ttf"
        }
    fi

    # SDL2_ttf has to be built against the SDL2 that was just built.
    #
    # SDL2 itself is a *shared* library on Android — its JNI entry
    # points have to be reachable from the Java shim, which loads
    # `libSDL2.so` by name.  SDL2_ttf is built as a static archive
    # against that shared library: the archive's own references to SDL2
    # are left undefined and are resolved by `libSDL2.so` when the
    # application library is linked, which is exactly the arrangement
    # the linker wrapper sets up.
    local sdl2_out
    sdl2_out="$(dirname "$(dirname "$(find "$TARGET_DIR/$TARGET/release/build" -name "libSDL2.so" 2>/dev/null | head -1)")")"
    [[ -d "$sdl2_out" ]] || die "no SDL2 was built for SDL2_ttf to use"
    [[ -f "$sdl2_out/lib/libSDL2.so" ]] || die "the SDL2 shared library is missing"

    rm -rf "$out"
    mkdir -p "$out"
    info "Building SDL2_ttf..."
    #
    # The output is not suppressed: when the build fails the reason is
    # in it, and a CI log that says only "could not build SDL2_ttf" is
    # no help at all.
    if (cd "$out" && cmake "$src" \
        -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
        -DCMAKE_TOOLCHAIN_FILE="$CMAKE_TOOLCHAIN_FILE" \
        -DANDROID_ABI="$ABI" \
        -DANDROID_PLATFORM="android-$API_LEVEL" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DSDL2TTF_SAMPLES=OFF \
        -DSDL2TTF_VENDORED=ON \
        -DSDL2TTF_HARFBUZZ=ON \
        -DSDL2_INCLUDE_DIR="$sdl2_out/include/SDL2" \
        -DSDL2_LIBRARY="$sdl2_out/lib/libSDL2.so" \
        -DSDL2_DIR="$sdl2_out" \
        && cmake --build . --target SDL2_ttf -j"$(nproc 2>/dev/null || echo 4)"); then
        mkdir -p "$TARGET_DIR/android/lib/$ABI"
        cp "$out/libSDL2_ttf.a" "$TARGET_DIR/android/lib/$ABI/libSDL2_ttf.a"
        info "SDL2_ttf: $TARGET_DIR/android/lib/$ABI/libSDL2_ttf.a"
    else
        die "could not build SDL2_ttf"
    fi

    # SDL2_ttf's archive needs freetype and harfbuzz, which it built
    # from the copies it ships with.  They have to be linked too, and
    # they are only needed when something in SDL2_ttf is actually
    # pulled in — so the archives are copied next to it and linked
    # after it.
    for lib in freetype harfbuzz; do
        local archive="$out/external/$lib/lib$lib.a"
        if [[ -f "$archive" ]]; then
            cp "$archive" "$TARGET_DIR/android/lib/$ABI/lib$lib.a"
        fi
    done
}

# ── HIDAPI, then link ───────────────────────────────────────────────────────
#
# The HIDAPI library needs the generated SDL_config.h, so it is built
# after the first pass.  The application is then linked against it.
build_hidapi
build_sdl2_ttf

info "Linking the annotator..."
#
# Cargo does not know that the linker wrapper changed, so it considers
# the library up to date and skips the link.  Removing the artifact
# makes cargo link it again, which is what picks up the SDL2_ttf and
# HIDAPI archives that were only just built.
rm -f "$TARGET_DIR/$TARGET/release/libannotator.so"
cargo build --release --target "$TARGET" --no-default-features --features android

LIB="$TARGET_DIR/$TARGET/release/libannotator.so"
[[ -f "$LIB" ]] || die "the build did not produce $LIB"

# The APK's Java shim loads the library as `libmain.so`, and the
# entry point inside it is `SDL_main`.
OUT_DIR="$TARGET_DIR/android/$ABI"
mkdir -p "$OUT_DIR"
cp "$LIB" "$OUT_DIR/libmain.so"
info "Library: $OUT_DIR/libmain.so"

# ── The APK ──────────────────────────────────────────────────────────────────

if [[ "$BUILD_APK" == "1" ]]; then
    info "Building the APK..."
    bash "$SCRIPT_DIR/package-android.sh" --abi "$ABI"
fi
