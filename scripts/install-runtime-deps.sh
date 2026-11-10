#!/usr/bin/env bash
# install-runtime-deps.sh — install the system packages needed to RUN the
# annotator binary on a Debian/Ubuntu host.
#
# This is NOT for building — for that use `make libs` / `make libs-arm`.
# This installs the runtime shared libraries the released binary links
# against, so the application starts on a bare machine.
#
# Usage:
#   ./scripts/install-runtime-deps.sh          # detect host architecture
#   ARCH=aarch64 ./scripts/install-runtime-deps.sh
#   ./scripts/install-runtime-deps.sh --dry-run
#
# Must run as root or via sudo.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DRY_RUN=0
ARCH="${ARCH:-$(uname -m)}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --arch)    ARCH="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *) echo "ERROR: unknown argument '$1'" >&2; exit 1 ;;
    esac
done

case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  DEB_ARCH=amd64 ;;
    aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
    *) echo "ERROR: unsupported architecture '$ARCH' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac

# Runtime packages that provide the shared libraries the binary needs.
# These match the NEEDED entries in the released ELF binary.
# Each entry is "logical-name candidate1 candidate2 ..." so the script can
# pick the concrete package that exists on the current distro (e.g. Debian
# bookworm has libasound2, Ubuntu 25.10 has libasound2t64).
PACKAGES=(
    "libsdl2-2.0-0"
    "libsdl2-ttf-2.0-0"
    "libasound2 libasound2t64"
    "libpulse0"
    "libsamplerate0"
    "libgbm1"
    "libwayland-client0"
    "libwayland-egl1"
    "libwayland-cursor0"
    "libdecor-0-0"
    "libxkbcommon0"
    "libdrm2"
    "libx11-6"
    "libxext6"
    "libxrandr2"
    "libxcursor1"
    "libxi6"
    "libxinerama1"
    "libxfixes3"
    "libxrender1"
    "libxss1"
    "libxcb1"
    "libxau6"
    "libxdmcp6"
    "libxshmfence1"
    "libfreetype6"
    "libpng16-16 libpng16-16t64"
    "libssl3 libssl3t64"
    "libbrotli1 libbrotli1t64"
    "libharfbuzz0b libharfbuzz0b64"
)

# Resolve a logical package to a concrete, installable package name.
# Tries the listed candidates in order; if none is concrete, picks the
# first provider listed by apt for the first candidate.
resolve_package() {
    local candidates=("$@")
    for c in "${candidates[@]}"; do
        # Package exists and has a Version: line -> concrete, installable.
        if apt-cache show "$c" >/dev/null 2>&1 && \
           apt-cache show "$c" 2>/dev/null | grep -q '^Version:'; then
            echo "$c"
            return 0
        fi
    done

    # No concrete candidate found; the first name is probably virtual.
    # Ask apt for its concrete providers and pick the first one.
    local first="${candidates[0]}"
    local provider
    provider="$(apt-cache showpkg "$first" 2>/dev/null | awk '/^Reverse Provides:/{flag=1; next} flag{print $1; exit}')"
    if [ -n "$provider" ]; then
        echo "$provider"
        return 0
    fi

    # Nothing worked — return the first candidate so apt can fail visibly.
    echo "$first"
    return 1
}

# Build the list of concrete package names in RESOLVED.
RESOLVED=()
FAILED=()
for entry in "${PACKAGES[@]}"; do
    # $entry is a space-separated list of candidate names.
    candidates=($entry)
    if pkg="$(resolve_package "${candidates[@]}")"; then
        RESOLVED+=("$pkg")
    else
        FAILED+=("$pkg")
        RESOLVED+=("$pkg")
    fi
done

if [ "${#FAILED[@]}" -gt 0 ]; then
    echo "WARN: could not resolve concrete package for: ${FAILED[*]}" >&2
    echo "      apt will probably fail; install the equivalent distro package manually." >&2
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "==> Would install the following packages for $ARCH:"
    printf '  %s\n' "${RESOLVED[@]/%/:$DEB_ARCH}"
    exit 0
fi

install_apt() {
    local pkgs=("$@")
    apt-get update
    apt-get install -y --no-install-recommends "${pkgs[@]/%/:$DEB_ARCH}"
}

if [ "$(id -u)" -eq 0 ]; then
    install_apt "${RESOLVED[@]}"
else
    if command -v sudo >/dev/null 2>&1; then
        sudo apt-get update
        sudo apt-get install -y --no-install-recommends "${RESOLVED[@]/%/:$DEB_ARCH}"
    else
        echo "ERROR: this script needs root privileges. Run as root or install sudo." >&2
        exit 1
    fi
fi

echo "==> Runtime dependencies installed for $ARCH"
