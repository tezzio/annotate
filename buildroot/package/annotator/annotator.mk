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

# Exclude the cargo build output directory from the rsync when
# ANNOTATOR_OVERRIDE_SRCDIR is used, otherwise buildroot's rsync copies
# target/ (which contains buildroot-output itself) back into its own build
# tree, creating infinite path recursion.
ANNOTATOR_OVERRIDE_SRCDIR_RSYNC_EXCLUSIONS = --exclude target/

# Rust / Cargo package — use the cargo-package infrastructure
ANNOTATOR_LICENSE = MIT

# ANNOTATOR_PREBUILT_BIN is set by build.rs to the path of the release binary
# it compiled before invoking buildroot make.  We skip cargo entirely here to
# avoid infinite build.rs recursion and just install the already-built file.
define ANNOTATOR_BUILD_CMDS
	@true
endef

define ANNOTATOR_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(ANNOTATOR_PREBUILT_BIN) \
		$(TARGET_DIR)/usr/bin/annotator
endef

$(eval $(generic-package))
