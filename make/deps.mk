# Necessary dependencies for the build system.
#
# The x86_64 build should work without host-global cargo-axplat or axconfig-gen
# installations. Fall back to the repo-local helper when axconfig-gen is
# absent, and only require Rust binutils for goals that actually invoke them.

ifeq ($(shell axconfig-gen --version >/dev/null 2>&1; echo $$?),0)
  AXCONFIG_GEN := axconfig-gen
else
  AXCONFIG_GEN := $(ROOT_DIR)/scripts/axconfig-tool.py
  ifeq ($(wildcard $(AXCONFIG_GEN)),)
    $(error missing axconfig-gen and repo-local fallback $(AXCONFIG_GEN))
  endif
endif

ifneq ($(filter build all run justrun debug,$(or $(MAKECMDGOALS), $(.DEFAULT_GOAL))),)
  ifeq ($(shell command -v rust-objcopy >/dev/null 2>&1; echo $$?),1)
    $(error missing rust-objcopy; use the repo-local dev image or preinstall cargo-binutils)
  endif
endif

ifneq ($(filter disasm,$(MAKECMDGOALS)),)
  ifeq ($(shell command -v rust-objdump >/dev/null 2>&1; echo $$?),1)
    $(error missing rust-objdump; use the repo-local dev image or preinstall cargo-binutils)
  endif
endif
