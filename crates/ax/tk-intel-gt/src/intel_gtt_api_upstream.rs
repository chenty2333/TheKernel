// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Source-order API/layout transcription from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_gtt.h.
//
// This header owns I915AddressSpace and I915Ggtt. VMA/resource/GT, DRM-MM,
// resource, and IO-mapping records use their canonical owner bindings.
// Framework records outside this header remain opaque where their layouts are
// unavailable; no synthetic object state is introduced. `i915_ggtt_pin_bias`
// itself is defined by i915_vma.h, and is represented here through the owned
// `I915Ggtt.pin_bias` field and `i915_vm_to_ggtt()` dependency, not duplicated.

#![allow(non_camel_case_types, non_snake_case, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_context_types_upstream::DrmI915FilePrivate,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915CacheLevel},
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::I915Vma,
    intel_context_upstream::{DrmMmNode, I915GemWwCtx, Kref, SgEntry, SgTable},
    intel_engine_cs_upstream::{AtomicT, ListHead, Mutex, RbRootCached, Spinlock, WorkStruct},
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gt_types_upstream::IntelGt,
    linux::{
        gem_memory::*,
        locks::*,
        memory::{__GFP_RETRY_MAYFAIL, *},
        registers::*,
        workqueue::*,
    },
    linux_config::{
        __GFP_NOWARN, GFP_KERNEL, INTEL_MEMORY_STOLEN_LOCAL, INTEL_MEMORY_SYSTEM, PAGE_SHIFT,
        PAGE_SIZE,
    },
    linux_i915_private::DrmI915Private,
};

pub type gen6_pte_t = u32;
pub type gen8_pte_t = u64;
pub type dma_addr_t = u64;
pub type resource_size_t = u64;
pub type drm_i915_gem_object = DrmI915GemObject;
pub type i915_vma = I915Vma;
pub type i915_vma_resource = I915VmaResource;
pub type i915_fence_reg = I915FenceReg;
pub type intel_gt = IntelGt;

#[inline]
pub const fn bit(bit: u32) -> u32 {
    1u32 << bit
}

#[inline]
pub const fn bit_ull(bit: u32) -> u64 {
    1u64 << bit
}

#[inline]
pub const fn genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) << low
}

#[inline]
pub const fn genmask_ull(high: u32, low: u32) -> u64 {
    (u64::MAX >> (63 - high)) << low
}

#[inline]
pub const fn reg_field_prep(mask: u32, value: u32) -> u32 {
    (value << mask.trailing_zeros()) & mask
}

pub const I915_GFP_ALLOW_FAIL: u32 = GFP_KERNEL | __GFP_RETRY_MAYFAIL | __GFP_NOWARN;

#[macro_export]
macro_rules! GTT_TRACE {
    ($($arg:tt)*) => {{ /* CONFIG_DRM_I915_TRACE_GTT is disabled for this target. */ }};
}

pub const NALLOC: u32 = 3;
pub const I915_GTT_PAGE_SIZE_4K: u64 = bit_ull(12);
pub const I915_GTT_PAGE_SIZE_64K: u64 = bit_ull(16);
pub const I915_GTT_PAGE_SIZE_2M: u64 = bit_ull(21);
pub const I915_GTT_PAGE_SIZE: u64 = I915_GTT_PAGE_SIZE_4K;
pub const I915_GTT_MAX_PAGE_SIZE: u64 = I915_GTT_PAGE_SIZE_2M;
pub const I915_GTT_PAGE_MASK: u64 = !(I915_GTT_PAGE_SIZE - 1);
pub const I915_GTT_MIN_ALIGNMENT: u64 = I915_GTT_PAGE_SIZE;
pub const I915_FENCE_REG_NONE: i32 = -1;
pub const I915_MAX_NUM_FENCES: u32 = 32;
pub const I915_MAX_NUM_FENCE_BITS: u32 = 6;

#[inline]
pub const fn ggtt_total_entries(ggtt: *const I915Ggtt) -> u64 {
    unsafe { (*ggtt).vm.total >> PAGE_SHIFT }
}

#[inline]
pub const fn I915_PTES(pte_len: usize) -> u32 {
    (PAGE_SIZE / pte_len) as u32
}

#[inline]
pub const fn I915_PTE_MASK(pte_len: usize) -> u32 {
    I915_PTES(pte_len) - 1
}

pub const I915_PDES: u32 = 512;
pub const I915_PDE_MASK: u32 = I915_PDES - 1;

#[inline]
pub const fn GEN6_GTT_ADDR_ENCODE(addr: u64) -> u64 {
    addr | ((addr >> 28) & 0xff0)
}
#[inline]
pub const fn GEN6_PTE_ADDR_ENCODE(addr: u64) -> u64 {
    GEN6_GTT_ADDR_ENCODE(addr)
}
#[inline]
pub const fn GEN6_PDE_ADDR_ENCODE(addr: u64) -> u64 {
    GEN6_GTT_ADDR_ENCODE(addr)
}
pub const GEN6_PTE_CACHE_LLC: u32 = 2 << 1;
pub const GEN6_PTE_UNCACHED: u32 = 1 << 1;
pub const GEN6_PTE_VALID: u32 = bit(0);
pub const GEN6_PTES: u32 = I915_PTES(size_of::<gen6_pte_t>());
pub const GEN6_PD_SIZE: usize = I915_PDES as usize * PAGE_SIZE;
pub const GEN6_PD_ALIGN: usize = PAGE_SIZE * 16;
pub const GEN6_PDE_SHIFT: u32 = 22;
pub const GEN6_PDE_VALID: u32 = bit(0);

