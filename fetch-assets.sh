#!/usr/bin/env bash
# Downloads required font assets into assets/
# Must be run from the repository root before first build.
set -euo pipefail

ASSETS="$(cd "$(dirname "$0")" && pwd)/assets"
mkdir -p "$ASSETS"

# ── helpers ───────────────────────────────────────────────────────────────────
dl() {
    local url="$1" dest="$2"
    if command -v curl &>/dev/null; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget &>/dev/null; then
        wget -qO "$dest" "$url"
    else
        echo "error: curl or wget required" >&2; exit 1
    fi
}

# ── DejaVuSans.ttf ────────────────────────────────────────────────────────────
if [[ ! -f "$ASSETS/DejaVuSans.ttf" ]]; then
    echo "Fetching DejaVuSans.ttf..."
    TMP=$(mktemp -d)
    dl "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2" \
       "$TMP/dejavu.tar.bz2"
    tar -xjf "$TMP/dejavu.tar.bz2" -C "$TMP" --strip-components=2 \
        dejavu-fonts-ttf-2.37/ttf/DejaVuSans.ttf
    mv "$TMP/DejaVuSans.ttf" "$ASSETS/DejaVuSans.ttf"
    rm -rf "$TMP"
    echo "  -> $ASSETS/DejaVuSans.ttf"
else
    echo "DejaVuSans.ttf already present, skipping."
fi

# ── fa-solid.otf (Font Awesome 6.7.2 Free Solid) ─────────────────────────────
if [[ ! -f "$ASSETS/fa-solid.otf" ]]; then
    echo "Fetching fa-solid.otf..."
    TMP=$(mktemp -d)
    dl "https://github.com/FortAwesome/Font-Awesome/releases/download/6.7.2/fontawesome-free-6.7.2-desktop.zip" \
       "$TMP/fa.zip"
    unzip -qj "$TMP/fa.zip" "fontawesome-free-6.7.2-desktop/otfs/Font Awesome 6 Free Solid.otf" \
          -d "$TMP"
    mv "$TMP/Font Awesome 6 Free Solid.otf" "$ASSETS/fa-solid.otf"
    rm -rf "$TMP"
    echo "  -> $ASSETS/fa-solid.otf"
else
    echo "fa-solid.otf already present, skipping."
fi

echo "Done."
