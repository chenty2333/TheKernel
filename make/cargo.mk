# Cargo features and build args

ifeq ($(V),1)
  verbose := -v
else ifeq ($(V),2)
  verbose := -vv
else
  verbose :=
endif

build_args-release := --release

build_args = \
  -Z unstable-options \
  --target $(TARGET) \
  --target-dir $(TARGET_DIR) \
  $(CARGO_BUILD_STD_ARGS) \
  $(build_args-$(MODE)) \
  $(verbose)

RUSTFLAGS_LINK_ARGS := -C link-arg=-T$(LD_SCRIPT) -C link-arg=-no-pie -C link-arg=-z -C link-arg=nostart-stop-gc
RUSTDOCFLAGS := -Z unstable-options --enable-index-page -D rustdoc::broken_intra_doc_links

ifeq ($(MAKECMDGOALS), doc_check_missing)
  RUSTDOCFLAGS += -D missing-docs
endif

define cargo_build
  $(call run_cmd,cargo -C $(1) build,$(build_args) --features "$(strip $(2))")
endef

# Lint the real kernel target rather than the host test target. Dead-code and
# drop-glue lints are configuration-sensitive: a symbol that is unreachable in
# the x86_64 host test build is frequently the live architecture path, and
# `GlobalGrace` only carries drop glue when `smp-tlb-shootdown` is enabled.
# Only the architecture build answers those lints truthfully.
# `-C` is resolved before cargo dispatches to the `cargo-clippy` subcommand
# binary, so the enabling `-Z unstable-options` must precede the subcommand
# name here. `cargo build` tolerates the trailing form; `cargo clippy` does not.
define cargo_clippy
  $(call run_cmd,cargo -Z unstable-options -C $(1) clippy,$(build_args) --features "$(strip $(2))" $(CLIPPY_PACKAGES) $(CLIPPY_ARGS))
endef
