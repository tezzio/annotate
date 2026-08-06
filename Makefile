# Makefile for annotate build system.
#
# Usage:
#   make help              Show available targets
#   make bootstrap         Install host dependencies (+ rustup if needed)
#   make sysroot           Create $ARCH sysroot
#   make build             Build annotator binary
#   make initrd            Build minimal initrd (Xorg + annotator)
#   make iso               Build bootable ISO
#   make run               Run ISO in QEMU
#   make run-minimal       Run minimal initrd in QEMU
#   make kill              Stop all QEMU processes
#   make clean             Remove build artifacts
#
# Environment:
#   ARCH       x86_64 | aarch64 (default: x86_64)
#   TARGET     Cargo target triple (default: $ARCH-unknown-linux-gnu)
#   SYSROOT    Path to sysroot (default: target/$ARCH-sysroot)

.SILENT: help bootstrap sysroot build initrd iso run run-minimal kill clean

PROJECT_DIR := $(abspath .)
TARGET_DIR  := $(PROJECT_DIR)/target
SCRIPTS_DIR := $(PROJECT_DIR)/scripts
ARCH        ?= x86_64
TARGET      ?= $(ARCH)-unknown-linux-gnu

SYSROOT_DIR         := $(TARGET_DIR)/$(ARCH)-sysroot
INITRD_ROOT_DIR     := $(TARGET_DIR)/$(ARCH)-minimal-initrd-root
ISO_ROOT_DIR        := $(TARGET_DIR)/$(ARCH)-iso-root
MINIMAL_INITRD_IMG  := $(TARGET_DIR)/$(ARCH)-minimal-initrd.img
ISO_IMG             := $(TARGET_DIR)/$(ARCH)-sysroot.iso
ANNOTATOR_BIN       ?= $(TARGET_DIR)/$(TARGET)/release/annotator

export PROJECT_DIR TARGET_DIR ARCH TARGET SYSROOT

help:
	@echo "annotate build system"
	@echo ""
	@echo "Setup targets:"
	@echo "  bootstrap      Install system dependencies and Rust"
	@echo "  sysroot        Create $(ARCH) sysroot (Debian debootstrap)"
	@echo ""
	@echo "Build targets:"
	@echo "  build          Build annotator for $(TARGET)"
	@echo "  initrd         Build minimal initrd (Xorg + annotator)"
	@echo "  iso            Build bootable ISO"
	@echo ""
	@echo "Run targets:"
	@echo "  run            Boot ISO in QEMU"
	@echo "  run-minimal    Boot minimal initrd in QEMU"
	@echo "  kill           Stop all QEMU processes"
	@echo ""
	@echo "Clean targets:"
	@echo "  clean          Remove build artifacts (keeps cargo cache)"
	@echo "  clean-all      Remove all generated files including sysroot"
	@echo ""
	@echo "Variables:"
	@echo "  ARCH=$(ARCH)         Target architecture"
	@echo "  TARGET=$(TARGET)     Cargo target triple"
	@echo ""
	@echo "Examples:"
	@echo "  make bootstrap                    # install host deps"
	@echo "  make                              # sysroot + build + initrd"
	@echo "  ARCH=aarch64 make sysroot         # cross aarch64 sysroot (sudo once)"
	@echo "  ARCH=aarch64 make build           # cross-compile annotator"
	@echo "  ARCH=aarch64 make initrd          # aarch64 initrd"
	@echo "  ARCH=aarch64 make run-minimal     # QEMU raspi3b"

.PHONY: help bootstrap sysroot build initrd iso run run-minimal kill clean clean-all all

all: sysroot build initrd

bootstrap:
	@bash $(SCRIPTS_DIR)/bootstrap.sh

sysroot:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/sysroot.sh

build:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/cross.sh --target $(TARGET)

initrd: build
	@ARCH=$(ARCH) SYSROOT=$(SYSROOT_DIR) ANNOTATOR_BIN=$(ANNOTATOR_BIN) \
		bash $(SCRIPTS_DIR)/initrd.sh

iso: sysroot build initrd
	@ARCH=$(ARCH) SYSROOT=$(SYSROOT_DIR) bash $(SCRIPTS_DIR)/iso.sh

run:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/run.sh $(ISO_IMG)

run-minimal:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/run-minimal.sh --initrd $(MINIMAL_INITRD_IMG)

kill:
	@bash $(SCRIPTS_DIR)/kill.sh

# clean: wipe per-arch generated artefacts. Cargo cache (target/<triple>/)
# is preserved so subsequent builds are fast.
clean:
	@rm -rf $(INITRD_ROOT_DIR) $(ISO_ROOT_DIR)
	@rm -f $(MINIMAL_INITRD_IMG) $(ISO_IMG)
	@echo "Cleaned build artifacts for $(ARCH)"

clean-all: clean kill
	@rm -rf $(TARGET_DIR)/$(ARCH)-sysroot
	@echo "Removed $(ARCH)-sysroot (cargo cache preserved)"
