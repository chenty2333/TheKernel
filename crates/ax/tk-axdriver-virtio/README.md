# axdriver_virtio

Wrappers of devices in the [virtio-drivers](https://docs.rs/virtio-drivers) crate that implement traits from the axdriver_* crates. For use in `no_std` environments.

Part of the [axdriver_crates](https://github.com/arceos-org/axdriver_crates) workspace.

## Features

- `alloc` – enable allocator support in virtio-drivers
- `block` – VirtIO block device (requires `axdriver_block`)
- `gpu` – VirtIO GPU (requires `axdriver_display`)
- `net` – VirtIO net (requires `axdriver_net`)

## License

GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0. See repository root LICENSE.

## TheKernel integration

This package targets x86_64 and uses `nightly-2026-08-23`
(`rustc 1.100.0-nightly`, `c54751567`, 2026-08-22); the manifest
Rust version does not promise stable-compiler support. Kernel consumers
should check against `x86_64-unknown-none`. Platform initialization and any
required per-CPU/linker symbols belong to the final kernel image; successful
library compilation alone does not validate that image integration.
