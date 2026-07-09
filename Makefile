ROOT_DIR := $(CURDIR)
STATE_DIR ?= $(ROOT_DIR)/.state
ARCH ?= riscv64
export PYTHONDONTWRITEBYTECODE ?= 1
KEEP ?=
QEMU_LOG ?=
QEMU_TRACE ?=
SHELL_ARGS ?=
LAB_ARGS ?=
SMOKE_ARGS ?=
REPLAY_ARCH ?= $(if $(filter command line,$(origin ARCH)),$(ARCH),both)
REPLAY_NAME := $(if $(filter command line,$(origin NAME)),$(NAME),replay)
export OSKERNEL_DEV_IMAGE ?= thekernel-dev:local
DEV_ENV_DIR ?= $(ROOT_DIR)/dev-env
EMPTY_TESTSUITE_DIR ?= $(ROOT_DIR)/.state/empty-testsuites
CLEAN_DIRS ?= \
	$(ROOT_DIR)/.tmp \
	$(STATE_DIR)/replay \
	$(STATE_DIR)/oscomp-replay \
	$(STATE_DIR)/oscomp-eval/runs \
	$(STATE_DIR)/riscv64/out \
	$(STATE_DIR)/riscv64/logs \
	$(STATE_DIR)/loongarch64/out \
	$(STATE_DIR)/loongarch64/logs \
	$(STATE_DIR)/shell

MAX_EVAL_KERNEL_BYTES ?= 838860800

PYTHONPATH_CMD = PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}"
BUILD_TOOL = $(PYTHONPATH_CMD) python3 tools/build.py

default: all

help:
	@printf '%s\n' \
		'Build commands:' \
		'  make all          official evaluator entrypoint; builds kernel-rv/kernel-la/disk.img/disk-la.img only' \
		'  make kernels      high-frequency build of kernel-rv and kernel-la only' \
		'  make kernel-rv    high-frequency RISC-V evaluator kernel; keeps Cargo target cache' \
		'  make kernel-la    high-frequency LoongArch evaluator kernel; keeps Cargo target cache' \
		'  make disk.img     build/update RISC-V support disk only' \
		'  make disk-la.img  build/update LoongArch support disk only' \
		'  make test-tools   run Python tool tests' \
		'  make dev-shell    open the containerized development shell' \
		'  make clean        remove evaluator artifacts, replay runs, and arch out/logs; keep caches and lab state' \
		'  make clean-all    full local clean, including all .state data' \
		'' \
		'Replay commands:' \
		'  make replay       build artifacts, run rv/la in parallel, judge, and score' \
		'  make replay ARCH=rv|la|both' \
		'  make replay NAME="memory_sys_improve"' \
		'  make replay KEEP=1' \
		'  make replay QEMU_LOG=1' \
		'  make replay QEMU_TRACE=int|cpu|exec' \
		'' \
		'Boot commands:' \
		'  make shell-rv     build a shell-mode rv kernel, then boot an interactive shell' \
		'  make shell-la     build a shell-mode la kernel, then boot an interactive shell' \
		'  make shell-rv SHELL_ARGS="--image path/to/sdcard-rv.img"' \
		'' \
		'Smoke commands:' \
		'  make smoke-list' \
		'  make smoke NAME=lwext4-io-boost ARCH=rv' \
		'' \
		'More:' \
		'  make help-lab     show focused lab helper commands'

help-lab:
	@printf '%s\n' \
		'Lab commands:' \
		'  make lab-list' \
		'  make lab-explain ARCH=rv SELECT=ltp-glibc:openat01' \
		'  make lab-run ARCH=rv SELECT=ltp-glibc:openat01' \
		'' \
		'Direct form:' \
		'  ./scripts/lab run --arch rv --select ltp-glibc:openat01'

all:
	@$(MAKE) --no-print-directory artifacts

artifacts: kernels disk.img disk-la.img
	@$(MAKE) --no-print-directory check-eval-artifacts

kernels: kernel-rv kernel-la

prebuild-scrub:
	@rm -rf $(CLEAN_DIRS)
	@mkdir -p $(STATE_DIR)

clean: prebuild-scrub legacy-clean

