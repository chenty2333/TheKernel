// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Layout-only LinuxKPI binding for the kernel's deferred RCU-work handle.
//! This does not provide a workqueue/RCU implementation; execution is owned by
//! the runtime's kernel-work integration.

use core::ffi::c_void;

use crate::{intel_context_upstream::RcuHead, intel_engine_cs_upstream::WorkStruct};

#[repr(C)]
pub struct RcuWork {
    pub work: WorkStruct,
    pub rcu: RcuHead,
    pub wq: *mut c_void,
}

const _: [(); 56] = [(); core::mem::size_of::<RcuWork>()];
const _: [(); 8] = [(); core::mem::align_of::<RcuWork>()];
