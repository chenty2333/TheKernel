# Build Options
ARCH ?= riscv64
export ARCH
export PYTHONDONTWRITEBYTECODE ?= 1
LOG ?= off
export LOG
BANNER ?= n
export BANNER
BACKTRACE ?= n
export BACKTRACE
DEBUGINFO ?= y
export DEBUGINFO
DWARF ?= n
export DWARF
MEMTRACK ?= n
export MEMTRACK
OSCOMP_PLAN_OVERRIDE ?=
EVAL_NAME ?= full-local
EVAL_ARGS ?=
export OSKERNEL_DEV_IMAGE ?= thekernel-dev:local
DEV_ENV_DIR ?= $(ROOT_DIR)/dev-env
EMPTY_TESTSUITE_DIR ?= $(ROOT_DIR)/.state/empty-testsuites
AUTOSCRUB_DIRS ?= \
	$(ROOT_DIR)/.tmp \
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
STRIP_EVAL_ELF ?= y
SUPPORT_DISK_CACHE_DIR ?= $(STATE_DIR)/support-disk
KERNEL_CACHE_DIR ?= $(STATE_DIR)/kernel-cache

ifeq ($(MEMTRACK), y)
	APP_FEATURES += starry-api/memtrack
endif

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
		'  make clean        remove daily rebuild/replay artifacts; keep image cache and lab state' \
		'  make clean-all    full local clean, including all .state data' \
		'' \
		'Replay commands:' \
		'  make replay-rv    build rv artifacts, run all rv tests, judge, score, and report' \
		'  make replay-la    build la artifacts, run all la tests, judge, score, and report' \
		'' \
		'Boot commands:' \
		'  make shell-rv     build a shell-mode rv kernel, then boot an interactive shell' \
		'  make shell-la     build a shell-mode la kernel, then boot an interactive shell' \
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
	@rm -rf $(AUTOSCRUB_DIRS)
	@mkdir -p $(STATE_DIR)

clean: prebuild-scrub legacy-clean

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

clean-all:
	@$(MAKE) -C make clean
	@rm -rf $(STATE_DIR)
	@$(MAKE) --no-print-directory legacy-clean

build disasm: defconfig
	@$(MAKE) -C make $@

replay-rv: kernel-rv disk.img
	@PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}" python3 -m tools.oscomp_eval.replay replay --arch rv $(if $(IMAGE),--image $(IMAGE),) $(if $(PLAN),--plan $(PLAN),) $(if $(TIMEOUT),--timeout $(TIMEOUT),) $(if $(IDLE_TIMEOUT),--idle-timeout $(IDLE_TIMEOUT),) $(if $(VERBOSE),--verbose,)

replay-la: kernel-la disk-la.img
	@PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}" python3 -m tools.oscomp_eval.replay replay --arch la $(if $(IMAGE),--image $(IMAGE),) $(if $(PLAN),--plan $(PLAN),) $(if $(TIMEOUT),--timeout $(TIMEOUT),) $(if $(IDLE_TIMEOUT),--idle-timeout $(IDLE_TIMEOUT),) $(if $(VERBOSE),--verbose,)

shell-rv: kernel-rv-shell
	@PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}" python3 -m tools.oscomp_eval.replay shell --arch rv --kernel "$(STATE_DIR)/shell/kernel-rv" $(if $(IMAGE),--image $(IMAGE),) $(if $(TIMEOUT),--timeout $(TIMEOUT),) $(if $(VERBOSE),--verbose,)

shell-la: kernel-la-shell
	@PYTHONPATH="$(ROOT_DIR)$${PYTHONPATH:+:$$PYTHONPATH}" python3 -m tools.oscomp_eval.replay shell --arch la --kernel "$(STATE_DIR)/shell/kernel-la" $(if $(IMAGE),--image $(IMAGE),) $(if $(TIMEOUT),--timeout $(TIMEOUT),) $(if $(VERBOSE),--verbose,)

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

