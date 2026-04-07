# Build Options
ARCH ?= riscv64
export ARCH
LOG ?= warn
export LOG
DWARF ?= y
export DWARF
MEMTRACK ?= n
export MEMTRACK
export OSKERNEL_DEV_IMAGE ?= oskernel-dev:local

# QEMU Options
export BLK := y
export NET := y
export VSOCK := n
export MEM := 1G
export ICOUNT := n
MAX_EVAL_KERNEL_BYTES ?= 838860800

# Generated Options
export A := $(PWD)
export NO_AXSTD := y
export AX_LIB := axfeat
export APP_FEATURES := qemu
export ROOT_DIR := $(PWD)
STATE_DIR ?= $(ROOT_DIR)/.state
STATE_ARCH_DIR ?= $(STATE_DIR)/$(ARCH)
TARGET_DIR ?= $(STATE_ARCH_DIR)/target
OUT_DIR ?= $(STATE_ARCH_DIR)/out
OUT_CONFIG ?= $(STATE_ARCH_DIR)/.axconfig.toml
LOG_DIR ?= $(STATE_ARCH_DIR)/logs
QEMU_LOG_FILE ?= $(LOG_DIR)/qemu.log
NET_DUMP_FILE ?= $(LOG_DIR)/netdump.pcap
DISK_IMG ?= $(STATE_ARCH_DIR)/disk.img

ifeq ($(MEMTRACK), y)
	APP_FEATURES += starry-api/memtrack
endif

default: build
all: legacy-clean kernel-rv kernel-la disk.img

legacy-clean:
	@rm -rf \
		$(ROOT_DIR)/target
	@rm -f \
		$(ROOT_DIR)/*.bin \
		$(ROOT_DIR)/*.elf \
		$(ROOT_DIR)/kernel-rv \
		$(ROOT_DIR)/kernel-la \
		$(ROOT_DIR)/disk.img \
		$(ROOT_DIR)/disk-la.img \
		$(ROOT_DIR)/qemu.log \
		$(ROOT_DIR)/netdump.pcap \
		$(ROOT_DIR)/.axconfig.toml \
		$(ROOT_DIR)/.axconfig.old.toml \
		$(ROOT_DIR)/make/disk.img \
		$(ROOT_DIR)/make/disk-*.img

defconfig:
	@$(MAKE) -C make $@

justrun:
	@./scripts/oscomp.sh run --arch $(ARCH) --skip-kernel-build $(OSCOMP_ARGS)

docker-shell:
	@OSKERNEL_DEV_IMAGE="$(OSKERNEL_DEV_IMAGE)" ./scripts/docker-shell.sh

clean:
	@$(MAKE) -C make clean
	@rm -rf $(STATE_DIR)
	@$(MAKE) --no-print-directory legacy-clean

build disasm: defconfig
	@$(MAKE) -C make $@

run:
	@./scripts/oscomp.sh run --arch $(ARCH) $(OSCOMP_ARGS)

eval-rv: kernel-rv
	@./scripts/oscomp.sh run --arch rv --skip-kernel-build $(OSCOMP_ARGS)

eval-la: kernel-la
	@./scripts/oscomp.sh run --arch la --skip-kernel-build $(OSCOMP_ARGS)

debug:
	@printf '%s\n' 'debug is not wired to the official pre-2025 evaluator flow; use scripts/oscomp.sh run instead.' >&2
	@exit 1

kernel-rv:
	@$(MAKE) -C make ARCH=riscv64 BUS=mmio defconfig
	@$(MAKE) -C make ARCH=riscv64 BUS=mmio build
	@kernel="$$(find "$(STATE_DIR)/riscv64/out" -maxdepth 1 -name '*.elf' | head -n 1)"; \
	test -n "$$kernel"; \
	python3 scripts/patch-riscv-kernel-elf.py "$$kernel" "$@"
	@$(MAKE) --no-print-directory check-eval-kernel-size

kernel-la:
	@$(MAKE) -C make ARCH=loongarch64 defconfig
	@$(MAKE) -C make ARCH=loongarch64 build
	@kernel="$$(find "$(STATE_DIR)/loongarch64/out" -maxdepth 1 -name '*.elf' | head -n 1)"; \
	test -n "$$kernel"; \
	python3 scripts/patch-loongarch-kernel-elf.py "$$kernel" "$@"
	@$(MAKE) --no-print-directory check-eval-kernel-size

disk.img:
	@bash ./scripts/build-oscomp-support-disk.sh --arch both --output "$@"

disk-la.img: disk.img
	@cp disk.img $@

ci-test:
	./scripts/ci-test.py $(ARCH)

# Aliases
rv:
	$(MAKE) ARCH=riscv64 run

la:
	$(MAKE) ARCH=loongarch64 run

vf2:
	$(MAKE) ARCH=riscv64 APP_FEATURES=vf2 MYPLAT=axplat-riscv64-visionfive2 BUS=mmio build

.PHONY: all build run eval-rv eval-la justrun docker-shell debug disasm clean legacy-clean check-eval-kernel-size kernel-rv kernel-la disk.img disk-la.img
check-eval-kernel-size:
	@for kernel in $(ROOT_DIR)/kernel-rv $(ROOT_DIR)/kernel-la; do \
		[ -f "$$kernel" ] || continue; \
		size="$$(stat -c '%s' "$$kernel")"; \
		if [ "$$size" -gt "$(MAX_EVAL_KERNEL_BYTES)" ]; then \
			printf 'kernel artifact too large for evaluator: %s (%s bytes > %s)\n' "$$kernel" "$$size" "$(MAX_EVAL_KERNEL_BYTES)" >&2; \
			exit 1; \
		fi; \
	done
