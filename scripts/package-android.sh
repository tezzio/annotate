#!/usr/bin/env bash
# package-android.sh — assemble the annotator's Android APK.
#
# The native code is built by scripts/build-android.sh; this script
# takes the resulting `libmain.so` and packages it into an APK with
# Gradle.
#
# The APK contains:
#
#   * `libmain.so` — the application, with SDL2 linked in statically.
#     SDL's Java shim loads it and calls `SDL_main` in it.
#   * `libhidapi.so` — SDL's HIDAPI support, which is a separate
#     shared library because its sources are C++.
#   * `org.libsdl.app.*` — SDL's Java shim, which implements the
#     Activity and the JNI bridge.
#
# Usage:
#   ./scripts/package-android.sh              # build a debug APK
#   ./scripts/package-android.sh --release    # build a release APK
#   ./scripts/package-android.sh --abi armeabi-v7a
#
# Environment:
#   ANDROID_HOME   the SDK directory        (default: ~/Library/Android/sdk)
#   ANDROID_NDK_HOME the NDK directory      (default: $ANDROID_HOME/ndk/<latest>)
#   GRADLE         the gradle command       (default: gradle, or the wrapper)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

PROJECT_DIR="$SCRIPT_DIR/.."
ANDROID_DIR="$PROJECT_DIR/android"

BUILD_TYPE="debug"
: "${API_LEVEL:=24}"
# The version comes from the crate, so the APK and the release assets
# are named the same way.
: "${VERSION:=$(grep -m1 '^version = ' "$PROJECT_DIR/Cargo.toml" | cut -d'"' -f2)}"

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --release      Build the release APK instead of the debug one
  --abi ABI[,ABI]
                 The Android ABI(s) to package (default: arm64-v8a).
                 Several ABIs may be given, separated by commas; they
                 are all packaged into a single APK.
  --api LEVEL    The minimum Android API level (default: 24)
  -h, --help     Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) BUILD_TYPE="release"; shift ;;
        --abi)     ABIS="$2"; shift 2 ;;
        --api)     API_LEVEL="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown argument: $1" ;;
    esac
done

: "${ANDROID_HOME:=$HOME/Library/Android/sdk}"
[[ -d "$ANDROID_HOME" ]] || die "the Android SDK was not found at $ANDROID_HOME"

# The NDK is needed for the C++ runtime, which is not part of the
# system image and has to be packaged with the application.
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -n "${ANDROID_NDK_HOME:-}" ]] || die "no NDK found under $ANDROID_HOME/ndk"

: "${ABIS:=arm64-v8a}"

# The Rust target triple for an ABI.  `common.sh` sets `TARGET` to the
# *host* triple, which on macOS is the Apple one, so it has to be
# overridden here.
target_for_abi() {
    case "$1" in
        arm64-v8a)   echo "aarch64-linux-android" ;;
        armeabi-v7a) echo "armv7-linux-androideabi" ;;
        x86_64)      echo "x86_64-linux-android" ;;
        x86)         echo "i686-linux-android" ;;
        *)           die "unknown ABI: $1" ;;
    esac
}

# The sysroot's ABI directory is named after the Android ABI.
sysroot_dir_for_abi() {
    case "$1" in
        arm64-v8a)   echo "aarch64-linux-android" ;;
        armeabi-v7a) echo "arm-linux-androideabi" ;;
        x86_64)      echo "x86_64-linux-android" ;;
        x86)         echo "i686-linux-android" ;;
        *)           echo "" ;;
    esac
}

# Strip the debug symbols from a library.
#
# The NDK's `llvm-strip` is used, because the macOS `strip` does not
# understand the Android binaries.
strip_library() {
    local lib="$1"
    [[ -f "$lib" ]] || return 0

    local stripper=""
    for host_tag in darwin-x86_64 darwin-arm64 linux-x86_64; do
        local candidate="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host_tag/bin/llvm-strip"
        [[ -x "$candidate" ]] || continue
        stripper="$candidate"
        break
    done
    if [[ -z "$stripper" ]]; then
        warn "no llvm-strip was found in the NDK — the library keeps its symbols"
        return 0
    fi
    "$stripper" --strip-unneeded "$lib" 2>/dev/null || {
        warn "could not strip $lib"
    }
}