define kernel_cache_key
		{ \
			printf 'target=%s\n' "$@"; \
			printf 'arch=%s\n' "$(1)"; \
			printf 'make_args=%s\n' "$(2)"; \
			printf 'app_features=%s\n' "$(3)"; \
			printf 'output=%s\n' "$(5)"; \
			printf 'STRIP_EVAL_ELF=%s\n' "$(STRIP_EVAL_ELF)"; \
			printf 'DEBUGINFO=%s\n' "$(DEBUGINFO)"; \
			printf 'DWARF=%s\n' "$(DWARF)"; \
			printf 'LOG=%s\n' "$(LOG)"; \
			printf 'BANNER=%s\n' "$(BANNER)"; \
			printf 'BACKTRACE=%s\n' "$(BACKTRACE)"; \
			printf 'MEMTRACK=%s\n' "$(MEMTRACK)"; \
			printf 'NO_AXSTD=%s\n' "$(NO_AXSTD)"; \
			printf 'AX_LIB=%s\n' "$(AX_LIB)"; \
			printf 'BLK=%s\n' "$(BLK)"; \
			printf 'NET=%s\n' "$(NET)"; \
			printf 'VSOCK=%s\n' "$(VSOCK)"; \
			printf 'MEM=%s\n' "$(MEM)"; \
			printf 'rustc:\n'; rustc -Vv; \
			printf 'cargo:\n'; cargo -V; \
			if [ "$(STRIP_EVAL_ELF)" = y ]; then \
				printf 'rust-objcopy:\n'; rust-objcopy --version | sed -n '1,3p'; \
			fi; \
			for path in \
				"$(ROOT_DIR)/Cargo.toml" \
				"$(ROOT_DIR)/Cargo.lock" \
				"$(ROOT_DIR)/kernel/Cargo.toml" \
				"$(ROOT_DIR)/Makefile" \
				"$(ROOT_DIR)/$(4)" \
			; do \
				[ -f "$$path" ] || continue; \
				stat -c 'meta=%a %s %Y %n' "$$path"; \
				sha256sum "$$path"; \
			done; \
			if [ -f "$(ROOT_DIR)/.cargo/config.toml" ]; then \
				stat -c 'meta=%a %s %Y %n' "$(ROOT_DIR)/.cargo/config.toml"; \
				sha256sum "$(ROOT_DIR)/.cargo/config.toml"; \
			fi; \
			for dir in \
				"$(ROOT_DIR)/src" \
				"$(ROOT_DIR)/kernel/src" \
				"$(ROOT_DIR)/crates" \
				"$(ROOT_DIR)/third_party/rust-patches" \
				"$(ROOT_DIR)/make" \
			; do \
				[ -d "$$dir" ] || continue; \
				find "$$dir" -type f -print0 | sort -z | xargs -0 -r stat -c 'meta=%s %Y %n'; \
			done; \
		} | sha256sum | awk '{print $$1}'
endef

