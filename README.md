# TheKernel

TheKernel is an OSComp 2026 operating-system design entry: a Rust-based
kernel for the RISC-V and LoongArch evaluator targets, with Linux ABI
compatibility for the official test suite.

## OSComp 2026 Documents

- Technical report: [docs/oscomp2026_report.pdf](docs/oscomp2026_report.pdf)
- Presentation slides: [docs/oscomp2026_slides.pdf](docs/oscomp2026_slides.pdf)

## Project

- Author: 陈天意 <hi@tychen.cc>
- Upstream baseline: StarryOS commit
  [`2e075accf4fb0aefdd1d252ebd9ccf29727d9923`](https://github.com/Starry-OS/StarryOS/tree/2e075accf4fb0aefdd1d252ebd9ccf29727d9923).
- Source code license: Apache License 2.0, see [LICENSE](LICENSE) and
  [NOTICE](NOTICE).
- Technical documents, presentation slides, and other defense materials:
  Creative Commons Attribution-ShareAlike 4.0 International
  ([CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/)).

Third-party code under `third_party/` and vendored patch directories keeps its
upstream copyright notices and license terms.

## Development

Build, boot, and replay commands should run inside the repo development
container. The host may not have the RISC-V or LoongArch cross toolchains; a
host-side build failure such as `no usable cross toolchain found for
architecture riscv64` means the command was run outside the dev shell.

Build the development container image once:

```bash
make dev-image
```

Check the toolchain contract inside the image:

```bash
make dev-check
```

Open a development shell:

```bash
make dev-shell
```

Run one command inside the development shell without staying in it:

```bash
make dev-shell DEV_CMD='make kernels'
```

Open the privileged builder service when needed:

```bash
make dev-shell-root
```

## Build

Official evaluator artifact build, from the host:

```bash
make dev-shell DEV_CMD='make all'
```

The same build from inside an already-open `make dev-shell`:

```bash
make all
```

Both forms produce the evaluator artifacts at the repository root:

- `kernel-rv`
- `kernel-la`
- `disk.img`
- `disk-rv.img`
- `disk-la.img`

Local iteration can use narrower commands:

```bash
make dev-shell DEV_CMD='make kernels'
make dev-shell DEV_CMD='make artifacts'
make dev-shell DEV_CMD='make kernel-rv'
make dev-shell DEV_CMD='make kernel-la'
```

Inside an already-open `make dev-shell`, run those inner `make ...` commands
directly.

## Replay

Replay the evaluator flow from the host:

```bash
make dev-shell DEV_CMD='make replay-rv'
make dev-shell DEV_CMD='make replay-la'
```

Rebuild the matching kernel before replaying:

```bash
make dev-shell DEV_CMD='make eval-rv'
make dev-shell DEV_CMD='make eval-la'
```

Inside an already-open `make dev-shell`, run the inner commands directly:

```bash
make replay-rv
make replay-la
make eval-rv
make eval-la
```

Validate official image layout inside the dev shell:

```bash
./scripts/oscomp.sh verify --arch rv
./scripts/oscomp.sh verify --arch la
```

## Boot

Boot an interactive local shell instead of the evaluator replay plan, from the
host:

```bash
make dev-shell DEV_CMD='make boot-rv'
make dev-shell DEV_CMD='make boot-la'
```

These targets build the matching kernel and a local support disk that sets
`OSCOMP_BOOT_SHELL=1`. That boot-only variable makes `src/init.sh` enter a
BusyBox shell; evaluator builds and replay runs do not set it.

Inside an already-open `make dev-shell`, run `make boot-rv` or `make boot-la`
directly. Exit the guest shell with `exit`; the kernel then powers off.

## Notes

- Kernel behavior lives mainly under `kernel/`.
- `src/init.sh` is guest-side runner logic.
- Runtime, replay, and lab state live under `.state`.
- OpenSpec changes should hold task-specific plans, investigations, and
  implementation notes.
