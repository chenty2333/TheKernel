# Thin developer entry points. Product build and boot behavior lives in
# tools/thekernel.py. Every target that needs the toolchain goes through
# scripts/dev-run.sh, so a host shell and an entered development shell use the
# same image, cache, limits, and dependency set.

# Matches the product default in tools/thekernel.py; --smp only bounds the
# --run-cpus ceiling, so the default run still boots RUN_CPUS processors.
SMP ?= 4
RUN_CPUS ?= $(SMP)
# Matches the product default in tools/thekernel.py; 512M guests also boot
# but run with a much smaller per-file page cache tier.
MEMORY ?= 1G
ACCEL ?= kvm
# Wall-clock kill switch for forgotten guests. One hour keeps an interactive
# shell session from dying mid-use while still reaping abandoned runs.
TIMEOUT ?= 3600
# 2 parallel rustc jobs stay well inside the 8G scope limit on the reference
# development host while keeping cold builds tolerable.
CARGO_JOBS ?= 2
SUITE ?= host
TIER ?= daily
RUN_ARGS ?=
# The desktop also builds WebKit, whose compiler needs a larger container
# budget. The compose service is still short-lived and never auto-started.
DEV_MEMORY ?= 8g

DEV_RUN = ./scripts/dev-run.sh
# `THEKERNEL_STATE_DIR` is a host-only override used by a few hardware and
# research helpers. It must not leak into the container, whose cache lives in
# its named HOME volume.
DEV_ENV = env -u THEKERNEL_STATE_DIR THEKERNEL_DEV_MEMORY=$(DEV_MEMORY) CARGO_BUILD_JOBS=$(CARGO_JOBS)

.PHONY: run run-gui run-existing bootstrap build lint lint-strict test bench verify clean \
	docker-clean host-cleanup

# The default image carries no tool payload, so the guest shell has no
# compiler; boot the distribution-compiler image with
# `make run RUN_ARGS="--toolchain gcc"` (the first build downloads ~100 MiB
# of pinned RPMs).
run:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py run --profile shell --interactive \
		--smp $(SMP) --run-cpus $(RUN_CPUS) --memory $(MEMORY) \
		--accel $(ACCEL) --timeout $(TIMEOUT) $(RUN_ARGS)

run-existing:
	$(MAKE) run RUN_ARGS="--no-build $(RUN_ARGS)"

# Reuse desktop images; RUN_ARGS=--build updates them before starting.
run-gui: MEMORY = 2G
run-gui: DEV_MEMORY = 16g
run-gui:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py run-gui \
		--smp $(SMP) --run-cpus $(RUN_CPUS) --memory $(MEMORY) \
		--accel $(ACCEL) --timeout $(TIMEOUT) $(RUN_ARGS)

bootstrap:
	$(DEV_ENV) $(DEV_RUN) ./scripts/setup-toolchain.sh

build:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py build --smp $(SMP) --memory $(MEMORY)

lint:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py lint --smp $(SMP) --memory $(MEMORY)

# The escalation path, developer-only.  `lint` above is what CI enforces: four
# packages, two denied Clippy groups, no `-D warnings`.  This runs every member
# on the host target and promotes every warning the in-code allowances do not
# cover, which the tree does not pass today -- run it to size that work, not to
# gate it. It still uses the same development container as the regular lint
# gate.
lint-strict:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py lint --workspace --deny-warnings

test:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py test --suite $(SUITE) $(RUN_ARGS)

bench: SUITE = all
bench:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py bench --suite $(SUITE) $(RUN_ARGS)

verify:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py verify --tier $(TIER)

clean:
	$(DEV_ENV) $(DEV_RUN) ./tools/thekernel.py clean

host-cleanup:
	./scripts/host-cleanup.sh --fix

# Also deletes the named volume thekernel-home (the container-side rustup
# toolchain) and the locally built development image.
docker-clean:
	docker compose -f dev-env/compose.yaml down -v --remove-orphans
	-docker image rm thekernel-dev:local
