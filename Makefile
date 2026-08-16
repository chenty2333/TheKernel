ROOT_DIR := $(CURDIR)
# Keep the Makefile paths and Python builder's environment on one state root.
STATE_DIR ?= $(if $(THEKERNEL_STATE_DIR),$(THEKERNEL_STATE_DIR),$(ROOT_DIR)/.state)
export THEKERNEL_STATE_DIR := $(STATE_DIR)
ARCH ?= x86_64
export PYTHONDONTWRITEBYTECODE ?= 1

SHELL_ARGS ?=
SMOKE_ARGS ?=
SYSTEM_ARGS ?=
ROOTFS_X86 ?= $(STATE_DIR)/rootfs/rootfs-x86.img
ESP_X86 ?= $(STATE_DIR)/uefi/kernel-x86_64.esp
SHELL_ESP_X86 ?= $(STATE_DIR)/uefi/shell-x86_64.esp
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
	$(STATE_DIR)/uefi \
	$(STATE_DIR)/x86_64/out \
	$(STATE_DIR)/x86_64/logs \
	$(STATE_DIR)/shell \
	$(STATE_DIR)/system-test \
	$(STATE_DIR)/*-current

default: all

help:
	@printf '%s\n' \
		'Build commands:' \
		'  make all          build the x86_64 q35/UEFI kernel image' \
		'  make artifacts    build the x86_64 q35/UEFI kernel image' \
		'  make test-fixtures  build the repository-built test root filesystem' \
		'  make kernels      build kernel-x86_64' \
		'  make rootfs       alias for make test-fixtures' \
		'  make kernel-x86_64 build the x86_64 Multiboot kernel and UEFI ESP' \
		'  make rootfs-x86   build the x86_64 test root filesystem' \
		'' \
		'Run commands:' \
		'  make shell-x86_64 boot an interactive x86_64 q35/UEFI shell' \
		'  make system-test  run project semantic init on x86_64 q35/UEFI' \
		'  make system-test-x86_64' \
		'' \
		'Test and environment commands:' \
		'  make test-tools   run project Python tool tests' \
		'  make smoke-list' \
		'  make smoke NAME=lwext4-io-boost ARCH=x86' \
		'  make dev-image | make dev-check | make dev-shell' \
		'  make clean        remove materialized build/run outputs, keep caches' \
		'  make clean-all    remove all generated state'

all: artifacts

artifacts: kernels

test-fixtures: rootfs-x86

kernels: kernel-x86_64

rootfs: test-fixtures

kernel-x86_64:
	@$(BUILD_TOOL) kernel x86_64 --esp "$(ESP_X86)"
	@$(MAKE) --no-print-directory check-kernel-size

kernel-x86_64-shell:
	@$(BUILD_TOOL) shell x86_64 --esp "$(SHELL_ESP_X86)"

kernel-x86_64-io-test:
	@$(BUILD_TOOL) io-test-shell x86_64 --esp "$(SHELL_ESP_X86)"

kernel-x86_64-mm-performance:
	@$(BUILD_TOOL) mm-performance-shell x86_64 --esp "$(SHELL_ESP_X86)"

rootfs-x86:
	@$(BUILD_TOOL) rootfs x86 --output "$(ROOTFS_X86)"

shell-x86_64: kernel-x86_64-shell rootfs-x86
	@$(QEMU_RUNNER) \
		--arch x86_64 \
		--kernel "$(STATE_DIR)/shell/kernel-x86_64" \
		--rootfs "$(ROOTFS_X86)" \
		--esp "$(SHELL_ESP_X86)" \
		--workdir "$(STATE_DIR)/qemu-runner/shell-x86_64" \
		--interactive \
		--input-after-marker THEKERNEL_SHELL_READY \
		$(SHELL_ARGS)

system-test: system-test-x86_64

system-test-x86_64:
	@./scripts/system-test.sh --arch x86_64 $(SYSTEM_ARGS)

smoke-list:
	@./scripts/smoke.sh list

smoke:
	@test -n "$(NAME)" || { printf '%s\n' 'NAME is required, e.g. make smoke NAME=lwext4-io-boost ARCH=x86' >&2; exit 1; }
	@arch="$(ARCH)"; \
	case "$$arch" in \
		x86_64) arch=x86 ;; \
	esac; \
	./scripts/smoke.sh "$(NAME)" --arch "$$arch" $(SMOKE_ARGS)

test-tools:
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/build_tools -v
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/qemu_runner -v
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests -p 'test_x86_uefi_esp.py' -v

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
		$(ROOT_DIR)/kernel-x86_64 \
		$(ROOT_DIR)/.axconfig.toml \
		$(ROOT_DIR)/.axconfig.old.toml \
		$(STATE_DIR)/x86_64/.axconfig.toml \
		$(STATE_DIR)/x86_64/.axconfig.old.toml

clean-all: clean
	@rm -rf "$(STATE_DIR)" "$(ROOT_DIR)/.tmp"

check-kernel-artifacts:
	@missing=0; \
	for artifact in "$(ROOT_DIR)/kernel-x86_64"; do \
		if [ ! -s "$$artifact" ]; then \
			printf 'missing kernel artifact: %s\n' "$$artifact" >&2; \
			missing=1; \
		fi; \
	done; \
	exit "$$missing"

check-kernel-size:
	@for kernel in "$(ROOT_DIR)/kernel-x86_64"; do \
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
	kernel-x86_64 kernel-x86_64-shell \
	kernel-x86_64-io-test \
	kernel-x86_64-mm-performance rootfs-x86 \
	shell-x86_64 system-test system-test-x86_64 \
	smoke-list smoke test-tools \
	dev-image dev-check dev-shell dev-shell-root \
	clean clean-all check-kernel-artifacts check-kernel-size