legacy-clean:
	@rm -f \
		$(STATE_DIR)/riscv64/.axconfig.toml \
		$(STATE_DIR)/riscv64/.axconfig.old.toml \
		$(STATE_DIR)/loongarch64/.axconfig.toml \
		$(STATE_DIR)/loongarch64/.axconfig.old.toml
	@rm -f \
		$(ROOT_DIR)/*.bin \
		$(ROOT_DIR)/*.elf \
		$(ROOT_DIR)/kernel-rv \
		$(ROOT_DIR)/kernel-la \
		$(ROOT_DIR)/disk.img \
		$(ROOT_DIR)/disk-rv.img \
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

clean-all:
	@$(MAKE) -C make clean
	@rm -rf $(STATE_DIR)
	@$(MAKE) --no-print-directory legacy-clean

replay:
	@arch="$(REPLAY_ARCH)"; \
	case "$$arch" in \
		both|all) \
			$(MAKE) --no-print-directory artifacts; \
			$(PYTHONPATH_CMD) python3 -m tools.oscomp_eval evaluate --arch both --name "$(REPLAY_NAME)" $(if $(KEEP),--keep,) $(if $(QEMU_LOG),--qemu-log,) $(if $(QEMU_TRACE),--qemu-trace "$(QEMU_TRACE)",); \
			;; \
		rv|riscv64) \
			$(MAKE) --no-print-directory replay-rv; \
			;; \
		la|loongarch64) \
			$(MAKE) --no-print-directory replay-la; \
			;; \
		*) \
			printf 'unsupported replay ARCH: %s\n' "$$arch" >&2; \
			exit 2; \
			;; \
	esac

replay-rv: kernel-rv disk.img
	@$(PYTHONPATH_CMD) python3 -m tools.oscomp_eval evaluate --arch rv --name "$(REPLAY_NAME)" $(if $(KEEP),--keep,) $(if $(QEMU_LOG),--qemu-log,) $(if $(QEMU_TRACE),--qemu-trace "$(QEMU_TRACE)",)

replay-la: kernel-la disk-la.img
	@$(PYTHONPATH_CMD) python3 -m tools.oscomp_eval evaluate --arch la --name "$(REPLAY_NAME)" $(if $(KEEP),--keep,) $(if $(QEMU_LOG),--qemu-log,) $(if $(QEMU_TRACE),--qemu-trace "$(QEMU_TRACE)",)

shell-rv: kernel-rv-shell
	@$(PYTHONPATH_CMD) python3 -m tools.oscomp_eval.replay shell --arch rv --kernel "$(STATE_DIR)/shell/kernel-rv" $(SHELL_ARGS)

shell-la: kernel-la-shell
	@$(PYTHONPATH_CMD) python3 -m tools.oscomp_eval.replay shell --arch la --kernel "$(STATE_DIR)/shell/kernel-la" $(SHELL_ARGS)

lab-list:
	@./scripts/lab list

lab-explain:
	@test -n "$(ARCH)" || { printf '%s\n' 'ARCH is required, e.g. make lab-explain ARCH=rv SELECT=ltp-glibc:openat01' >&2; exit 1; }
	@test -n "$(SELECT)" || { printf '%s\n' 'SELECT is required, e.g. make lab-explain ARCH=rv SELECT=ltp-glibc:openat01' >&2; exit 1; }
	@./scripts/lab explain --arch "$(ARCH)" --select "$(SELECT)" $(LAB_ARGS)

lab-run:
	@test -n "$(ARCH)" || { printf '%s\n' 'ARCH is required, e.g. make lab-run ARCH=rv SELECT=ltp-glibc:openat01' >&2; exit 1; }
	@test -n "$(SELECT)" || { printf '%s\n' 'SELECT is required, e.g. make lab-run ARCH=rv SELECT=ltp-glibc:openat01' >&2; exit 1; }
	@./scripts/lab run --arch "$(ARCH)" --select "$(SELECT)" $(LAB_ARGS)

kernel-rv:
	@$(BUILD_TOOL) kernel rv
	@$(MAKE) --no-print-directory check-eval-kernel-size

kernel-la:
	@$(BUILD_TOOL) kernel la
	@$(MAKE) --no-print-directory check-eval-kernel-size

kernel-rv-shell:
	@$(BUILD_TOOL) shell rv

kernel-la-shell:
	@$(BUILD_TOOL) shell la

disk.img:
	@$(BUILD_TOOL) disk rv

disk-la.img:
	@$(BUILD_TOOL) disk la

smoke-list:
	@./scripts/smoke.sh list

smoke:
	@test -n "$(NAME)" || { printf '%s\n' 'NAME is required, e.g. make smoke NAME=lwext4-io-boost ARCH=rv' >&2; exit 1; }
	@arch="$(ARCH)"; \
	case "$$arch" in \
		riscv64) arch=rv ;; \
		loongarch64) arch=la ;; \
	esac; \
	if [ "$(NAME)" = "phase9-la-depth-gate" ]; then \
		./scripts/smoke.sh "$(NAME)" $(SMOKE_ARGS); \
	else \
		./scripts/smoke.sh "$(NAME)" --arch "$$arch" $(SMOKE_ARGS); \
	fi

test-tools:
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/oscomp_eval -v
	@$(PYTHONPATH_CMD) python3 -m unittest discover -s tests/build_tools -v

.PHONY: help help-lab all artifacts kernels replay replay-rv replay-la shell-rv shell-la lab-list lab-explain lab-run smoke-list smoke test-tools dev-image dev-check dev-shell dev-shell-root clean clean-all legacy-clean prebuild-scrub check-eval-artifacts check-eval-kernel-size kernel-rv kernel-la kernel-rv-shell kernel-la-shell disk.img disk-la.img

check-eval-artifacts:
	@missing=0; \
	for artifact in \
		$(ROOT_DIR)/kernel-rv \
		$(ROOT_DIR)/kernel-la \
		$(ROOT_DIR)/disk.img \
		$(ROOT_DIR)/disk-la.img; \
	do \
		if [ ! -s "$$artifact" ]; then \
			printf 'missing evaluator artifact: %s\n' "$$artifact" >&2; \
			missing=1; \
		fi; \
		done; \
		exit "$$missing"

check-eval-kernel-size:
	@for kernel in $(ROOT_DIR)/kernel-rv $(ROOT_DIR)/kernel-la; do \
		[ -f "$$kernel" ] || continue; \
		size="$$(stat -c '%s' "$$kernel")"; \
		if [ "$$size" -gt "$(MAX_EVAL_KERNEL_BYTES)" ]; then \
			printf 'kernel artifact too large for evaluator: %s (%s bytes > %s)\n' "$$kernel" "$$size" "$(MAX_EVAL_KERNEL_BYTES)" >&2; \
			exit 1; \
		fi; \
	done
