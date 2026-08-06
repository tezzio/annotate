#!/usr/bin/env bash
# kill.sh — terminate all QEMU processes spawned by run.sh / run-minimal.sh

pkill -9 -f 'qemu-system-x86_64'  2>/dev/null || true
pkill -9 -f 'qemu-system-aarch64' 2>/dev/null || true
pkill -9 -f 'qemu-system-arm'     2>/dev/null || true
pkill -9 -f 'qemu-img'            2>/dev/null || true
info "QEMU processes terminated"
