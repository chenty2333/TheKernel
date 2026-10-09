// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Source-layout Linux MM callback records used by the i915 mmap translation.

use core::ffi::{c_char, c_int, c_ulong, c_void};

#[repr(C)]
pub struct VmAreaStruct {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct VmFault {
    _opaque: [u8; 0],
}

/// Linux 7.2.3 `vm_operations_struct` callback table for the wt-dev target.
/// The target's `auto.conf` has CONFIG_NUMA=y and CONFIG_USERFAULTFD=y, while
/// CONFIG_FIND_NORMAL_PAGE and CONFIG_NUMA_BALANCING are unset. Thus the
/// table has the core callbacks, NUMA policy callbacks, and the userfaultfd
/// tail pointer in this order.
#[repr(C)]
pub struct VmOperationsStruct {
    pub open: Option<unsafe extern "C" fn(*mut VmAreaStruct)>,
    pub close: Option<unsafe extern "C" fn(*mut VmAreaStruct)>,
    pub mapped: Option<
        unsafe extern "C" fn(usize, usize, c_ulong, *const c_void, *mut *mut c_void) -> c_int,
    >,
    pub may_split: Option<unsafe extern "C" fn(*mut VmAreaStruct, c_ulong) -> c_int>,
    pub mremap: Option<unsafe extern "C" fn(*mut VmAreaStruct) -> c_int>,
    pub mprotect:
        Option<unsafe extern "C" fn(*mut VmAreaStruct, c_ulong, c_ulong, c_ulong) -> c_int>,
    pub fault: Option<unsafe extern "C" fn(*mut VmFault) -> u32>,
    pub huge_fault: Option<unsafe extern "C" fn(*mut VmFault, u32) -> u32>,
    pub map_pages: Option<unsafe extern "C" fn(*mut VmFault, c_ulong, c_ulong) -> u32>,
    pub pagesize: Option<unsafe extern "C" fn(*mut VmAreaStruct) -> c_ulong>,
    pub page_mkwrite: Option<unsafe extern "C" fn(*mut VmFault) -> u32>,
    pub pfn_mkwrite: Option<unsafe extern "C" fn(*mut VmFault) -> u32>,
    pub access:
        Option<unsafe extern "C" fn(*mut VmAreaStruct, c_ulong, *mut c_void, c_int, c_int) -> c_int>,
    pub name: Option<unsafe extern "C" fn(*mut VmAreaStruct) -> *const c_char>,
    pub set_policy: Option<unsafe extern "C" fn(*mut VmAreaStruct, *mut c_void) -> c_int>,
    pub get_policy:
        Option<unsafe extern "C" fn(*mut VmAreaStruct, c_ulong, *mut c_ulong) -> *mut c_void>,
    pub uffd_ops: *const c_void,
}

// The callback table is published as immutable static kernel metadata; its
// pointer-valued fields are never mutated after initialization.
unsafe impl Sync for VmOperationsStruct {}

impl VmOperationsStruct {
    pub const EMPTY: Self = Self {
        open: None,
        close: None,
        mapped: None,
        may_split: None,
        mremap: None,
        mprotect: None,
        fault: None,
        huge_fault: None,
        map_pages: None,
        pagesize: None,
        page_mkwrite: None,
        pfn_mkwrite: None,
        access: None,
        name: None,
        set_policy: None,
        get_policy: None,
        uffd_ops: core::ptr::null(),
    };
}

const _: [(); 17 * core::mem::size_of::<usize>()] =
    [(); core::mem::size_of::<VmOperationsStruct>()];
