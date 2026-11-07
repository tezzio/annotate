# Makefile for the annotate build system.
#
# The build produces a single executable per architecture, linked against
# the Debian library packages fetched by scripts/sysroot.sh.  There is no
# initrd, no ISO and no operating system image — just the binary.
#
# Usage:
#   make help              Show available targets
#   make bootstrap         Install host dependencies (+ rustup if needed)
#   make libs              Fetch the Debian library packages
#   make build             Build the annotator binary
#   make build-arm         Build the aarch64 binary
#   make all               Fetch the libraries and build both binaries
#   make clean             Remove the built binaries
#
# Environment:
#   ARCH       x86_64 | aarch64 (default: x86_64)
#   TARGET     Cargo target triple (default: $ARCH-unknown-linux-gnu)

PROJECT_DIR := $(abspath .)
TARGET_DIR  := $(PROJECT_DIR)/target
SCRIPTS_DIR := $(PROJECT_DIR)/scripts
ARCH        ?= x86_64
TARGET      ?= $(ARCH)-unknown-linux-gnu

LIBDIRS := $(TARGET_DIR)/x86_64-libs $(TARGET_DIR)/aarch64-libs

.PHONY: help bootstrap libs libs-x86 libs-arm build build-arm test all clean clean-all

help:
	@echo "annotate build system"
	@echo ""
	@echo "Setup targets:"
	@echo "  bootstrap      Install system dependencies and Rust"
	@echo "  libs           Fetch the Debian library packages for $(ARCH)"
	@echo "  libs-x86       Fetch the Debian library packages for x86_64"
	@echo "  libs-arm       Fetch the Debian library packages for aarch64"
	@echo ""
	@echo "Build targets:"
	@echo "  build          Build the annotator for $(TARGET)"
	@echo "  build-arm      Build the annotator for aarch64"
	@echo "  all            Fetch the libraries and build both binaries"
	@echo ""
	@echo "Clean targets:"
	@echo "  clean          Remove the built binaries"
	@echo "  clean-all      Remove the binaries and the fetched libraries"
	@echo ""
	@echo "Variables:"
	@echo "  ARCH=$(ARCH)         Target architecture"
	@echo "  TARGET=$(TARGET)     Cargo target triple"
	@echo ""
	@echo "Examples:"
	@echo "  make bootstrap                    # install host deps"
	@echo "  make                              # libs + build for $(ARCH)"
	@echo "  make build-arm                    # cross-compile for aarch64"
	@echo "  make all                          # both architectures"

bootstrap:
	@bash $(SCRIPTS_DIR)/bootstrap.sh

libs:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/sysroot.sh

libs-x86:
	@ARCH=x86_64 bash $(SCRIPTS_DIR)/sysroot.sh

libs-arm:
	@ARCH=aarch64 bash $(SCRIPTS_DIR)/sysroot.sh

build:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/build.sh

build-arm:
	@ARCH=aarch64 bash $(SCRIPTS_DIR)/build.sh

# test: run the unit tests.  The host has no SDL2 development packages
# installed, so the libraries from the library directory are put on the
# link path.
test:
	@ARCH=$(ARCH) bash $(SCRIPTS_DIR)/test.sh

all: libs-x86 libs-arm
	@$(MAKE) build
	@$(MAKE) build-arm

# clean: remove the built binaries.  The fetched libraries and the cargo
# cache are preserved so that the next build is fast.
clean:
	@rm -f $(TARGET_DIR)/x86_64/annotator $(TARGET_DIR)/aarch64/annotator
	@echo "Cleaned binaries (libraries and cargo cache preserved)"

# clean-all: also remove the fetched Debian library packages and the
# apt state that goes with them.
clean-all: clean
	@rm -rf $(LIBDIRS) $(TARGET_DIR)/apt-cache $(TARGET_DIR)/apt-state \
	          $(TARGET_DIR)/apt-keyring $(TARGET_DIR)/tools
	@echo "Removed the fetched libraries"