define build_kernel_artifact
	@set -e; \
	mkdir -p "$(KERNEL_CACHE_DIR)" "$(dir $(5))"; \
	key_file="$(KERNEL_CACHE_DIR)/$@.key"; \
	if [ -s "$(5)" ] && [ -f "$$key_file" ]; then \
		stale=0; \
		for path in \
			"$(ROOT_DIR)/Cargo.toml" \
			"$(ROOT_DIR)/Cargo.lock" \
			"$(ROOT_DIR)/kernel/Cargo.toml" \
			"$(ROOT_DIR)/Makefile" \
			"$(ROOT_DIR)/$(4)" \
			"$(ROOT_DIR)/.cargo/config.toml" \
		; do \
			[ -e "$$path" ] || continue; \
			if [ "$$path" -nt "$$key_file" ]; then stale=1; break; fi; \
		done; \
		if [ "$$stale" -eq 0 ]; then \
			for dir in \
				"$(ROOT_DIR)/src" \
				"$(ROOT_DIR)/kernel/src" \
				"$(ROOT_DIR)/crates" \
				"$(ROOT_DIR)/third_party/rust-patches" \
				"$(ROOT_DIR)/make" \
			; do \
				[ -d "$$dir" ] || continue; \
				if find "$$dir" -type f -newer "$$key_file" -print -quit | grep -q .; then \
					stale=1; \
					break; \
				fi; \
			done; \
		fi; \
		if [ "$$stale" -eq 0 ]; then \
			printf '%s\n' "$(5) is up to date"; \
			exit 0; \
		fi; \
	fi; \
	key="$$( $(call kernel_cache_key,$(1),$(2),$(3),$(4),$(5)) )"; \
	if [ -s "$(5)" ] && [ -f "$$key_file" ] && [ "$$key" = "$$(cat "$$key_file")" ]; then \
		printf '%s\n' "$(5) is up to date"; \
		exit 0; \
	fi; \
	$(MAKE) -C make ARCH="$(1)" $(2) APP_FEATURES="$(3)" defconfig; \
	$(MAKE) -C make ARCH="$(1)" $(2) APP_FEATURES="$(3)" build-elf-fast; \
	kernel="$$(find "$(STATE_DIR)/$(1)/out" -maxdepth 1 -name '*.elf' | head -n 1)"; \
	test -n "$$kernel"; \
	python3 "$(4)" "$$kernel" "$(5)"; \
	if [ "$(STRIP_EVAL_ELF)" = y ]; then \
		rust-objcopy --strip-all "$(5)" "$(5).stripped"; \
		mv "$(5).stripped" "$(5)"; \
	fi; \
	printf '%s\n' "$$key" > "$$key_file"; \
	$(MAKE) --no-print-directory check-eval-kernel-size
endef

kernel-rv:
	$(call build_kernel_artifact,riscv64,BUS=mmio,$(APP_FEATURES),scripts/patch-riscv-kernel-elf.py,$@)

kernel-la:
	$(call build_kernel_artifact,loongarch64,,$(APP_FEATURES),scripts/patch-loongarch-kernel-elf.py,$@)

kernel-rv-shell:
	$(call build_kernel_artifact,riscv64,BUS=mmio,qemu boot-shell,scripts/patch-riscv-kernel-elf.py,$(STATE_DIR)/shell/kernel-rv)

kernel-la-shell:
	$(call build_kernel_artifact,loongarch64,,qemu boot-shell,scripts/patch-loongarch-kernel-elf.py,$(STATE_DIR)/shell/kernel-la)

