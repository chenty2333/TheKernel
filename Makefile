# Thin developer entry points.  Product build and boot behavior lives in
# tools/thekernel.py; these defaults only keep ordinary interactive runs safe
# on the host.

# Matches the product default in tools/thekernel.py; --smp only bounds the
# --run-cpus ceiling, so the default run still boots RUN_CPUS processors.
SMP ?= 4
RUN_CPUS ?= 1
# Matches the product default in tools/thekernel.py; 512M guests also boot
# but run with a much smaller per-file page cache tier.
MEMORY ?= 1G
ACCEL ?= kvm
# Wall-clock kill switch for forgotten guests.  One hour keeps an interactive
# shell session from dying mid-use while still reaping abandoned runs.
TIMEOUT ?= 3600
STATE_DIR ?= $(HOME)/.cache/thekernel-targets
# 4 parallel rustc jobs stay well inside the 8G scope limit on the reference
# development host while keeping cold builds tolerable.
CARGO_JOBS ?= 4
RUN_ARGS ?=

.PHONY: run run-existing build lint test clean docker-clean

run:
	systemd-run --user --scope --quiet --collect \
		-p MemoryMax=8G \
		-p MemorySwapMax=0 \
		-p OOMPolicy=stop \
		env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
		THEKERNEL_STATE_DIR="$(STATE_DIR)" \
		./tools/thekernel.py run --profile shell --interactive \
		--smp $(SMP) --run-cpus $(RUN_CPUS) --memory $(MEMORY) \
		--accel $(ACCEL) --timeout $(TIMEOUT) $(RUN_ARGS)

run-existing:
	$(MAKE) run RUN_ARGS="--no-build $(RUN_ARGS)"

build:
	env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py build --smp $(SMP) --memory $(MEMORY)

lint:
	env CARGO_BUILD_JOBS=$(CARGO_JOBS) \
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py lint --smp $(SMP) --memory $(MEMORY)

test:
	python3 -m unittest discover -s tests -t .
	cargo test --locked -p thekernel-readiness-adapter
	cargo test --locked -p thekernel-linux-process-adapter

clean:
	THEKERNEL_STATE_DIR="$(STATE_DIR)" \
	./tools/thekernel.py clean

# Also deletes the named volume thekernel-home (the container-side rustup
# toolchain) and the locally built development image.
docker-clean:
	docker compose -f dev-env/compose.yaml down -v --remove-orphans
	-docker image rm thekernel-dev:local
