# Necessary dependencies for the build system

# Tool to parse information about the target package
ifeq ($(shell cargo axplat --version 2>/dev/null),)
  $(info Installing cargo-axplat...)
  $(shell cargo install --locked cargo-axplat)
endif

# Tool to generate platform configuration files
ifeq ($(shell axconfig-gen --version 2>/dev/null),)
  $(info Installing axconfig-gen...)
  $(shell cargo install --locked axconfig-gen)
endif

# Cargo binutils
ifeq ($(shell command -v rust-objdump >/dev/null 2>&1 && command -v rust-objcopy >/dev/null 2>&1; echo $$?),1)
  $(info Installing cargo-binutils...)
  $(shell cargo install --locked cargo-binutils)
endif
