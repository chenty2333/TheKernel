// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Source-layout Linux MM callback records used by the i915 mmap translation.

use core::ffi::{c_char, c_int, c_ulong, c_void};

use crate::{
    intel_context_types_upstream::File,
    linux::gem_memory::PgProt,
};

/// Linux 7.2.3 `vm_area_struct` fields consumed by the i915 translations.
/// Offsets are from the wt-dev x86_64 oracle build (including
/// CONFIG_PER_VMA_LOCK, CONFIG_SWAP, and CONFIG_NUMA). Unused source fields
/// remain opaque bytes; this record does not model their behavior.
#[repr(C, align(64))]
pub struct VmAreaStruct {
    pub vm_start: c_ulong,
    pub vm_end: c_ulong,
    pub vm_mm: *mut c_void,
    pub vm_page_prot: PgProt,
    pub vm_flags: c_ulong,
    pub vm_lock_seq: u32,
    _vm_lock_seq_pad: u32,
    _anon_vma_chain: [u8; 16],
    _anon_vma: *mut c_void,
    pub vm_ops: *const VmOperationsStruct,
    pub vm_pgoff: c_ulong,
    pub vm_file: *mut File,
    pub vm_private_data: *mut c_void,
    _swap_readahead_info: c_ulong,
    _vm_policy: *mut c_void,
    _rest: [u8; 72],
}

#[repr(C)]
pub struct VmFault {
    pub vma: *mut VmAreaStruct,
    pub gfp_mask: u32,
    _gfp_pad: u32,
    pub pgoff: c_ulong,
    pub address: c_ulong,
    pub real_address: c_ulong,
    pub flags: u32,
    _flags_pad: u32,
    _pmd: *mut c_void,
    _pud: *mut c_void,
    _orig_pte_or_pmd: u64,
    _cow_page: *mut c_void,
    _page: *mut c_void,
    _pte: *mut c_void,
    _ptl: *mut c_void,
    _prealloc_pte: *mut c_void,
}

// `vm_fault_t` result bits from include/linux/mm_types.h.
pub const VM_FAULT_OOM: u32 = 0x000001;
pub const VM_FAULT_SIGBUS: u32 = 0x000002;
pub const VM_FAULT_NOPAGE: u32 = 0x000100;

// Linux `vm_flags` values from include/linux/mm.h for this CONFIG_MMU build.
pub const VM_WRITE: c_ulong = 1 << 1;
pub const VM_PFNMAP: c_ulong = 1 << 10;
pub const VM_IO: c_ulong = 1 << 14;
pub const VM_DONTEXPAND: c_ulong = 1 << 18;
pub const VM_DONTDUMP: c_ulong = 1 << 26;
pub const VM_MIXEDMAP: c_ulong = 1 << 28;
pub const VM_MAYWRITE: c_ulong = 1 << 5;

// Linux `mm.h` declares this architecture-owned helper when CONFIG_MMU=y;
// the configured x86 kernel exports the implementation from pgprot.c.
unsafe extern "C" {
    pub fn vm_get_page_prot(vm_flags: c_ulong) -> PgProt;
    pub fn vma_set_file(vma: *mut VmAreaStruct, file: *mut File);
}

/// Linux `vma_pages()`: the page-count of the page-aligned VMA interval.
#[inline]
pub unsafe fn vma_pages(vma: *const VmAreaStruct) -> c_ulong {
    unsafe { ((*vma).vm_end - (*vma).vm_start) >> crate::linux_config::PAGE_SHIFT }
}

// Linux 7.2.3 include/linux/mmap_lock.h uses this helper when
// CONFIG_PER_VMA_LOCK=y (the configured wt-dev target).
unsafe extern "C" {
    fn __vma_start_write(vma: *mut VmAreaStruct, state: i32) -> i32;
}

#[inline]
pub unsafe fn vm_flags_set(vma: *mut VmAreaStruct, flags: c_ulong) {
    unsafe {
        __vma_start_write(vma, crate::linux::wait::TASK_UNINTERRUPTIBLE as i32);
        (*vma).vm_flags |= flags;
    }
}

#[inline]
pub unsafe fn vm_flags_clear(vma: *mut VmAreaStruct, flags: c_ulong) {
    unsafe {
        __vma_start_write(vma, crate::linux::wait::TASK_UNINTERRUPTIBLE as i32);
        (*vma).vm_flags &= !flags;
    }
}

const _: [(); 192] = [(); core::mem::size_of::<VmAreaStruct>()];
const _: [(); 64] = [(); core::mem::align_of::<VmAreaStruct>()];
const _: [(); 40] = [(); core::mem::offset_of!(VmAreaStruct, vm_lock_seq)];
const _: [(); 80] = [(); core::mem::offset_of!(VmAreaStruct, vm_pgoff)];
const _: [(); 88] = [(); core::mem::offset_of!(VmAreaStruct, vm_file)];
const _: [(); 96] = [(); core::mem::offset_of!(VmAreaStruct, vm_private_data)];
const _: [(); 112] = [(); core::mem::offset_of!(VmAreaStruct, _vm_policy)];
const _: [(); 112] = [(); core::mem::size_of::<VmFault>()];
const _: [(); 0] = [(); core::mem::offset_of!(VmFault, vma)];
const _: [(); 24] = [(); core::mem::offset_of!(VmFault, address)];

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

/// TheKernel has no swap daemon; no native task has Linux's PF_KSWAPD role.
#[inline]
pub fn current_is_kswapd() -> bool {
    false
}
