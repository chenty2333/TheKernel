ROOT_DIR := $(CURDIR)
STATE_DIR ?= $(ROOT_DIR)/.state
ARCH ?= riscv64
export PYTHONDONTWRITEBYTECODE ?= 1

SHELL_ARGS ?=
SMOKE_ARGS ?=
SYSTEM_ARGS ?=
ROOTFS_RV ?= $(STATE_DIR)/rootfs/rootfs-rv.img
ROOTFS_LA ?= $(STATE_DIR)/rootfs/rootfs-la.img
MAX_KERNEL_BYTES ?= 838860800

export THEKERNEL_DEV_IMAGE ?= thekernel-dev:local
DEV_ENV_DIR ?= $(ROOT_DIR)/dev-env
EMPTY_ROOTFS_DIR ?= $(STATE_DIR)/empty-rootfs

PYTHONPATH_CMD = PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}"
BUILD_TOOL = $(PYTHONPATH_CMD) python3 tools/build.py
QEMU_RUNNER = $(PYTHONPATH_CMD) python3 -m tools.qemu_runner run

CLEAN_DIRS ?= \
	$(ROOT_DIR)/.tmp \
	$(STATE_DIR)/ci \
	$(STATE_DIR)/empty-rootfs \
	$(STATE_DIR)/io-test-shell \
	$(STATE_DIR)/mm-performance-shell \
	$(STATE_DIR)/qemu-runner \
	$(STATE_DIR)/rootfs \
	$(STATE_DIR)/rootfs-build \
	$(STATE_DIR)/riscv64/out \
	$(STATE_DIR)/riscv64/logs \
	$(STATE_DIR)/loongarch64/out \
	$(STATE_DIR)/loongarch64/logs \
	$(STATE_DIR)/shell \
	$(STATE_DIR)/system-test \
	$(STATE_DIR)/*-current

default: all

help:
	@printf '%s\n' \
		'Build commands:' \
		'  make all          build both release-mode kernel images' \
		'  make artifacts    build both release-mode kernel images' \
		'  make test-fixtures  build both repository-built test root filesystems' \
		'  make kernels      build kernel-rv and kernel-la' \
		'  make rootfs       alias for make test-fixtures' \
		'  make kernel-rv    build the RISC-V kernel' \
		'  make kernel-la    build the LoongArch kernel' \
		'  make rootfs-rv    build the RISC-V test root filesystem' \
		'  make rootfs-la    build the LoongArch test root filesystem' \
		'' \
		'Run commands:' \
		'  make shell-rv     boot an interactive RISC-V shell' \
		'  make shell-la     boot an interactive LoongArch shell' \
		'  make system-test  run project semantic init on both architectures' \
		'  make system-test-rv | make system-test-la' \
		'' \
		'Test and environment commands:' \
		'  make test-tools   run project Python tool tests' \
		'  make smoke-list' \
		'  make smoke NAME=lwext4-io-boost ARCH=rv' \
		'  make dev-image | make dev-check | make dev-shell' \
		'  make clean        remove materialized build/run outputs, keep caches' \
		'  make clean-all    remove all generated state'

all: artifacts

artifacts: kernels

test-fixtures: rootfs-rv rootfs-la

kernels: kernel-rv kernel-la

rootfs: test-fixtures

kernel-rv:
	@$(BUILD_TOOL) kernel rv
	@$(MAKE) --no-print-directory check-kernel-size

kernel-la:
	@$(BUILD_TOOL) kernel la
	@$(MAKE) --no-print-directory check-kernel-size

kernel-rv-shell:
	@$(BUILD_TOOL) shell rv

kernel-la-shell:
	@$(BUILD_TOOL) shell la

kernel-rv-io-test:
	@$(BUILD_TOOL) io-test-shell rv

kernel-la-io-test:
	@$(BUILD_TOOL) io-test-shell la

kernel-rv-mm-performance:
	@$(BUILD_TOOL) mm-performance-shell rv

kernel-la-mm-performance:
	@$(BUILD_TOOL) mm-performance-shell la

rootfs-rv:
	@$(BUILD_TOOL) rootfs rv --output "$(ROOTFS_RV)"

rootfs-la:
	@$(BUILD_TOOL) rootfs la --output "$(ROOTFS_LA)"

shell-rv: kernel-rv-shell rootfs-rv
	@$(QEMU_RUNNER) \
		--arch rv \
		--kernel "$(STATE_DIR)/shell/kernel-rv" \
		--rootfs "$(ROOTFS_RV)" \
		--workdir "$(STATE_DIR)/qemu-runner/shell-rv" \
		--interactive \
		--input-after-marker THEKERNEL_SHELL_READY \
		$(SHELL_ARGS)

shell-la: kernel-la-shell rootfs-la
	@$(QEMU_RUNNER) \
		--arch la \
		--kernel "$(STATE_DIR)/shell/kernel-la" \
		--rootfs "$(ROOTFS_LA)" \
		--workdir "$(STATE_DIR)/qemu-runner/shell-la" \
		--interactive \
		--input-after-marker THEKERNEL_SHELL_READY \
		$(SHELL_ARGS)

system-test: system-test-rv system-test-la

system-test-rv:
	@./scripts/system-test.sh --arch rv $(SYSTEM_ARGS)

system-test-la:
	@./scripts/system-test.sh --arch la $(SYSTEM_ARGS)

smoke-list:
	@./scripts/smoke.sh list

smoke:
	@test -n "$(NAME)" || { printf '%s\n' 'NAME is required, e.g. make smoke NAME=lwext4-io-boost ARCH=rv' >&2; exit 1; }
	@arch="$(ARCH)"; \
	case "$$arch" in \
		riscv64) arch=rv ;; \
		loongarch64) arch=la ;; \
	esac; \
	./scripts/smoke.sh "$(NAME)" --arch "$$arch" $(SMOKE_ARGS)

test-tools:
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/build_tools -v
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/qemu_runner -v

dev-image:
	@mkdir -p "$(EMPTY_ROOTFS_DIR)"
	@THEKERNEL_ROOTFS_HOST_DIR="$(EMPTY_ROOTFS_DIR)" docker compose \
		--env-file "$(DEV_ENV_DIR)/versions.env" \
		-f "$(DEV_ENV_DIR)/compose.yaml" build dev

dev-check:
	@mkdir -p "$(EMPTY_ROOTFS_DIR)"
	@THEKERNEL_ROOTFS_HOST_DIR="$(EMPTY_ROOTFS_DIR)" docker compose \
		--env-file "$(DEV_ENV_DIR)/versions.env" \
		-f "$(DEV_ENV_DIR)/compose.yaml" run --rm --remove-orphans dev \
		thekernel-dev-check

dev-shell:
	@THEKERNEL_DEV_IMAGE="$(THEKERNEL_DEV_IMAGE)" ./scripts/dev-shell.sh \
		$(if $(DEV_CMD),-- $(DEV_CMD),)

dev-shell-root:
	@THEKERNEL_DEV_IMAGE="$(THEKERNEL_DEV_IMAGE)" ./scripts/dev-shell.sh \
		--service builder -- bash

clean:
	@rm -rf $(CLEAN_DIRS)
	@rm -f \
		$(ROOT_DIR)/kernel-rv \
		$(ROOT_DIR)/kernel-la \
		$(ROOT_DIR)/.axconfig.toml \
		$(ROOT_DIR)/.axconfig.old.toml \
		$(STATE_DIR)/riscv64/.axconfig.toml \
		$(STATE_DIR)/riscv64/.axconfig.old.toml \
		$(STATE_DIR)/loongarch64/.axconfig.toml \
		$(STATE_DIR)/loongarch64/.axconfig.old.toml

clean-all: clean
	@rm -rf "$(STATE_DIR)" "$(ROOT_DIR)/.tmp"

check-kernel-artifacts:
	@missing=0; \
	for artifact in "$(ROOT_DIR)/kernel-rv" "$(ROOT_DIR)/kernel-la"; do \
		if [ ! -s "$$artifact" ]; then \
			printf 'missing kernel artifact: %s\n' "$$artifact" >&2; \
			missing=1; \
		fi; \
	done; \
	exit "$$missing"

check-kernel-size:
	@for kernel in "$(ROOT_DIR)/kernel-rv" "$(ROOT_DIR)/kernel-la"; do \
		[ -f "$$kernel" ] || continue; \
		size="$$(stat -c '%s' "$$kernel")"; \
		if [ "$$size" -gt "$(MAX_KERNEL_BYTES)" ]; then \
			printf 'kernel artifact too large: %s (%s bytes > %s)\n' \
				"$$kernel" "$$size" "$(MAX_KERNEL_BYTES)" >&2; \
			exit 1; \
		fi; \
	done

.PHONY: \
	help all artifacts test-fixtures kernels rootfs \
	kernel-rv kernel-la kernel-rv-shell kernel-la-shell \
	kernel-rv-io-test kernel-la-io-test \
	kernel-rv-mm-performance kernel-la-mm-performance rootfs-rv rootfs-la \
	shell-rv shell-la system-test system-test-rv system-test-la \
	smoke-list smoke test-tools \
	dev-image dev-check dev-shell dev-shell-root \
	clean clean-all check-kernel-artifacts check-kernel-size