#[inline]
pub const fn NUM_PTE(pde_shift: u32) -> u32 {
    1u32 << (pde_shift - PAGE_SHIFT)
}

pub const GEN7_PTE_CACHE_L3_LLC: u32 = 3 << 1;
pub const BYT_PTE_SNOOPED_BY_CPU_CACHES: u32 = bit(2);
pub const BYT_PTE_WRITEABLE: u32 = bit(1);
pub const MTL_PPGTT_PTE_PAT3: u64 = bit_ull(62);
pub const GEN12_PPGTT_PTE_LM: u64 = bit_ull(11);
pub const GEN12_PPGTT_PTE_PAT2: u64 = bit_ull(7);
pub const GEN12_PPGTT_PTE_PAT1: u64 = bit_ull(4);
pub const GEN12_PPGTT_PTE_PAT0: u64 = bit_ull(3);
pub const GEN12_GGTT_PTE_LM: u64 = bit_ull(1);
pub const MTL_GGTT_PTE_PAT0: u64 = bit_ull(52);
pub const MTL_GGTT_PTE_PAT1: u64 = bit_ull(53);
pub const GEN12_GGTT_PTE_ADDR_MASK: u64 = genmask_ull(45, 12);
pub const MTL_GGTT_PTE_PAT_MASK: u64 = genmask_ull(53, 52);
pub const GEN12_PDE_64K: u32 = bit(6);
pub const GEN12_PTE_PS64: u32 = bit(8);

#[inline]
pub const fn HSW_CACHEABILITY_CONTROL(bits: u32) -> u32 {
    ((bits & 0x7) << 1) | ((bits & 0x8) << 8)
}
pub const HSW_WB_LLC_AGE3: u32 = HSW_CACHEABILITY_CONTROL(0x2);
pub const HSW_WB_LLC_AGE0: u32 = HSW_CACHEABILITY_CONTROL(0x3);
pub const HSW_WB_ELLC_LLC_AGE3: u32 = HSW_CACHEABILITY_CONTROL(0x8);
pub const HSW_WB_ELLC_LLC_AGE0: u32 = HSW_CACHEABILITY_CONTROL(0xb);
pub const HSW_WT_ELLC_LLC_AGE3: u32 = HSW_CACHEABILITY_CONTROL(0x7);
pub const HSW_WT_ELLC_LLC_AGE0: u32 = HSW_CACHEABILITY_CONTROL(0x6);
pub const HSW_PTE_UNCACHED: u32 = 0;
#[inline]
pub const fn HSW_GTT_ADDR_ENCODE(addr: u64) -> u64 {
    addr | ((addr >> 28) & 0x7f0)
}
#[inline]
pub const fn HSW_PTE_ADDR_ENCODE(addr: u64) -> u64 {
    HSW_GTT_ADDR_ENCODE(addr)
}

pub const GEN8_3LVL_PDPES: u32 = 4;
// Values from arch/x86/include/asm/pgtable_types.h for the selected x86_64 ABI.
const X86_PAGE_PWT: u64 = 1 << 3;
const X86_PAGE_PCD: u64 = 1 << 4;
const X86_PAGE_PAT: u64 = 1 << 7;
pub const PPAT_UNCACHED: u64 = X86_PAGE_PWT | X86_PAGE_PCD;
pub const PPAT_CACHED_PDE: u64 = 0;
pub const PPAT_CACHED: u64 = X86_PAGE_PAT;
pub const PPAT_DISPLAY_ELLC: u64 = X86_PAGE_PCD;
pub const CHV_PPAT_SNOOP: u32 = bit(6);
#[inline]
pub const fn GEN8_PPAT_AGE(x: u32) -> u32 {
    x << 4
}
pub const GEN8_PPAT_LLCeLLC: u32 = 3 << 2;
pub const GEN8_PPAT_LLCELLC: u32 = 2 << 2;
pub const GEN8_PPAT_LLC: u32 = 1 << 2;
pub const GEN8_PPAT_WB: u32 = 3;
pub const GEN8_PPAT_WT: u32 = 2;
pub const GEN8_PPAT_WC: u32 = 1;
pub const GEN8_PPAT_UC: u32 = 0;
pub const GEN8_PPAT_ELLC_OVERRIDE: u32 = 0;
#[inline]
pub const fn GEN8_PPAT(index: u32, value: u64) -> u64 {
    value << (index * 8)
}
pub const GEN8_PAGE_PRESENT: u64 = bit_ull(0);
pub const GEN8_PAGE_RW: u64 = bit_ull(1);
pub const GEN8_PDE_IPS_64K: u32 = bit(11);
pub const GEN8_PDE_PS_2M: u32 = bit(7);
pub const MTL_PPAT_L4_CACHE_POLICY_MASK: u32 = genmask(3, 2);
pub const MTL_PAT_INDEX_COH_MODE_MASK: u32 = genmask(1, 0);
pub const MTL_PPAT_L4_3_UC: u32 = reg_field_prep(MTL_PPAT_L4_CACHE_POLICY_MASK, 3);
pub const MTL_PPAT_L4_1_WT: u32 = reg_field_prep(MTL_PPAT_L4_CACHE_POLICY_MASK, 1);
pub const MTL_PPAT_L4_0_WB: u32 = reg_field_prep(MTL_PPAT_L4_CACHE_POLICY_MASK, 0);
pub const MTL_3_COH_2W: u32 = reg_field_prep(MTL_PAT_INDEX_COH_MODE_MASK, 3);
pub const MTL_2_COH_1W: u32 = reg_field_prep(MTL_PAT_INDEX_COH_MODE_MASK, 2);

