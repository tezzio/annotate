################################################################################
#
# annotator
#
################################################################################

# When built via 'cargo build' from the repo, ANNOTATOR_OVERRIDE_SRCDIR is
# set by build.rs so buildroot uses the local source tree directly.
# The SITE below is the fallback for standalone buildroot builds.
ANNOTATOR_VERSION = main
ANNOTATOR_SITE = https://github.com/tezzio/annotate.git
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
