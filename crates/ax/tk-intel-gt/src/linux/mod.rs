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
pub(crate) mod bitmap;
pub(crate) mod bits;
pub(crate) mod contexts;
#[cfg(feature = "upstream-gt")]
pub(crate) mod cpufreq;
pub(crate) mod fields;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod firmware;
pub(crate) mod gem;
pub(crate) mod gem_memory;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod highmem;
pub mod i915;
pub(crate) mod i915_trace;
pub(crate) mod idr;
pub(crate) mod iosys_map;
pub(crate) use idr::Ida;
pub(crate) use iosys_map::{IosysMap, IosysMapAddr};
pub(crate) mod irq;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod mm;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod page;
pub mod primitives;
pub(crate) mod drm_mm;
pub(crate) mod drm_vma;
pub(crate) mod dma_fence_core;
pub mod drm_core;
pub(crate) mod rbtree;
pub(crate) mod rcu;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod scatterlist;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod seq_file;
pub(crate) mod registers;
pub(crate) mod requests;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod user_extensions;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod signal;
pub(crate) mod srcu;
pub(crate) mod ww_mutex;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod rcu_work;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod mmu_notifier;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod mm_native;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod shmem;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod vm;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod iomapping;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod dma;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod kernel_services;

/// Linux core APIs with kernel-installed PCI/DMA providers (see PROVIDERS.md).
pub mod kernel_core;
