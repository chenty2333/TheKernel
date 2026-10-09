// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux SRCU embedded-record ABI for the configured kernel.
//!
//! Linux v7.2.3 `include/linux/srcutree.h` defines `srcu_struct` when
//! CONFIG_TREE_SRCU=y. With CONFIG_LOCKDEP=n, `lockdep_map` is an empty C
//! record. Runtime SRCU operations remain owned by the kernel RCU subsystem.

use core::ffi::c_void;

#[repr(C)]
pub struct SrcuStruct {
    pub srcu_ctrp: *mut c_void,
    pub sda: *mut c_void,
    pub srcu_reader_flavor: u8,
    _padding: [u8; 7],
    pub srcu_sup: *mut c_void,
}

const _: [(); 32] = [(); core::mem::size_of::<SrcuStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<SrcuStruct>()];
const _: [(); 0] = [(); core::mem::offset_of!(SrcuStruct, srcu_ctrp)];
const _: [(); 8] = [(); core::mem::offset_of!(SrcuStruct, sda)];
const _: [(); 16] = [(); core::mem::offset_of!(SrcuStruct, srcu_reader_flavor)];
const _: [(); 24] = [(); core::mem::offset_of!(SrcuStruct, srcu_sup)];
