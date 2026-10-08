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
pub(crate) mod bits;
pub(crate) mod contexts;
pub(crate) mod fields;
pub(crate) mod gem;
pub(crate) mod gem_memory;
pub(crate) mod i915;
pub(crate) mod irq;
pub(crate) mod primitives;
pub(crate) mod rbtree;
pub(crate) mod rcu;
pub(crate) mod registers;
pub(crate) mod requests;
