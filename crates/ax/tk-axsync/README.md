# axsync

[![Crates.io](https://img.shields.io/crates/v/axsync)](https://crates.io/crates/axsync)
[![Docs.rs](https://docs.rs/axsync/badge.svg)](https://docs.rs/axsync)

[ArceOS](https://github.com/arceos-org/arceos) synchronization primitives.

## Primitives

- **Mutex**: A mutual exclusion primitive. With the `multitask` feature, it uses task-aware locking; otherwise it is an alias of `kspin::SpinNoIrq`.
- **spin**: Re-export of the [kspin](https://crates.io/crates/kspin) crate (spinlocks).

## Features

- `multitask`: Enable multi-threaded support. When enabled, `Mutex` uses blocking that cooperates with the task scheduler; when disabled, `Mutex` is a spinlock.

## License

This project is licensed under GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0.

## TheKernel integration

This package targets x86_64 and uses `nightly-2026-08-23`
(`rustc 1.100.0-nightly`, `c54751567`, 2026-08-22); the manifest
Rust version does not promise stable-compiler support. Kernel consumers
should check against `x86_64-unknown-none`. Platform initialization and any
required per-CPU/linker symbols belong to the final kernel image; successful
library compilation alone does not validate that image integration.
