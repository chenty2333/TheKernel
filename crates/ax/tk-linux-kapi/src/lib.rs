// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Generic Linux kernel-API shims (LinuxKPI layer) for the TheKernel GPU ports.
//!
//! This crate sits below every Linux DRM / i915 / virtio-gpu port and above the
//! mechanism and platform services (HAL, task runtime, allocator, mm). It holds
//! only generic kernel APIs: list and rbtree helpers, wait queues, workqueues,
//! timers, locks, completions, dma-fence core, dma mapping, firmware loading,
//! iomapping and printk. GPU-driver object types stay in the owning crate.
#![no_std]
#![feature(c_variadic)]
#![deny(unsafe_code)]
#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::duplicated_attributes,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::macro_metavars_in_unsafe,
    clippy::never_loop,
    clippy::absurd_extreme_comparisons,
    clippy::swap_ptr_to_ref,
    clippy::erasing_op,
    clippy::empty_line_after_doc_comments
)]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod types;

pub mod config;
#[allow(unsafe_code)]
pub mod bits;
#[allow(unsafe_code)]
pub mod firmware;
#[allow(unsafe_code)]
pub mod heap;
#[allow(unsafe_code)]
pub mod assert;
#[allow(unsafe_code)]
pub mod print;
#[allow(unsafe_code)]
pub mod list;
#[allow(unsafe_code)]
pub mod memory;
#[allow(unsafe_code)]
pub mod locks;
#[allow(unsafe_code)]
pub mod xarray;
#[allow(unsafe_code)]
pub mod wait;
#[allow(unsafe_code)]
pub mod tasklet;
#[allow(unsafe_code)]
pub mod mutex;
#[allow(unsafe_code)]
pub mod timer;
#[allow(unsafe_code)]
pub mod workqueue;
#[allow(unsafe_code)]
pub mod mm;
#[allow(unsafe_code)]
pub mod mm_native;
#[allow(unsafe_code)]
pub mod idr;
#[allow(unsafe_code)]
pub mod rbtree;
#[allow(unsafe_code)]
pub mod rcu;
#[allow(unsafe_code)]
pub mod rcu_work;
#[allow(unsafe_code)]
pub mod srcu;
#[allow(unsafe_code)]
pub mod signal;
#[allow(unsafe_code)]
pub mod scatterlist;
#[allow(unsafe_code)]
pub mod seq_file;
#[allow(unsafe_code)]
pub mod dma;
#[allow(unsafe_code)]
pub mod drm_mm;
#[allow(unsafe_code)]
pub mod iomapping;
#[allow(unsafe_code)]
pub mod vm;
#[allow(unsafe_code)]
pub mod mmu_notifier;
#[allow(unsafe_code)]
pub mod ww_mutex;
#[allow(unsafe_code)]
pub mod kernel_services;
#[allow(unsafe_code)]
pub mod kernel_core;
#[allow(unsafe_code)]
pub mod fields;
#[allow(unsafe_code)]
pub mod bitmap;
#[allow(unsafe_code)]
pub mod gem_memory;
#[allow(unsafe_code)]
pub mod primitives;
#[allow(unsafe_code)]
pub mod shmem;
#[allow(unsafe_code)]
pub mod page_cache;
#[allow(unsafe_code)]
pub mod clock;
#[allow(unsafe_code)]
pub mod completion;
#[allow(unsafe_code)]
pub mod seq_buf;
#[allow(unsafe_code)]
pub mod iopoll;
#[allow(unsafe_code)]
pub mod i2c;
#[allow(unsafe_code)]
pub mod i2c_algo_bit;
#[allow(unsafe_code)]
pub mod dma_fence_chain;
#[allow(unsafe_code)]
pub mod pci;
#[allow(unsafe_code)]
pub mod acpi;
#[allow(unsafe_code)]
pub mod backlight;
#[allow(unsafe_code)]
pub mod pwm;
#[allow(unsafe_code)]
pub mod component;
#[allow(unsafe_code)]
pub mod vga;
#[allow(unsafe_code)]
pub mod debugfs;
