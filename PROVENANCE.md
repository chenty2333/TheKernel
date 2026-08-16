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

`dev-env/versions.env` and `dev-env/Dockerfile` pin the development image,
QEMU source release, and Rust toolchain by version and checksum. These are
external build tools, not source incorporated into TheKernel.

## Generated test root filesystem

The images produced by `make test-fixtures` (or its `make rootfs` alias) are
local and CI test fixtures. They are not part of the published kernel release
artifact set.

The builder downloads BusyBox 1.36.1 from
<https://busybox.net/downloads/busybox-1.36.1.tar.bz2> and requires SHA-256
`b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314`.
BusyBox is GPL-2.0-only; its license is installed in every generated image. The
test init and helpers are statically linked with the C runtime supplied by the
selected cross toolchain.

Anyone redistributing a generated image must preserve its notices and satisfy
the corresponding-source requirements for BusyBox and the license obligations
of the selected C runtime. The repository's release artifact policy excludes
these generated rootfs images.