#[repr(C)]
#[derive(Clone, Copy)]
pub union I915PageTableUsedOrStash {
    pub used: AtomicT,
    pub stash: *mut I915PageTable,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915PageTable {
    pub base: *mut DrmI915GemObject,
    pub used_or_stash: I915PageTableUsedOrStash,
    pub is_compact: bool,
}

#[repr(C)]
pub struct I915PageDirectory {
    pub pt: I915PageTable,
    pub lock: Spinlock,
    pub entry: *mut *mut c_void,
}

#[repr(C)]
pub struct I915VmPtStash {
    pub pt: [*mut I915PageTable; 2],
    pub pt_sz: c_int,
}

/// `struct i915_vma_ops`; function pointer signatures follow this header.
#[repr(C)]
pub struct I915VmaOpsLayout {
    pub bind_vma: Option<
        unsafe extern "C" fn(
            *mut I915AddressSpace,
            *mut I915VmPtStash,
            *mut I915VmaResource,
            u32,
            u32,
        ),
    >,
    pub unbind_vma: Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915VmaResource)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915AddressSpaceReserved {
    pub obj: *mut DrmI915GemObject,
    pub vma: *mut I915Vma,
}

pub const VM_CLASS_GGTT: u32 = 0;
pub const VM_CLASS_PPGTT: u32 = 1;
pub const VM_CLASS_DPT: u32 = 2;
pub const PTE_READ_ONLY: u32 = bit(0);
pub const PTE_LM: u32 = bit(1);

#[repr(C, align(8))]
pub struct DmaResvStorage {
    _opaque: [u8; 40],
}

/// Complete source-owned `struct i915_address_space` from intel_gtt.h.
/// `dma_resv` is a framework record kept as its known Linux x86_64 ABI storage;
/// DRM-MM, workqueue, GT, VMA resource, and all i915-owned types use owner
/// bindings. Selftest-only fields are absent because CONFIG_DRM_I915_SELFTEST=n.
#[repr(C)]
pub struct I915AddressSpace {
    pub r#ref: Kref,
    pub release_work: WorkStruct,
    pub mm: DrmMm,
    pub rsvd: I915AddressSpaceReserved,
    pub gt: *mut IntelGt,
    pub i915: *mut DrmI915Private,
    pub fpriv: *mut DrmI915FilePrivate,
    pub dma: *mut c_void,
    pub total: u64,
    pub reserved: u64,
    pub min_alignment: [u64; (INTEL_MEMORY_STOLEN_LOCAL as usize) + 1],
    pub bind_async_flags: u32,
    _mutex_pad: [u8; 4],
    pub mutex: Mutex,
    pub resv_ref: Kref,
    pub _resv: DmaResvStorage,
    pub scratch: [*mut DrmI915GemObject; 4],
    pub bound_list: ListHead,
    pub unbound_list: ListHead,
    /// Source adjacent bitfields: is_ggtt, is_dpt, has_read_only,
    /// skip_pte_rewrite (bits 0 through 3).
    pub vm_flags: u8,
    pub top: u8,
    pub pd_shift: u8,
    pub scratch_order: u8,
    pub lmem_pt_obj_flags: c_ulong,
    pub pending_unbind: RbRootCached,
    pub alloc_pt_dma:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, c_int) -> *mut DrmI915GemObject>,
    pub alloc_scratch_dma:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, c_int) -> *mut DrmI915GemObject>,
    pub pte_encode: Option<unsafe extern "C" fn(dma_addr_t, u32, u32) -> u64>,
    pub pte_decode: Option<unsafe extern "C" fn(u64, *mut bool, *mut bool) -> dma_addr_t>,
    pub allocate_va_range:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915VmPtStash, u64, u64)>,
    pub clear_range: Option<unsafe extern "C" fn(*mut I915AddressSpace, u64, u64)>,
    pub scratch_range: Option<unsafe extern "C" fn(*mut I915AddressSpace, u64, u64)>,
    pub insert_page: Option<unsafe extern "C" fn(*mut I915AddressSpace, dma_addr_t, u64, u32, u32)>,
    pub insert_entries:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915VmaResource, u32, u32)>,
    pub raw_insert_page:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, dma_addr_t, u64, u32, u32)>,
    pub raw_insert_entries:
        Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915VmaResource, u32, u32)>,
    pub read_entry: Option<
        unsafe extern "C" fn(*mut I915AddressSpace, u64, *mut bool, *mut bool) -> dma_addr_t,
    >,
    pub cleanup: Option<unsafe extern "C" fn(*mut I915AddressSpace)>,
    pub foreach: Option<
        unsafe extern "C" fn(
            *mut I915AddressSpace,
            u64,
            u64,
            Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915PageTable, *mut c_void)>,
            *mut c_void,
        ),
    >,
    pub vma_ops: I915VmaOpsLayout,
}

