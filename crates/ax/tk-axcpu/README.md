# axcpu

[![Crates.io](https://img.shields.io/crates/v/axcpu)](https://crates.io/crates/axcpu)
[![Docs.rs](https://docs.rs/axcpu/badge.svg)](https://docs.rs/axcpu)
[![CI](https://github.com/arceos-org/axcpu/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/arceos-org/axcpu/actions/workflows/ci.yml)

This crate provides privileged instruction and structure abstractions for x86_64. It is designed to implement the hardware abstraction layer of an operating system kernel.

## Supported Architecture

* x86_64

## TheKernel integration

This package targets x86_64 and uses `nightly-2026-08-23`
(`rustc 1.100.0-nightly`, `c54751567`, 2026-08-22); the manifest
Rust version does not promise stable-compiler support. Kernel consumers
should check against `x86_64-unknown-none`. Platform initialization and any
required per-CPU/linker symbols belong to the final kernel image; successful
library compilation alone does not validate that image integration.
