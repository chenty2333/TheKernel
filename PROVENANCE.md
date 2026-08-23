# Source and Build Provenance

This file records source lineage and build inputs that must remain available
independently of release or project-history presentation.

## Kernel lineage

TheKernel began from the StarryOS source tree at commit
[`2e075accf4fb0aefdd1d252ebd9ccf29727d9923`](https://github.com/Starry-OS/StarryOS/tree/2e075accf4fb0aefdd1d252ebd9ccf29727d9923)
and continues to use ArceOS components. `NOTICE` preserves upstream authorship
at the repository level. Files and packages under `third_party/` retain their
own license texts and source records.

The patched Rust package set has a stricter, package-level record under
`third_party/rust-patches/`. Its `PROVENANCE.md`, `PROVENANCE.toml`, and each
package's `VENDOR.md` identify immutable upstream archives or VCS revisions,
license status, and maintained differences.

## Development toolchains

The root [`rust-toolchain.toml`](rust-toolchain.toml) is the sole Rust
toolchain authority for TheKernel product builds and CI: the rustup snapshot is
`nightly-2026-08-23`, corresponding to
`rustc 1.100.0-nightly` (`c54751567`, commit date 2026-08-22), with the
declared components and `x86_64-unknown-none` target. Rust is installed by
rustup from that repository pin.

`dev-env/Dockerfile` defines only the system build dependencies and pinned QEMU
source used by the development image. Those external build tools are not source
incorporated into TheKernel. CI consumes that image through the repository
variable `THEKERNEL_DEV_IMAGE`, which must contain an immutable digest reference;
the moving publish tags are discovery names, not CI inputs.

## Generated test root filesystem

The image produced by `./tools/thekernel.py rootfs` is a local and CI test
fixture.

The builder downloads BusyBox 1.36.1 from
<https://busybox.net/downloads/busybox-1.36.1.tar.bz2> and requires SHA-256
`b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314`.
BusyBox is GPL-2.0-only; its license is installed in every generated image. The
test init and helpers are statically linked with the C runtime supplied by the
selected cross toolchain.

Anyone redistributing a generated image must preserve its notices and satisfy
the corresponding-source requirements for BusyBox and the license obligations
of the selected C runtime.
