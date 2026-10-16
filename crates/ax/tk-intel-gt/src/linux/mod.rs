// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI bindings shared by the source-order i915 translations.
//!
//! Keep APIs grouped by the Linux header/subsystem they model. These are
//! target-bound kernel primitives, not host compatibility shims.

pub(crate) use crate::{
    linux_assert as assertion, linux_config as config, linux_heap as heap,
    linux_i915_private as i915_private, linux_list as list, linux_locks as locks,
    linux_memory as memory, linux_mutex as mutex, linux_pm as pm, linux_print as print,
    linux_sw_fence as sw_fence, linux_tasklet as tasklet, linux_timer as timer, linux_wait as wait,
    linux_workqueue as workqueue, linux_xarray as xarray,
};
pub(crate) mod average;
pub use tk_linux_kapi::bitmap;
pub use tk_linux_kapi::bits;
pub(crate) mod contexts;
pub(crate) mod cpufreq;
pub(crate) mod fields;
pub(crate) mod wait_var;
pub use tk_linux_kapi::firmware;
pub mod gem;
pub mod gem_memory;
#[allow(unsafe_code)]
pub mod highmem;
pub mod i915;
pub(crate) mod i915_trace;
pub use tk_linux_kapi::idr;
pub(crate) mod iosys_map;
pub(crate) mod irq;
pub use tk_linux_kapi::mm;
#[allow(unsafe_code)]
pub(crate) mod page;
pub mod primitives;
pub use tk_linux_kapi::drm_mm;
// drm_vma now ported in tk-drm-upstream
pub mod drm_core;
pub use tk_linux_kapi::rbtree;
pub use tk_linux_kapi::rcu;
pub use tk_linux_kapi::scatterlist;
pub use tk_linux_kapi::seq_file;
pub(crate) mod registers;
pub(crate) mod requests;
#[allow(unsafe_code)]
pub(crate) mod user_extensions;
pub use tk_linux_kapi::signal;
pub use tk_linux_kapi::srcu;
pub use tk_linux_kapi::ww_mutex;

pub use tk_linux_kapi::rcu_work;

pub use tk_linux_kapi::mmu_notifier;

pub use tk_linux_kapi::mm_native;

pub use tk_linux_kapi::shmem;

pub use tk_linux_kapi::vm;

pub use tk_linux_kapi::iomapping;

pub use tk_linux_kapi::dma;
pub mod ggtt_owner;

pub use tk_linux_kapi::kernel_services;

/// Linux core APIs with kernel-installed PCI/DMA providers (see PROVIDERS.md).
pub use tk_linux_kapi::kernel_core;
pub mod ttm;