define build_support_disk
	@set -e; \
	mkdir -p "$(SUPPORT_DISK_CACHE_DIR)"; \
	if [ -n "$(OSCOMP_PLAN_OVERRIDE)" ] && [ ! -f "$(OSCOMP_PLAN_OVERRIDE)" ]; then \
		printf 'missing plan override: %s\n' "$(OSCOMP_PLAN_OVERRIDE)" >&2; \
		exit 1; \
	fi; \
	key="$$( \
		{ \
			printf 'arch=%s\n' "$(1)"; \
			printf 'plan=%s\n' "$(OSCOMP_PLAN_OVERRIDE)"; \
			printf 'OSCOMP_RV_CC=%s\n' "$${OSCOMP_RV_CC:-}"; \
			printf 'OSCOMP_LA_CC=%s\n' "$${OSCOMP_LA_CC:-}"; \
			printf 'OSCOMP_LA_GLIBC_CC=%s\n' "$${OSCOMP_LA_GLIBC_CC:-}"; \
			printf 'OSCOMP_RV_LIBGCC=%s\n' "$${OSCOMP_RV_LIBGCC:-}"; \
			printf 'OSCOMP_LA_LIBGCC=%s\n' "$${OSCOMP_LA_LIBGCC:-}"; \
			stat -c 'meta=%a %s %n' "$(ROOT_DIR)/scripts/build-oscomp-support-disk.sh" "$(ROOT_DIR)/ltp_test.txt"; \
			sha256sum "$(ROOT_DIR)/scripts/build-oscomp-support-disk.sh" "$(ROOT_DIR)/ltp_test.txt"; \
			if [ -n "$(OSCOMP_PLAN_OVERRIDE)" ]; then \
				stat -c 'meta=%a %s %n' "$(OSCOMP_PLAN_OVERRIDE)"; \
				sha256sum "$(OSCOMP_PLAN_OVERRIDE)"; \
			fi; \
			find "$(ROOT_DIR)/scripts/support-tools" "$(ROOT_DIR)/scripts/support-overlay" -type f -print0 | sort -z | xargs -0 -r stat -c 'meta=%a %s %n'; \
			find "$(ROOT_DIR)/scripts/support-tools" "$(ROOT_DIR)/scripts/support-overlay" -type f -print0 | sort -z | xargs -0 -r sha256sum; \
			if [ -d "$(STATE_DIR)/ltp-lab/refs/testsuits-for-oskernel/scripts" ]; then \
				find "$(STATE_DIR)/ltp-lab/refs/testsuits-for-oskernel/scripts" -type f -print0 | sort -z | xargs -0 -r stat -c 'meta=%a %s %n'; \
				find "$(STATE_DIR)/ltp-lab/refs/testsuits-for-oskernel/scripts" -type f -print0 | sort -z | xargs -0 -r sha256sum; \
			fi; \
		} | sha256sum | awk '{print $$1}' \
	)"; \
	key_file="$(SUPPORT_DISK_CACHE_DIR)/$@.key"; \
	if [ -s "$@" ] && [ -f "$$key_file" ] && [ "$$key" = "$$(cat "$$key_file")" ]; then \
		printf '%s\n' "$@ is up to date"; \
		exit 0; \
	fi; \
	if [ -s "$@" ] && [ ! -f "$$key_file" ] && ./scripts/oscomp.sh support-check --arch "$(1)" --image "$@" >/dev/null; then \
		printf '%s\n' "$$key" > "$$key_file"; \
		printf '%s\n' "$@ is up to date"; \
		exit 0; \
	fi; \
	set -- bash ./scripts/build-oscomp-support-disk.sh --arch "$(1)" --output "$@"; \
		if [ -n "$(OSCOMP_PLAN_OVERRIDE)" ]; then \
			set -- "$$@" --plan-override "$(OSCOMP_PLAN_OVERRIDE)"; \
		fi; \
	"$$@"; \
	printf '%s\n' "$$key" > "$$key_file"
endef

disk.img:
	$(call build_support_disk,rv)

disk-la.img:
	$(call build_support_disk,la)

.PHONY: help help-lab all artifacts kernels build replay-rv replay-la shell-rv shell-la lab-list lab-explain lab-run dev-image dev-check dev-shell dev-shell-root disasm clean clean-all legacy-clean prebuild-scrub check-eval-artifacts check-eval-kernel-size kernel-rv kernel-la kernel-rv-shell kernel-la-shell disk.img disk-la.img
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
	@./scripts/oscomp.sh support-check --arch rv --image "$(ROOT_DIR)/disk.img"
	@./scripts/oscomp.sh support-check --arch la --image "$(ROOT_DIR)/disk-la.img"

check-eval-kernel-size:
	@for kernel in $(ROOT_DIR)/kernel-rv $(ROOT_DIR)/kernel-la; do \
		[ -f "$$kernel" ] || continue; \
		size="$$(stat -c '%s' "$$kernel")"; \
		if [ "$$size" -gt "$(MAX_EVAL_KERNEL_BYTES)" ]; then \
			printf 'kernel artifact too large for evaluator: %s (%s bytes > %s)\n' "$$kernel" "$$size" "$(MAX_EVAL_KERNEL_BYTES)" >&2; \
			exit 1; \
		fi; \
	done
