# Build and host boundary

This document is the short version of TheKernel's build contract. It exists to
keep the repository from growing a second build framework or a host daemon.

## One path

```text
make / scripts/dev-run.sh
          │
          ▼
  dev-env/compose.yaml (one `dev` service, `run --rm`)
          │
          ▼
    tools/thekernel.py
          │
          ├── scripts/build-guest-tools.sh
          ├── scripts/build-rootfs.sh
          ├── scripts/build-graphics-rootfs.sh
          ├── scripts/build-x86-uefi-esp.sh
          └── Cargo / QEMU / verification suites
```

`tools/thekernel.py` owns command selection, artifact identity, locks, and
build/boot/test sequencing. The shell scripts below it each build one artifact
and are not parallel user-facing workflows. `scripts/dev-run.sh` is the host
boundary: from a host it enters the checked-in image; from inside that image it
executes directly rather than nesting containers.

The `hardware` verification tier is intentionally the exception. It runs on a
KVM runner with the host toolchain because the runner itself is the device under
test. It still uses the same `tools/thekernel.py` entry point and state layout.

## State and storage

| Data | Default location | Lifetime |
| --- | --- | --- |
| Rust toolchain and Cargo cache in the dev image | Docker volume `thekernel-dev_thekernel-home` | Persistent, regenerable |
| Product outputs and run logs in the dev image | `/home/thekernel/.cache/thekernel-targets` | Persistent, regenerable |
| Host hardware-tier outputs | `~/.cache/thekernel-targets` | Persistent, regenerable |
| Desktop user home disk | `~/.local/share/thekernel/desktop/` | User data; do not clean automatically |

The project rejects `/tmp` and `/dev/shm` for persistent artifacts. `make
docker-clean` deliberately removes only the named development volume and local
image; it does not prune unrelated Docker data.

## Container safety invariants

The development container is an execution boundary, not a service:

* Compose defines one `dev` service and `restart: "no"`.
* Commands use `docker compose run --rm`; there is no `up`, boot-time unit, or
  detached container in the repository.
* The container uses the normal bridge network. It never uses `network: host`
  and never configures a physical interface, DHCP server, TFTP server, or DNS
  daemon on the host.
* The old privileged `builder` service is gone. Local KVM and VirGL are passed
  through only when their device nodes already exist (`/dev/kvm` and
  `/dev/dri`); CI without those devices uses TCG/software rendering.
* `make host-cleanup` is a narrow repair tool for installations that predate
  this contract. It only recognizes the exact historical names
  `thekernel-boot`, `thekernel-netboot`, `thekernel-boot.service`, and
  `thekernel-netboot.service`/`.timer`.

If an old host installation still contains one of those entries, first run
`scripts/host-cleanup.sh` to inspect it, then rerun with `--fix`. The script
does not alter unrelated containers, services, or NetworkManager profiles.

## Adding a new build step

Add it to `tools/thekernel.py` and its matching verification/tests first. Keep
large generated files below the state root, acquire the existing build lock,
and expose it through `make` only by calling `scripts/dev-run.sh`. Do not add a
second Compose service, a background container, a host-network mode, or a
systemd autostart unit for a build or boot experiment.
