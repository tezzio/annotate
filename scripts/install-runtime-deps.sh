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
#
# Must run as root or via sudo.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
    x86_64|amd64)  ARCH=x86_64;  DEB_ARCH=amd64 ;;
    aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
    *) echo "ERROR: unsupported architecture '$ARCH' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac

# Runtime packages that provide the shared libraries the binary needs.
# These match the NEEDED entries in the released ELF binary.
PACKAGES=(
    libsdl2-2.0-0
    libsdl2-ttf-2.0-0
    libasound2
    libpulse0
    libsamplerate0
    libgbm1
    libwayland-client0
    libwayland-egl1
    libwayland-cursor0
    libdecor-0-0
    libxkbcommon0
    libdrm2
    libx11-6
    libxext6
    libxrandr2
    libxcursor1
    libxi6
    libxinerama1
    libxfixes3
    libxrender1
    libxss1
    libxcb1
    libxau6
    libxdmcp6
    libxshmfence1
    libfreetype6
    libpng16-16
    libbrotli1
    libharfbuzz0b
)

install_apt() {
    apt-get update
    apt-get install -y --no-install-recommends "${PACKAGES[@]/%/:$DEB_ARCH}"
}

if [ "$(id -u)" -eq 0 ]; then
    install_apt
else
    if command -v sudo >/dev/null 2>&1; then
        sudo apt-get update
        sudo apt-get install -y --no-install-recommends "${PACKAGES[@]/%/:$DEB_ARCH}"
    else
        echo "ERROR: this script needs root privileges. Run as root or install sudo." >&2
        exit 1
    fi
fi

echo "==> Runtime dependencies installed for $ARCH"
