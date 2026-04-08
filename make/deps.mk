# Necessary dependencies for the build system.
#
# The repository-local dev image is expected to provide these tools up front.
# Keep this file fail-fast and avoid mutating the host environment during build.

ifeq ($(shell cargo axplat --version 2>/dev/null),)
  $(error missing cargo-axplat; use the repo-local dev image or preinstall it)
endif

ifeq ($(shell axconfig-gen --version 2>/dev/null),)
  $(error missing axconfig-gen; use the repo-local dev image or preinstall it)
endif

ifeq ($(shell command -v rust-objdump >/dev/null 2>&1 && command -v rust-objcopy >/dev/null 2>&1; echo $$?),1)
  $(error missing rust-objdump/rust-objcopy; use the repo-local dev image or preinstall cargo-binutils)
endif