# ── The native libraries ─────────────────────────────────────────────────────
#
# Every ABI's libraries are collected into `jniLibs/<abi>/`, and the
# Gradle build packages all of them into one APK.  Android picks the
# right one at install time, so a single APK runs on every device.
#
IFS=',' read -r -a ABI_LIST <<< "$ABIS"
[[ ${#ABI_LIST[@]} -gt 0 ]] || die "no ABI was given"

# The APK's native library directory.
JNI_LIBS_ROOT="$ANDROID_DIR/app/src/main/jniLibs"

for ABI in "${ABI_LIST[@]}"; do
    [[ -n "$ABI" ]] || continue
    TARGET="$(target_for_abi "$ABI")"

    # The build script leaves the library here.
    LIBS="$TARGET_DIR/android/$ABI"
    LIB_MAIN="$LIBS/libmain.so"
    [[ -f "$LIB_MAIN" ]] || die "no libmain.so in $LIBS — run: make build-android"

    JNI_LIBS="$JNI_LIBS_ROOT/$ABI"
    mkdir -p "$JNI_LIBS"
    cp "$LIB_MAIN" "$JNI_LIBS/libmain.so"

    # SDL2 itself.
    #
    # SDL2 is a separate shared library on Android because its JNI
    # entry points — the ones the Java shim calls, such as
    # `nativeGetVersion` — live in SDL2, and the shim reaches them by
    # loading `libSDL2.so` and looking the symbols up in it.
    SDL2_SO="$(find "$TARGET_DIR/$TARGET/release/build" -name "libSDL2.so" 2>/dev/null | head -1)"
    if [[ -n "$SDL2_SO" ]]; then
        cp "$SDL2_SO" "$JNI_LIBS/libSDL2.so"
        # The debug symbols are of no use in a shipped library, and they
        # are three quarters of its size.  The APK carries four
        # architectures, so every library is packaged four times.
        strip_library "$JNI_LIBS/libSDL2.so"
        info "Packaging libSDL2.so for $ABI"
    else
        die "no libSDL2.so was built for $ABI"
    fi

    # The C++ runtime.
    #
    # The HIDAPI sources are C++, so the application needs the shared
    # C++ runtime.  The NDK ships it, but it is not part of the system
    # image, so it has to be packaged with the application.
    #
    # It lives under a directory named after the *host* the NDK was
    # built for — `darwin-x86_64`, `linux-x86_64` and so on — so the
    # directory is searched rather than assumed.
    if [[ -n "${ANDROID_NDK_HOME:-}" ]]; then
        abi_dir="$(sysroot_dir_for_abi "$ABI")"
        libcxx=""
        for prebuilt in "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt"/*; do
            [[ -n "$abi_dir" ]] || break
            candidate="$prebuilt/sysroot/usr/lib/$abi_dir/libc++_shared.so"
            [[ -f "$candidate" ]] || continue
            libcxx="$candidate"
            break
        done
        if [[ -n "$libcxx" ]]; then
            cp "$libcxx" "$JNI_LIBS/libc++_shared.so"
            info "Packaging libc++_shared.so for $ABI"
        else
            die "no libc++_shared.so was found for $ABI in the NDK"
        fi
    fi
done

# ── Gradle ───────────────────────────────────────────────────────────────────

# The wrapper is not checked in — it would add a binary to the tree —
# so a Gradle on the PATH is used, or the one the build downloaded.
GRADLE_CMD="${GRADLE:-}"
if [[ -z "$GRADLE_CMD" ]]; then
    if command -v gradle >/dev/null 2>&1; then
        GRADLE_CMD="gradle"
    elif [[ -x "/tmp/gradledist/gradle-8.13/bin/gradle" ]]; then
        GRADLE_CMD="/tmp/gradledist/gradle-8.13/bin/gradle"
    else
        die "gradle was not found — install it, or set GRADLE to its path"
    fi
fi

# Gradle needs a JDK.  The Android Gradle Plugin wants Java 17, and
# Gradle itself cannot parse the version number of a newer runtime —
# Android Studio's bundled JDK 25, for instance, is rejected outright
# with "IllegalArgumentException: 25.0.4".
#
# So the candidates are tried in order of preference: an explicit
# JAVA_HOME, then the system JDK, then the Homebrew and Android Studio
# copies.
if [[ -z "${JAVA_HOME:-}" ]]; then
    for candidate in \
        "/usr/lib/jvm/java-17-openjdk-amd64" \
        "/usr/lib/jvm/java-17-openjdk" \
        "/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home" \
        "/opt/homebrew/opt/openjdk/libexec/openjdk.jdk/Contents/Home" \
        "/Applications/Android Studio.app/Contents/jbr/Contents/Home"; do
        if [[ -x "$candidate/bin/java" ]]; then
            export JAVA_HOME="$candidate"
            break
        fi
    done
fi
[[ -n "${JAVA_HOME:-}" ]] || die "JAVA_HOME is not set and no usable JDK was found"

export ANDROID_HOME
export ANDROID_SDK_ROOT="$ANDROID_HOME"

# Gradle packages every ABI whose libraries are in `jniLibs`.  The
# list is passed explicitly so that a stale directory from an earlier
# build cannot leak into the APK.
ABI_FILTER="$(IFS=,; echo "${ABI_LIST[*]}")"
export ANNOTATOR_ANDROID_ABIS="$ABI_FILTER"

TASK="assembleDebug"
[[ "$BUILD_TYPE" == "release" ]] && TASK="assembleRelease"

info "Building the $BUILD_TYPE APK for $ABI_FILTER..."
cd "$ANDROID_DIR"
"$GRADLE_CMD" --no-daemon ":app:$TASK"

# ── The result ───────────────────────────────────────────────────────────────

APK_DIR="$ANDROID_DIR/app/build/outputs/apk"
APK="$(find "$APK_DIR" -name "*.apk" 2>/dev/null | head -1)"
[[ -n "$APK" ]] || die "the build did not produce an APK"

# The APK is copied next to the other build artefacts so that the
# release script can find it.  A single APK carries every ABI, so the
# name lists them all.
mkdir -p "$TARGET_DIR/android"
OUT="$TARGET_DIR/android/annotator-$VERSION-$ABI_FILTER.apk"
cp "$APK" "$OUT"

info "APK: $OUT"
if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$OUT" > "$OUT.sha256"
    info "Checksum: $(cat "$OUT.sha256")"
fi