pub type i915_address_space = I915AddressSpace;

#[repr(C)]
pub struct I915Ggtt {
    pub vm: I915AddressSpace,
    pub iomap: IoMapping,
    pub gmadr: Resource,
    pub mappable_end: resource_size_t,
    pub gsm: *mut c_void,
    pub invalidate: Option<unsafe extern "C" fn(*mut I915Ggtt)>,
    pub alias: *mut I915Ppgtt,
    pub do_idle_maps: bool,
    pub mtrr: c_int,
    pub bit_6_swizzle_x: u32,
    pub bit_6_swizzle_y: u32,
    pub pin_bias: u32,
    pub num_fences: u32,
    pub fence_regs: *mut I915FenceReg,
    pub fence_list: ListHead,
    pub userfault_list: ListHead,
    pub error_mutex: Mutex,
    pub error_capture: DrmMmNode,
    pub uc_fw: DrmMmNode,
    pub gt_list: ListHead,
}

#[repr(C)]
pub struct I915Ppgtt {
    pub vm: I915AddressSpace,
    pub pd: *mut I915PageDirectory,
}

pub type i915_page_table = I915PageTable;
pub type i915_page_directory = I915PageDirectory;
pub type i915_vm_pt_stash = I915VmPtStash;
pub type i915_vma_ops = I915VmaOpsLayout;
pub type i915_ggtt = I915Ggtt;
pub type i915_ppgtt = I915Ppgtt;

#[inline]
pub fn i915_is_ggtt(vm: *const I915AddressSpace) -> bool {
    unsafe { (*vm).vm_flags & 1 != 0 }
}
#[inline]
pub fn i915_is_dpt(vm: *const I915AddressSpace) -> bool {
    unsafe { (*vm).vm_flags & 2 != 0 }
}
#[inline]
pub fn i915_is_ggtt_or_dpt(vm: *const I915AddressSpace) -> bool {
    i915_is_ggtt(vm) || i915_is_dpt(vm)
}

const _: [(); 40] = [(); offset_of!(I915AddressSpace, mm)];
const _: [(); 280] = [(); offset_of!(I915AddressSpace, rsvd)];
const _: [(); 488] = [(); offset_of!(I915AddressSpace, bound_list)];
const _: [(); 680] = [(); size_of::<I915AddressSpace>()];
const _: [(); 0] = [(); offset_of!(I915Ggtt, vm)];
const _: [(); 24] = [(); size_of::<I915PageTable>()];
const _: [(); 40] = [(); size_of::<I915PageDirectory>()];
const _: [(); 24] = [(); size_of::<I915VmPtStash>()];
const _: [(); 16] = [(); size_of::<I915VmaOpsLayout>()];

#[repr(C)]
pub struct SgtDma {
    pub sg: *mut SgEntry,
    pub dma: dma_addr_t,
    pub max: dma_addr_t,
}
pub type sgt_dma = SgtDma;

#[repr(C)]
struct SgTableHeaderView {
    sgl: *mut SgEntry,
    nents: u32,
    orig_nents: u32,
}

/// x86_64 Linux `scatterlist` fields needed by `sg_dma_address/sg_dma_len`.
/// `CONFIG_NEED_SG_DMA_LENGTH` is selected by arch/x86, so dma_length is the
/// source field used for max; the full page-link behavior is retained below.
#[repr(C)]
struct ScatterListDmaView {
    page_link: c_ulong,
    offset: u32,
    length: u32,
    dma_address: u64,
    dma_length: u32,
    _padding: u32,
}
const _: [(); 16] = [(); offset_of!(ScatterListDmaView, dma_address)];
const _: [(); 24] = [(); offset_of!(ScatterListDmaView, dma_length)];
const _: [(); 32] = [(); size_of::<ScatterListDmaView>()];
const SG_CHAIN: c_ulong = 1;
const SG_END: c_ulong = 2;
const SG_PAGE_LINK_MASK: c_ulong = SG_CHAIN | SG_END;

#[derive(Clone, Copy)]
pub struct SgDaddrIter {
    pub sgp: *mut SgEntry,
    pub dma: u64,
    pub curr: u64,
    pub max: u64,
    step: u64,
}

