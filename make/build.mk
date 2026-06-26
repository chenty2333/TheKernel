# Main building script

include cargo.mk

ifeq ($(APP_TYPE), c)
  include build_c.mk
else
  rust_package := $(shell cat $(APP)/Cargo.toml | sed -n 's/^name = "\([a-z0-9A-Z_\-]*\)"/\1/p' | head -1)
  rust_elf := $(TARGET_DIR)/$(TARGET)/$(MODE)/$(rust_package)
endif

ifneq ($(filter $(MAKECMDGOALS),doc doc_check_missing),)
  # run `make doc`
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)") $(info RUSTDOCFLAGS: "$(RUSTDOCFLAGS)"))
  export RUSTFLAGS
  export RUSTDOCFLAGS
else ifneq ($(filter $(MAKECMDGOALS),unittest unittest_no_fail_fast),)
  # run `make unittest`
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)"))
  export RUSTFLAGS
else ifneq ($(filter $(or $(MAKECMDGOALS), $(.DEFAULT_GOAL)), all build build-elf build-elf-fast run justrun debug),)
  # run `make build` and other above goals
  ifneq ($(V),)
    $(info APP: "$(APP)")
    $(info APP_TYPE: "$(APP_TYPE)")
    $(info FEATURES: "$(FEATURES)")
    $(info PLAT_CONFIG: "$(PLAT_CONFIG)")
    $(info arceos features: "$(AX_FEAT)")
    $(info lib features: "$(LIB_FEAT)")
    $(info app features: "$(APP_FEAT)")
  endif
  ifeq ($(APP_TYPE), c)
    $(if $(V), $(info CFLAGS: "$(CFLAGS)") $(info LDFLAGS: "$(LDFLAGS)"))
  else ifeq ($(APP_TYPE), rust)
    RUSTFLAGS += $(RUSTFLAGS_LINK_ARGS)
    ifeq ($(ARCH), loongarch64)
      # The default loongarch64 target exposes LSX, which lets LLVM emit
      # kernel-space vector ops before we have any vr* save/restore support.
      RUSTFLAGS += -C target-feature=-lsx,-lasx
      ifeq ($(TARGET), loongarch64-unknown-none)
        # Hard-float prebuilt core may still contain LSX with this toolchain.
        # The default LA target is softfloat, so this network-sensitive path is
        # only used by explicit hard-float builds.
        CARGO_BUILD_STD_ARGS += -Z build-std=core,alloc,compiler_builtins
      endif
    endif
  endif
  ifneq ($(filter y,$(DEBUGINFO) $(DWARF)),)
    RUSTFLAGS += -C force-frame-pointers -C debuginfo=2 -C strip=none
  endif
  $(if $(V), $(info RUSTFLAGS: "$(RUSTFLAGS)"))
  export RUSTFLAGS
  ifeq ($(LTO), y)
    export CARGO_PROFILE_RELEASE_LTO=true
    export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
  endif
endif

_cargo_build: oldconfig | prepare_build_state
	@printf "    $(GREEN_C)Building$(END_C) App: $(APP_NAME), Arch: $(ARCH), Platform: $(PLAT_NAME), App type: $(APP_TYPE)\n"
ifeq ($(APP_TYPE), rust)
	$(call cargo_build,$(APP),$(AX_FEAT) $(LIB_FEAT) $(APP_FEAT))
	@cp $(rust_elf) $(OUT_ELF)
else ifeq ($(APP_TYPE), c)
	$(call cargo_build,ulib/axlibc,$(AX_FEAT) $(LIB_FEAT))
endif

build-elf: prepare_build_state $(OUT_ELF)

$(OUT_ELF): _cargo_build

build-elf-fast: oldconfig | state_dirs
	@printf "    $(GREEN_C)Building$(END_C) App: $(APP_NAME), Arch: $(ARCH), Platform: $(PLAT_NAME), App type: $(APP_TYPE)\n"
ifeq ($(APP_TYPE), rust)
	$(call cargo_build,$(APP),$(AX_FEAT) $(LIB_FEAT) $(APP_FEAT))
	@cp $(rust_elf) $(OUT_ELF)
else ifeq ($(APP_TYPE), c)
	$(call cargo_build,ulib/axlibc,$(AX_FEAT) $(LIB_FEAT))
endif

$(OUT_DIR):
	$(call run_cmd,mkdir,-p $@)

_dwarf: $(OUT_ELF)
ifeq ($(DWARF), y)
	$(call run_cmd,./dwarf.sh,$(OUT_ELF) $(OBJCOPY))
endif

$(OUT_BIN): $(OUT_ELF) _dwarf
	$(call run_cmd,$(OBJCOPY),$(OUT_ELF) --strip-all -O binary $@)
	@if [ ! -s $(OUT_BIN) ]; then \
		echo 'Empty kernel image "$(notdir $(FINAL_IMG))" is built, please check your build configuration'; \
		exit 1; \
	fi

ifeq ($(ARCH), aarch64)
  uimg_arch := arm64
else ifeq ($(ARCH), riscv64)
  uimg_arch := riscv
else
  uimg_arch := $(ARCH)
endif

$(OUT_UIMG): $(OUT_BIN)
	$(call run_cmd,mkimage,\
		-A $(uimg_arch) -O linux -T kernel -C none \
		-a $(subst _,,$(shell $(AXCONFIG_GEN) "$(OUT_CONFIG)" -r plat.kernel-base-paddr)) \
		-d $(OUT_BIN) $@)

.PHONY: build-elf build-elf-fast _cargo_build _dwarf
