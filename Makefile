# Build Options
ARCH ?= riscv64
export ARCH
LOG ?= off
export LOG
BANNER ?= n
export BANNER
BACKTRACE ?= n
export BACKTRACE
DWARF ?= y
export DWARF
MEMTRACK ?= n
export MEMTRACK
OSCOMP_PLAN_OVERRIDE ?=
export OSKERNEL_DEV_IMAGE ?= thekernel-dev:local
DEV_ENV_DIR ?= $(ROOT_DIR)/dev-env
EMPTY_TESTSUITE_DIR ?= $(ROOT_DIR)/.state/empty-testsuites
AUTOSCRUB_DIRS ?= \
	$(ROOT_DIR)/.tmp \
	$(STATE_DIR)/riscv64 \
	$(STATE_DIR)/loongarch64 \
	$(STATE_DIR)/oscomp-replay

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

help:
	@printf '%s\n' \
		'Build commands:' \
		'  make all          clean evaluator build; remote-submission entrypoint, not high-frequency' \
		'  make artifacts    refresh kernel-rv/kernel-la/disk.img/disk-la.img without clean-eval' \
		'  make kernels      high-frequency build of kernel-rv and kernel-la only' \
		'  make kernel-rv    high-frequency RISC-V evaluator kernel; keeps Cargo target cache' \
		'  make kernel-la    high-frequency LoongArch evaluator kernel; keeps Cargo target cache' \
		'  make disk.img     rebuild support disk only' \
		'  make clean-eval   remove evaluator artifacts and build/replay state; keep .state/ltp-lab' \
		'  make clean        full local clean, including .state' \
		'' \
		'Replay commands:' \
		'  make eval-rv      rebuild kernel-rv, then replay rv official image' \
		'  make eval-la      rebuild kernel-la, then replay la official image' \
		'  make replay-rv    reuse existing artifacts, then replay rv official image' \
		'  make replay-la    reuse existing artifacts, then replay la official image' \
		'' \
		'Lab commands:' \
		'  make lab-check' \
		'  make lab-inventory' \
		'  make lab-campaign LAB_ARGS="create goal3-fs-vfs-0001 --runtest fs --runtest syscalls --limit 120"' \
		'  make lab-list LAB_ARGS="..."' \
		'  make lab-run LAB_ARGS="..."' \
		'  make lab-trim       daily cleanup of disposable lab artifacts' \
		'  make lab-clean LAB_CLEAN_ARGS="..."'

all:
	@$(MAKE) --no-print-directory clean-eval
	@$(MAKE) --no-print-directory artifacts

artifacts: kernels disk.img disk-la.img

kernels: kernel-rv kernel-la

prebuild-scrub:
	@rm -rf $(AUTOSCRUB_DIRS)
	@mkdir -p $(STATE_DIR)

clean-eval: prebuild-scrub legacy-clean

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
		$(ROOT_DIR)/rv_.out \
		$(ROOT_DIR)/la_.out \
		$(ROOT_DIR)/score.txt \
		$(ROOT_DIR)/qemu.log \
		$(ROOT_DIR)/netdump.pcap \
		$(ROOT_DIR)/.axconfig.toml \
		$(ROOT_DIR)/.axconfig.old.toml \
		$(ROOT_DIR)/make/disk.img \
		$(ROOT_DIR)/make/disk-*.img

defconfig:
	@$(MAKE) -C make $@

dev-image:
	@mkdir -p "$(EMPTY_TESTSUITE_DIR)"
	@OSCOMP_TESTSUITE_HOST_DIR="$(EMPTY_TESTSUITE_DIR)" docker compose --env-file "$(DEV_ENV_DIR)/versions.env" -f "$(DEV_ENV_DIR)/compose.yaml" build dev

dev-check:
	@mkdir -p "$(EMPTY_TESTSUITE_DIR)"
	@OSCOMP_TESTSUITE_HOST_DIR="$(EMPTY_TESTSUITE_DIR)" docker compose --env-file "$(DEV_ENV_DIR)/versions.env" -f "$(DEV_ENV_DIR)/compose.yaml" run --rm --remove-orphans dev oskernel-image-check

dev-shell:
	@OSKERNEL_DEV_IMAGE="$(OSKERNEL_DEV_IMAGE)" ./scripts/dev-shell.sh $(if $(DEV_CMD),-- $(DEV_CMD),)