unsafe fn sgt_iter(sg: *mut SgEntry) -> SgDaddrIter {
    if sg.is_null() {
        return SgDaddrIter {
            sgp: ptr::null_mut(),
            dma: 0,
            curr: 0,
            max: 0,
            step: I915_GTT_PAGE_SIZE,
        };
    }
    let entry = unsafe { &*sg.cast::<ScatterListDmaView>() };
    let len = entry.dma_length as u64;
    SgDaddrIter {
        sgp: sg,
        dma: entry.dma_address,
        curr: 0,
        max: len,
        step: I915_GTT_PAGE_SIZE,
    }
}

unsafe fn sg_next(sg: *mut SgEntry) -> *mut SgEntry {
    if sg.is_null() {
        return ptr::null_mut();
    }
    let page_link = unsafe { (*sg.cast::<ScatterListDmaView>()).page_link };
    if page_link & SG_END != 0 {
        return ptr::null_mut();
    }
    let next = unsafe { sg.add(1) };
    let next_flags = unsafe { (*next.cast::<ScatterListDmaView>()).page_link } & SG_PAGE_LINK_MASK;
    if next_flags & SG_CHAIN != 0 {
        (unsafe { (*next.cast::<ScatterListDmaView>()).page_link } & !SG_PAGE_LINK_MASK)
            as *mut SgEntry
    } else {
        next
    }
}

impl Iterator for SgDaddrIter {
    type Item = dma_addr_t;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.sgp.is_null() {
                return None;
            }
            if self.curr < self.max {
                let address = self.dma.wrapping_add(self.curr);
                self.curr = self.curr.wrapping_add(self.step);
                return Some(address);
            }
            self.sgp = unsafe { sg_next(self.sgp) };
            if self.sgp.is_null() {
                return None;
            }
            let current = unsafe { sgt_iter(self.sgp) };
            self.dma = current.dma;
            self.curr = current.curr;
            self.max = current.max;
        }
    }
}

/// Rust iterator forms of the source `for_each_sgt_daddr` wrappers. The x86
/// scatterlist/sg_table views are limited to the documented Linux ABI fields.
pub unsafe fn for_each_sgt_daddr(sgt: *mut SgTable) -> SgDaddrIter {
    let table = unsafe { &*sgt.cast::<SgTableHeaderView>() };
    unsafe { sgt_iter(table.sgl) }
}

pub fn for_each_sgt_daddr_next(iter: &mut SgDaddrIter) -> Option<dma_addr_t> {
    iter.next()
}

