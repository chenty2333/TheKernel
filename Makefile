# Thin developer entry points.  Product build and boot behavior lives in
# tools/thekernel.py; these defaults only keep ordinary interactive runs safe
# on the host.

# Matches the product default in tools/thekernel.py; --smp only bounds the
# --run-cpus ceiling, so the default run still boots RUN_CPUS processors.
SMP ?= 4
RUN_CPUS ?= $(SMP)
# Matches the product default in tools/thekernel.py; 512M guests also boot
# but run with a much smaller per-file page cache tier.
MEMORY ?= 1G
ACCEL ?= kvm
# Wall-clock kill switch for forgotten guests.  One hour keeps an interactive
# shell session from dying mid-use while still reaping abandoned runs.
TIMEOUT ?= 3600
STATE_DIR ?= $(HOME)/.cache/thekernel-targets
# 2 parallel rustc jobs stay well inside the 8G scope limit on the reference
# development host while keeping cold builds tolerable.
CARGO_JOBS ?= 2
SUITE ?= host
RUN_ARGS ?=
# Every build-capable entry point shares the same host memory budget.
RESOURCE_SCOPE = systemd-run --user --scope --quiet --collect \
	-p MemoryMax=8G -p MemorySwapMax=0 -p OOMPolicy=stop

.PHONY: run run-gui run-existing build lint test bench clean docker-clean

run:
	$(RESOURCE_SCOPE) \
		env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
		THEKERNEL_STATE_DIR="$(STATE_DIR)" \
		./tools/thekernel.py run --profile shell --interactive \
		--smp $(SMP) --run-cpus $(RUN_CPUS) --memory $(MEMORY) \
		--accel $(ACCEL) --timeout $(TIMEOUT) $(RUN_ARGS)

run-existing:
	$(MAKE) run RUN_ARGS="--no-build $(RUN_ARGS)"

run-gui:
	$(RESOURCE_SCOPE) \
		env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
		THEKERNEL_STATE_DIR="$(STATE_DIR)" \
		./tools/thekernel.py run-gui \
		--smp $(SMP) --run-cpus $(RUN_CPUS) --memory $(MEMORY) \
		--accel $(ACCEL) --timeout $(TIMEOUT) $(RUN_ARGS)

build:
	$(RESOURCE_SCOPE) env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py build --smp $(SMP) --memory $(MEMORY)

lint:
	$(RESOURCE_SCOPE) env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py lint --smp $(SMP) --memory $(MEMORY)

test:
	$(RESOURCE_SCOPE) env CARGO_BUILD_JOBS=$(CARGO_JOBS) THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py test --suite $(SUITE) $(RUN_ARGS)

bench: SUITE = all
bench:
	$(RESOURCE_SCOPE) env CARGO_BUILD_JOBS=$(CARGO_JOBS) THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py bench --suite $(SUITE) $(RUN_ARGS)

clean:
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py clean

# Also deletes the named volume thekernel-home (the container-side rustup
# toolchain) and the locally built development image.
docker-clean:
	docker compose -f dev-env/compose.yaml down -v --remove-orphans
	-docker image rm thekernel-dev:local
