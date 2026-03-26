#!/usr/bin/env bash
# bootstrap.sh — one-shot setup for fresh Ubuntu 25.10 (no Rust pre-installed)
set -e

echo "==> Installing system dependencies..."
sudo apt install -y build-essential pkg-config libsdl2-dev libsdl2-ttf-dev v4l-utils curl

if ! command -v rustc &>/dev/null; then
    echo "==> Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
else
    echo "==> Rust already installed: $(rustc --version)"
fi

echo "==> Building annotator (release)..."
cargo build --release

echo ""
echo "Done!  Run with:  ./target/release/annotator"
echo "For windowed dev mode set 'windowed = true' in config.toml before running."