// Out-of-line routines declared in intel_gtt.h or the included i915 headers.
unsafe extern "C" {
    pub fn __px_page(object: *mut DrmI915GemObject) -> *mut c_void;
    pub fn __px_dma(object: *mut DrmI915GemObject) -> dma_addr_t;
    pub fn __px_vaddr(object: *mut DrmI915GemObject) -> *mut c_void;
    pub fn intel_vm_no_concurrent_access_wa(i915: *mut DrmI915Private) -> bool;
    pub fn i915_vm_lock_objects(vm: *mut I915AddressSpace, ww: *mut I915GemWwCtx) -> c_int;
    pub fn i915_vm_release(kref: *mut Kref);
    pub fn i915_vm_resv_release(kref: *mut Kref);
    pub fn i915_address_space_init(vm: *mut I915AddressSpace, subclass: c_int);
    pub fn i915_address_space_fini(vm: *mut I915AddressSpace);
    pub fn ppgtt_init(ppgtt: *mut I915Ppgtt, gt: *mut IntelGt, lmem_pt_obj_flags: c_ulong);
    pub fn intel_ggtt_bind_vma(
        vm: *mut I915AddressSpace,
        stash: *mut I915VmPtStash,
        vma_res: *mut I915VmaResource,
        pat_index: u32,
        flags: u32,
    );
    pub fn intel_ggtt_unbind_vma(vm: *mut I915AddressSpace, vma_res: *mut I915VmaResource);
    pub fn intel_ggtt_read_entry(
        vm: *mut I915AddressSpace,
        offset: u64,
        is_present: *mut bool,
        is_local: *mut bool,
    ) -> dma_addr_t;
    pub fn i915_ggtt_probe_hw(i915: *mut DrmI915Private) -> c_int;
    pub fn i915_ggtt_init_hw(i915: *mut DrmI915Private) -> c_int;
    pub fn i915_ggtt_enable_hw(i915: *mut DrmI915Private) -> c_int;
    pub fn i915_init_ggtt(i915: *mut DrmI915Private) -> c_int;
    pub fn i915_ggtt_driver_release(i915: *mut DrmI915Private);
    pub fn i915_ggtt_driver_late_release(i915: *mut DrmI915Private);
    pub fn i915_ggtt_create(i915: *mut DrmI915Private) -> *mut I915Ggtt;
    pub fn i915_ppgtt_init_hw(gt: *mut IntelGt) -> c_int;
    pub fn i915_ppgtt_create(gt: *mut IntelGt, lmem_pt_obj_flags: c_ulong) -> *mut I915Ppgtt;
    pub fn i915_ggtt_suspend_vm(vm: *mut I915AddressSpace, evict_all: bool);
    pub fn i915_ggtt_resume_vm(vm: *mut I915AddressSpace, all_evicted: bool) -> bool;
    pub fn i915_ggtt_suspend(gtt: *mut I915Ggtt);
    pub fn i915_ggtt_resume(ggtt: *mut I915Ggtt);
    pub fn fill_page_dma(object: *mut DrmI915GemObject, value: u64, count: u32);
    pub fn setup_scratch_page(vm: *mut I915AddressSpace) -> c_int;
    pub fn free_scratch(vm: *mut I915AddressSpace);
    pub fn alloc_pt_dma(vm: *mut I915AddressSpace, size: c_int) -> *mut DrmI915GemObject;
    pub fn alloc_pt_lmem(vm: *mut I915AddressSpace, size: c_int) -> *mut DrmI915GemObject;
    pub fn alloc_pt(vm: *mut I915AddressSpace, size: c_int) -> *mut I915PageTable;
    pub fn alloc_pd(vm: *mut I915AddressSpace) -> *mut I915PageDirectory;
    pub fn __alloc_pd(npde: c_int) -> *mut I915PageDirectory;
    pub fn map_pt_dma(vm: *mut I915AddressSpace, object: *mut DrmI915GemObject) -> c_int;
    pub fn map_pt_dma_locked(vm: *mut I915AddressSpace, object: *mut DrmI915GemObject) -> c_int;
    pub fn free_px(vm: *mut I915AddressSpace, pt: *mut I915PageTable, lvl: c_int);
    pub fn __set_pd_entry(
        pd: *mut I915PageDirectory,
        index: u16,
        pt: *mut I915PageTable,
        encode: unsafe extern "C" fn(dma_addr_t, I915CacheLevel) -> u64,
    );
    pub fn clear_pd_entry(pd: *mut I915PageDirectory, index: u16, scratch: *const DrmI915GemObject);
    pub fn release_pd_entry(
        pd: *mut I915PageDirectory,
        index: u16,
        pt: *mut I915PageTable,
        scratch: *const DrmI915GemObject,
    ) -> bool;
    pub fn gen6_ggtt_invalidate(ggtt: *mut I915Ggtt);
    pub fn ppgtt_bind_vma(
        vm: *mut I915AddressSpace,
        stash: *mut I915VmPtStash,
        vma_res: *mut I915VmaResource,
        pat_index: u32,
        flags: u32,
    );
    pub fn ppgtt_unbind_vma(vm: *mut I915AddressSpace, vma_res: *mut I915VmaResource);
    pub fn gtt_write_workarounds(gt: *mut IntelGt);
    pub fn setup_private_pat(gt: *mut IntelGt);
    pub fn i915_vm_alloc_pt_stash(
        vm: *mut I915AddressSpace,
        stash: *mut I915VmPtStash,
        size: u64,
    ) -> c_int;
    pub fn i915_vm_map_pt_stash(vm: *mut I915AddressSpace, stash: *mut I915VmPtStash) -> c_int;
    pub fn i915_vm_free_pt_stash(vm: *mut I915AddressSpace, stash: *mut I915VmPtStash);
    pub fn __vm_create_scratch_for_read(vm: *mut I915AddressSpace, size: c_ulong) -> *mut I915Vma;
    pub fn __vm_create_scratch_for_read_pinned(
        vm: *mut I915AddressSpace,
        size: c_ulong,
    ) -> *mut I915Vma;
    pub fn i915_ggtt_require_binder(i915: *mut DrmI915Private) -> bool;
}

// The source macro `__px_choose_expr` performs compile-time C type dispatch.
// Rust implements the same type selection through the PxBasePointer and
// PxPageTablePointer traits rather than reproducing a GNU C expression macro.
pub trait PxBasePointer {
    unsafe fn base(self) -> *mut DrmI915GemObject;
}
impl PxBasePointer for *mut DrmI915GemObject {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        self
    }
}
impl PxBasePointer for *const DrmI915GemObject {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        self.cast_mut()
    }
}
impl PxBasePointer for *mut I915PageTable {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        unsafe { (*self).base }
    }
}
impl PxBasePointer for *const I915PageTable {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        unsafe { (*self).base }
    }
}
impl PxBasePointer for *mut I915PageDirectory {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        unsafe { (*self).pt.base }
    }
}
impl PxBasePointer for *const I915PageDirectory {
    unsafe fn base(self) -> *mut DrmI915GemObject {
        unsafe { (*self).pt.base }
    }
}

pub unsafe fn px_base<T: PxBasePointer>(px: T) -> *mut DrmI915GemObject {
    unsafe { px.base() }
}

pub trait PxPageTablePointer {
    unsafe fn page_table(self) -> *mut I915PageTable;
}
impl PxPageTablePointer for *mut I915PageTable {
    unsafe fn page_table(self) -> *mut I915PageTable {
        self
    }
}
impl PxPageTablePointer for *const I915PageTable {
    unsafe fn page_table(self) -> *mut I915PageTable {
        self.cast_mut()
    }
}
impl PxPageTablePointer for *mut I915PageDirectory {
    unsafe fn page_table(self) -> *mut I915PageTable {
        unsafe { ptr::addr_of_mut!((*self).pt) }
    }
}
impl PxPageTablePointer for *const I915PageDirectory {
    unsafe fn page_table(self) -> *mut I915PageTable {
        unsafe { ptr::addr_of!((*self).pt).cast_mut() }
    }
}

