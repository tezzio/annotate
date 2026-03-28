################################################################################
#
# annotator
#
################################################################################

ANNOTATOR_VERSION = HEAD
ANNOTATOR_SITE = $(call github,tezzio,annotate,$(ANNOTATOR_VERSION))
ANNOTATOR_SITE_METHOD = git

ANNOTATOR_DEPENDENCIES = sdl2 sdl2_ttf

# Rust / Cargo package — use the cargo-package infrastructure
ANNOTATOR_LICENSE = MIT

define ANNOTATOR_BUILD_CMDS
	cd $(@D) && \
		$(TARGET_MAKE_ENV) \
		CARGO_HOME=$(HOST_DIR)/share/cargo \
		$(HOST_DIR)/bin/cargo build --release \
			--manifest-path $(@D)/Cargo.toml
endef

define ANNOTATOR_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/target/release/annotator \
		$(TARGET_DIR)/usr/bin/annotator
endef

$(eval $(generic-package))