dev-shell-root:
	@OSKERNEL_DEV_IMAGE="$(OSKERNEL_DEV_IMAGE)" ./scripts/dev-shell.sh --service builder -- bash

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

replay-rv:
	@./scripts/oscomp.sh run --arch rv --skip-kernel-build $(OSCOMP_ARGS)

replay-la:
	@./scripts/oscomp.sh run --arch la --skip-kernel-build $(OSCOMP_ARGS)

lab-check:
	@./scripts/oscomp.sh lab bootstrap

lab-inventory:
	@./scripts/oscomp.sh lab inventory

lab-plan:
	@./scripts/oscomp.sh lab plan $(LAB_ARGS)

lab-list:
	@./scripts/oscomp.sh lab generate $(LAB_ARGS)

lab-run:
	@./scripts/oscomp.sh lab run $(LAB_ARGS)

lab-parse:
	@./scripts/oscomp.sh lab parse $(LAB_ARGS)

lab-summary:
	@./scripts/oscomp.sh lab summarize $(LAB_ARGS)

lab-promote:
	@./scripts/oscomp.sh lab promote $(LAB_ARGS)

lab-campaign:
	@./scripts/oscomp.sh lab campaign $(LAB_ARGS)

lab-clean:
	@./scripts/oscomp.sh lab clean --generated --legacy-root $(LAB_CLEAN_ARGS)

lab-trim:
	@./scripts/oscomp.sh lab clean --trim $(LAB_CLEAN_ARGS)

debug:
	@printf '%s\n' 'debug is not wired to the official pre-2025 evaluator flow; use scripts/oscomp.sh run instead.' >&2
	@exit 1

kernel-rv:
	@$(MAKE) -C make ARCH=riscv64 BUS=mmio defconfig
	@$(MAKE) -C make ARCH=riscv64 BUS=mmio build-elf-fast
	@kernel="$$(find "$(STATE_DIR)/riscv64/out" -maxdepth 1 -name '*.elf' | head -n 1)"; \
	test -n "$$kernel"; \
	python3 scripts/patch-riscv-kernel-elf.py "$$kernel" "$@"
	@$(MAKE) --no-print-directory check-eval-kernel-size

kernel-la:
	@$(MAKE) -C make ARCH=loongarch64 defconfig
	@$(MAKE) -C make ARCH=loongarch64 build-elf-fast
	@kernel="$$(find "$(STATE_DIR)/loongarch64/out" -maxdepth 1 -name '*.elf' | head -n 1)"; \
	test -n "$$kernel"; \
	python3 scripts/patch-loongarch-kernel-elf.py "$$kernel" "$@"
	@$(MAKE) --no-print-directory check-eval-kernel-size

disk.img:
	@set -- bash ./scripts/build-oscomp-support-disk.sh --arch both --output "$@"; \
		if [ -n "$(OSCOMP_PLAN_OVERRIDE)" ]; then \
			set -- "$$@" --plan-override "$(OSCOMP_PLAN_OVERRIDE)"; \
		fi; \
		"$$@"

disk-la.img: disk.img
	@cp "$<" "$@"

# Aliases
rv:
	$(MAKE) ARCH=riscv64 run

la:
	$(MAKE) ARCH=loongarch64 run

.PHONY: help all artifacts kernels build run eval-rv eval-la replay-rv replay-la lab-check lab-inventory lab-plan lab-list lab-run lab-parse lab-summary lab-promote lab-campaign lab-clean lab-trim dev-image dev-check dev-shell dev-shell-root debug disasm clean clean-eval legacy-clean prebuild-scrub check-eval-kernel-size kernel-rv kernel-la disk.img disk-la.img
check-eval-kernel-size:
	@for kernel in $(ROOT_DIR)/kernel-rv $(ROOT_DIR)/kernel-la; do \
		[ -f "$$kernel" ] || continue; \
		size="$$(stat -c '%s' "$$kernel")"; \
		if [ "$$size" -gt "$(MAX_EVAL_KERNEL_BYTES)" ]; then \
			printf 'kernel artifact too large for evaluator: %s (%s bytes > %s)\n' "$$kernel" "$$size" "$(MAX_EVAL_KERNEL_BYTES)" >&2; \
			exit 1; \
		fi; \
	done