pub unsafe fn px_pt<T: PxPageTablePointer>(px: T) -> *mut I915PageTable {
    unsafe { px.page_table() }
}

pub unsafe fn px_used<T: PxPageTablePointer>(px: T) -> *mut I915PageTableUsedOrStash {
    unsafe { ptr::addr_of_mut!((*px_pt(px)).used_or_stash) }
}

pub unsafe fn px_dma<T: PxBasePointer>(px: T) -> dma_addr_t {
    unsafe { __px_dma(px_base(px)) }
}

pub unsafe fn px_vaddr<T: PxBasePointer>(px: T) -> *mut c_void {
    unsafe { __px_vaddr(px_base(px)) }
}

pub unsafe fn fill_px<T: PxBasePointer>(px: T, value: u64) {
    unsafe { fill_page_dma(px_base(px), value, (PAGE_SIZE / size_of::<u64>()) as u32) };
}

pub unsafe fn fill32_px<T: PxBasePointer>(px: T, value: u64) {
    let low = value as u32 as u64;
    unsafe { fill_px(px, (low << 32) | low) };
}

pub unsafe fn free_pt(vm: *mut I915AddressSpace, pt: *mut I915PageTable) {
    unsafe { free_px(vm, pt, 0) };
}

pub unsafe fn free_pd<T: PxPageTablePointer>(vm: *mut I915AddressSpace, pd: T) {
    unsafe { free_px(vm, px_pt(pd), 1) };
}

/// `set_pd_entry` in C captures the file-local `gen8_pde_encode()` function
/// from gen8_ppgtt.c. The owner is not exported to this header/module, so the
/// Rust adapter takes that exact encoder explicitly rather than inventing or
/// falsely importing a static C symbol.
pub unsafe fn set_pd_entry_with_encoder<T: PxPageTablePointer>(
    pd: *mut I915PageDirectory,
    index: u16,
    to: T,
    encode: unsafe extern "C" fn(dma_addr_t, I915CacheLevel) -> u64,
) {
    unsafe { __set_pd_entry(pd, index, px_pt(to), encode) };
}

// upstream: intel_gtt.h i915_vm_is_4lvl()
pub unsafe fn i915_vm_is_4lvl(vm: *const I915AddressSpace) -> bool {
    unsafe { ((*vm).total.wrapping_sub(1) >> 32) != 0 }
}

// upstream: intel_gtt.h i915_vm_has_scratch_64K()
pub unsafe fn i915_vm_has_scratch_64K(vm: *const I915AddressSpace) -> bool {
    unsafe { (*vm).scratch_order == get_order(I915_GTT_PAGE_SIZE_64K as usize) }
}

#[inline]
const fn get_order(size: usize) -> u8 {
    if size <= PAGE_SIZE {
        0
    } else {
        (usize::BITS - (size - 1).leading_zeros()) as u8 - PAGE_SHIFT as u8
    }
}

// upstream: intel_gtt.h i915_vm_min_alignment()
pub unsafe fn i915_vm_min_alignment(vm: *const I915AddressSpace, mut memory_type: i32) -> u64 {
    if memory_type >= (unsafe { (*vm).min_alignment.len() } as i32) {
        memory_type = INTEL_MEMORY_SYSTEM;
    }
    unsafe { (*vm).min_alignment[memory_type as usize] }
}

