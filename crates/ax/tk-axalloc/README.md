# axalloc

ArceOS global memory allocator. Provides [`GlobalAllocator`] implementing [`core::alloc::GlobalAlloc`] for use with `#[global_allocator]`.

Uses the [axallocator](https://docs.rs/axallocator) crate for the underlying byte and page allocators (TLSF, slab, buddy, bitmap).

## Features

- `tlsf` (default) – TLSF byte allocator
- `slab` – slab byte allocator
- `buddy` – buddy byte allocator
- `page-alloc-256m` (default), `page-alloc-4g`, `page-alloc-64g` – page allocator capacity
- `tracking` – allocation tracking (requires `percpu`, `axbacktrace`)

## License

GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0

## TheKernel integration

This package targets x86_64 and uses `nightly-2026-08-23`
(`rustc 1.100.0-nightly`, `c54751567`, 2026-08-22); the manifest
Rust version does not promise stable-compiler support. Kernel consumers
should check against `x86_64-unknown-none`. Platform initialization and any
required per-CPU/linker symbols belong to the final kernel image; successful
library compilation alone does not validate that image integration.