// upstream: intel_gtt.h i915_vm_obj_min_alignment()
pub unsafe fn i915_vm_obj_min_alignment(
    vm: *const I915AddressSpace,
    object: *const DrmI915GemObject,
) -> u64 {
    let region = unsafe { READ_ONCE!((*object).mm.region) };
    let memory_type = if region.is_null() {
        INTEL_MEMORY_SYSTEM
    } else {
        unsafe { (*region).r#type as i32 }
    };
    unsafe { i915_vm_min_alignment(vm, memory_type) }
}

// upstream: intel_gtt.h i915_vm_has_cache_coloring()
pub unsafe fn i915_vm_has_cache_coloring(vm: *const I915AddressSpace) -> bool {
    i915_is_ggtt(vm) && unsafe { (*vm).mm.color_adjust.is_some() }
}

// upstream: intel_gtt.h i915_vm_to_ggtt()
pub unsafe fn i915_vm_to_ggtt(vm: *mut I915AddressSpace) -> *mut I915Ggtt {
    GEM_BUG_ON!(!i915_is_ggtt(vm));
    vm.cast::<I915Ggtt>()
}

// upstream: intel_gtt.h i915_vm_to_ppgtt()
pub unsafe fn i915_vm_to_ppgtt(vm: *mut I915AddressSpace) -> *mut I915Ppgtt {
    GEM_BUG_ON!(i915_is_ggtt_or_dpt(vm));
    vm.cast::<I915Ppgtt>()
}

// upstream: intel_gtt.h i915_vm_get()
pub unsafe fn i915_vm_get(vm: *mut I915AddressSpace) -> *mut I915AddressSpace {
    unsafe { kref_get(ptr::addr_of_mut!((*vm).r#ref)) };
    vm
}

// upstream: intel_gtt.h i915_vm_tryget()
pub unsafe fn i915_vm_tryget(vm: *mut I915AddressSpace) -> *mut I915AddressSpace {
    if unsafe { kref_get_unless_zero(&mut (*vm).r#ref) } {
        vm
    } else {
        ptr::null_mut()
    }
}

// upstream: intel_gtt.h assert_vm_alive()
pub unsafe fn assert_vm_alive(vm: *const I915AddressSpace) {
    GEM_BUG_ON!(kref_read(unsafe { &(*vm).r#ref }) == 0);
}

// upstream: intel_gtt.h i915_vm_resv_get()
pub unsafe fn i915_vm_resv_get(vm: *mut I915AddressSpace) -> *mut c_void {
    unsafe { kref_get(ptr::addr_of_mut!((*vm).resv_ref)) };
    unsafe { ptr::addr_of_mut!((*vm)._resv).cast::<c_void>() }
}

// upstream: intel_gtt.h i915_vm_put()
pub unsafe fn i915_vm_put(vm: *mut I915AddressSpace) {
    unsafe { kref_put(ptr::addr_of_mut!((*vm).r#ref), i915_vm_release) };
}

// upstream: intel_gtt.h i915_vm_resv_put()
pub unsafe fn i915_vm_resv_put(vm: *mut I915AddressSpace) {
    unsafe { kref_put(ptr::addr_of_mut!((*vm).resv_ref), i915_vm_resv_release) };
}

// upstream: intel_gtt.h i915_pte_index()
pub const fn i915_pte_index(address: u64, pde_shift: u32) -> u32 {
    ((address >> PAGE_SHIFT) as u32) & (NUM_PTE(pde_shift) - 1)
}

// upstream: intel_gtt.h i915_pte_count()
pub unsafe fn i915_pte_count(addr: u64, length: u64, pde_shift: u32) -> u32 {
    GEM_BUG_ON!(length == 0);
    GEM_BUG_ON!(((addr | length) & (PAGE_SIZE as u64 - 1)) != 0);
    let mask = !((1u64 << pde_shift) - 1);
    let end = addr.wrapping_add(length);
    if (addr & mask) != (end & mask) {
        NUM_PTE(pde_shift) - i915_pte_index(addr, pde_shift)
    } else {
        i915_pte_index(end, pde_shift) - i915_pte_index(addr, pde_shift)
    }
}

// upstream: intel_gtt.h i915_pde_index()
pub const fn i915_pde_index(addr: u64, shift: u32) -> u32 {
    ((addr >> shift) as u32) & I915_PDE_MASK
}

// upstream: intel_gtt.h i915_pt_entry()
pub unsafe fn i915_pt_entry(pd: *const I915PageDirectory, index: u16) -> *mut I915PageTable {
    unsafe { *(*pd).entry.add(index as usize).cast::<*mut I915PageTable>() }
}

// upstream: intel_gtt.h i915_pd_entry()
pub unsafe fn i915_pd_entry(pdp: *const I915PageDirectory, index: u16) -> *mut I915PageDirectory {
    unsafe {
        *(*pdp)
            .entry
            .add(index as usize)
            .cast::<*mut I915PageDirectory>()
    }
}

// upstream: intel_gtt.h i915_page_dir_dma_addr()
pub unsafe fn i915_page_dir_dma_addr(ppgtt: *const I915Ppgtt, index: u32) -> dma_addr_t {
    let pt = unsafe {
        *(*(*ppgtt).pd)
            .entry
            .add(index as usize)
            .cast::<*mut I915PageTable>()
    };
    let object = if pt.is_null() {
        unsafe { (*ppgtt).vm.scratch[(*ppgtt).vm.top as usize] }
    } else {
        unsafe { (*pt).base }
    };
    unsafe { __px_dma(object) }
}

// upstream: intel_gtt.h i915_ggtt_has_aperture()
pub unsafe fn i915_ggtt_has_aperture(ggtt: *const I915Ggtt) -> bool {
    unsafe { (*ggtt).mappable_end > 0 }
}

// upstream: intel_gtt.h sgt_dma()
pub unsafe fn sgt_dma(vma_res: *mut I915VmaResource) -> SgtDma {
    let table = unsafe { (*vma_res).bi.pages.cast::<SgTableHeaderView>() };
    let sg = unsafe { (*table).sgl };
    let entry = unsafe { &*sg.cast::<ScatterListDmaView>() };
    let addr = entry.dma_address;
    SgtDma {
        sg,
        dma: addr,
        max: addr.wrapping_add(entry.dma_length as u64),
    }
}

// Layout and source declaration aliases.
pub type i915_cache_level = I915CacheLevel;
pub type i915_vm_class = u32;
