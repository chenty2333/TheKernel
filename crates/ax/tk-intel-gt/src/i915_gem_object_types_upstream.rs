// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
//! Linux v7.2.3 type/constants transcription of
//! `drivers/gpu/drm/i915/gem/i915_gem_object_types.h`.
//!
//! The source header carries SPDX MIT. i915-owned records are defined here;
//! framework records are imported from their existing owner bindings. The
//! target `.config` has CONFIG_PROC_FS=y, CONFIG_MMU_NOTIFIER=y, and
//! CONFIG_DRM_I915_SELFTEST=n. Its userptr arm embeds `mmu_interval_notifier`
//! by value, so the full object owner depends on the canonical LinuxKPI
//! owner `linux::mmu_notifier::MmuIntervalNotifier`. This file intentionally
//! uses that owner path rather than an opaque by-value placeholder; keep this module feature-gated until caller integration is complete.

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::ManuallyDrop,
};

use crate::{
    i915_gem_context_types_upstream::{I915DrmClient, I915GemContext},
    i915_gem_shmem_upstream::{DrmI915GemPread, DrmI915GemPwrite},
    i915_vma_resource_types_upstream::{I915PageSizes, I915RefctSgt},
    intel_context_types_upstream::I915Active,
    intel_context_upstream::{
        DrmMmNode, DrmVmaOffsetNode, I915AddressSpace, RadixTreeRoot, RcuHead, SgTable,
    },
    intel_engine_cs_upstream::{AtomicT, ListHead, LlistNode, Mutex, RbNode, RbRoot, Spinlock},
    intel_gt_defines_types_upstream::I915_MAX_GT,
    linux::{
        gem::{DrmGemObject, TtmBufferObjectLayout},
        gem_memory::IntelMemoryRegion,
        mmu_notifier::MmuIntervalNotifier,
    },
    linux_i915_private::DrmI915Private,
};

// Forward-declared or pointer-only Linux/i915 dependencies from lines 21-23
// and their uses below. These intentionally have no fabricated by-value data.
#[repr(C)]
pub struct I915Frontbuffer {
    _opaque: [u8; 0],
}
pub use crate::linux::mm::VmOperationsStruct;
#[repr(C)]
pub struct TtmResource {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct Page {
    _opaque: [u8; 0],
}

// `struct i915_lut_handle` at i915_gem_object_types.h:31-35.
#[repr(C)]
pub struct I915LutHandle {
    pub obj_link: ListHead,
    pub ctx: *mut I915GemContext,
    pub handle: u32,
}

// Constants nested in `struct drm_i915_gem_object_ops` in the C header. BIT(n)
// values used by its unsigned-int flag arguments remain 32-bit here.
pub const I915_GEM_OBJECT_IS_SHRINKABLE: u32 = 1 << 1;
pub const I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST: u32 = 1 << 2;
pub const I915_GEM_OBJECT_IS_PROXY: u32 = 1 << 3;
pub const I915_GEM_OBJECT_NO_MMAP: u32 = 1 << 4;
pub const I915_GEM_OBJECT_SHRINK_WRITEBACK: u32 = 1 << 0;
pub const I915_GEM_OBJECT_SHRINK_NO_GPU_WAIT: u32 = 1 << 1;

/// `struct drm_i915_gem_object_ops` (`i915_gem_object_types.h:37-118`).
#[repr(C)]
pub struct DrmI915GemObjectOps {
    pub flags: u32,
    pub get_pages: Option<unsafe extern "C" fn(*mut DrmI915GemObject) -> i32>,
    pub put_pages: Option<unsafe extern "C" fn(*mut DrmI915GemObject, *mut SgTable)>,
    pub truncate: Option<unsafe extern "C" fn(*mut DrmI915GemObject) -> i32>,
    pub shrink: Option<unsafe extern "C" fn(*mut DrmI915GemObject, u32) -> i32>,
    pub pread: Option<unsafe extern "C" fn(*mut DrmI915GemObject, *const DrmI915GemPread) -> i32>,
    pub pwrite: Option<unsafe extern "C" fn(*mut DrmI915GemObject, *const DrmI915GemPwrite) -> i32>,
    pub mmap_offset: Option<unsafe extern "C" fn(*mut DrmI915GemObject) -> u64>,
    pub unmap_virtual: Option<unsafe extern "C" fn(*mut DrmI915GemObject)>,
    pub dmabuf_export: Option<unsafe extern "C" fn(*mut DrmI915GemObject) -> i32>,
    pub adjust_lru: Option<unsafe extern "C" fn(*mut DrmI915GemObject)>,
    pub delayed_free: Option<unsafe extern "C" fn(*mut DrmI915GemObject)>,
    pub migrate:
        Option<unsafe extern "C" fn(*mut DrmI915GemObject, *mut IntelMemoryRegion, u32) -> i32>,
    pub release: Option<unsafe extern "C" fn(*mut DrmI915GemObject)>,
    pub mmap_ops: *const VmOperationsStruct,
    pub name: *const c_char,
}

/// `enum i915_cache_level` (`i915_gem_object_types.h:131-205`).
#[repr(i32)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I915CacheLevel {
    I915_CACHE_NONE      = 0,
    I915_CACHE_LLC       = 1,
    I915_CACHE_L3_LLC    = 2,
    I915_CACHE_WT        = 3,
    I915_MAX_CACHE_LEVEL = 4,
}
pub const I915_MAX_CACHE_LEVEL: u32 = 4;

/// `enum i915_map_type` has an unsigned 32-bit representation on the target
/// compiler because its force-map enumerators use bit 31.
pub type I915MapType = u32;
pub const I915_MAP_WB: I915MapType = 0;
pub const I915_MAP_WC: I915MapType = 1;
pub const I915_MAP_OVERRIDE: I915MapType = 1 << 31;
pub const I915_MAP_FORCE_WB: I915MapType = I915_MAP_WB | I915_MAP_OVERRIDE;
pub const I915_MAP_FORCE_WC: I915MapType = I915_MAP_WC | I915_MAP_OVERRIDE;

/// `enum i915_mmap_type` (`i915_gem_object_types.h:215-221`).
#[repr(i32)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I915MmapType {
    I915_MMAP_TYPE_GTT   = 0,
    I915_MMAP_TYPE_WC    = 1,
    I915_MMAP_TYPE_WB    = 2,
    I915_MMAP_TYPE_UC    = 3,
    I915_MMAP_TYPE_FIXED = 4,
}

/// `struct i915_mmap_offset` (`i915_gem_object_types.h:223-229`).
#[repr(C)]
pub struct I915MmapOffset {
    pub vma_node: DrmVmaOffsetNode,
    pub obj: *mut DrmI915GemObject,
    pub mmap_type: I915MmapType,
    pub offset: RbNode,
}

/// `struct i915_gem_object_page_iter` (`i915_gem_object_types.h:231-237`).
#[repr(C)]
pub struct I915GemObjectPageIter {
    pub sg_pos: *mut SgEntry,
    pub sg_idx: u32,
    pub radix: RadixTreeRoot,
    pub lock: Mutex,
}

#[repr(C)]
pub struct SgEntry {
    _opaque: [u8; 0],
}

// `drm_i915_gem_object` flag macros at i915_gem_object_types.h:335-370.
pub const I915_BO_ALLOC_CONTIGUOUS: c_ulong = 1 << 0;
pub const I915_BO_ALLOC_VOLATILE: c_ulong = 1 << 1;
pub const I915_BO_ALLOC_CPU_CLEAR: c_ulong = 1 << 2;
pub const I915_BO_ALLOC_USER: c_ulong = 1 << 3;
pub const I915_BO_ALLOC_PM_VOLATILE: c_ulong = 1 << 4;
pub const I915_BO_ALLOC_PM_EARLY: c_ulong = 1 << 5;
pub const I915_BO_ALLOC_GPU_ONLY: c_ulong = 1 << 6;
pub const I915_BO_ALLOC_CCS_AUX: c_ulong = 1 << 7;
pub const I915_BO_ALLOC_NOTHP: c_ulong = 1 << 8;
pub const I915_BO_PREALLOC: c_ulong = 1 << 9;
pub const I915_BO_ALLOC_FLAGS: c_ulong = I915_BO_ALLOC_CONTIGUOUS
    | I915_BO_ALLOC_VOLATILE
    | I915_BO_ALLOC_CPU_CLEAR
    | I915_BO_ALLOC_USER
    | I915_BO_ALLOC_PM_VOLATILE
    | I915_BO_ALLOC_PM_EARLY
    | I915_BO_ALLOC_GPU_ONLY
    | I915_BO_ALLOC_CCS_AUX
    | I915_BO_ALLOC_NOTHP
    | I915_BO_PREALLOC;
pub const I915_BO_READONLY: c_ulong = 1 << 10;
pub const I915_TILING_QUIRK_BIT: i32 = 11;
pub const I915_BO_PROTECTED: c_ulong = 1 << 12;

// Mutable placement flags at lines 378-380.
pub const I915_BO_FLAG_STRUCT_PAGE: u32 = 1 << 0;
pub const I915_BO_FLAG_IOMEM: u32 = 1 << 1;

// Cache-coherency bitfields at lines 485-487.
pub const I915_BO_CACHE_COHERENT_FOR_READ: u32 = 1 << 0;
pub const I915_BO_CACHE_COHERENT_FOR_WRITE: u32 = 1 << 1;

// Tiling stride macros at lines 583-585.
pub const FENCE_MINIMUM_STRIDE: i32 = 128;
pub const TILING_MASK: i32 = FENCE_MINIMUM_STRIDE - 1;
pub const STRIDE_MASK: i32 = !TILING_MASK;

/// Source anonymous VMA-list subrecord (`i915_gem_object_types.h:253-279`).
#[repr(C)]
pub struct I915GemObjectVma {
    pub lock: Spinlock,
    pub list: ListHead,
    pub tree: RbRoot,
}

/// Source anonymous mmap-offset subrecord (`i915_gem_object_types.h:328-331`).
#[repr(C)]
pub struct I915GemObjectMmo {
    pub lock: Spinlock,
    pub offsets: RbRoot,
}

/// C anonymous RCU/free-list union (`i915_gem_object_types.h:316-319`).
#[repr(C)]
pub union I915GemObjectRcuOrFreed {
    pub rcu: ManuallyDrop<RcuHead>,
    pub freed: ManuallyDrop<LlistNode>,
}

/// Anonymous `drm_i915_gem_object.mm` subrecord from
/// `i915_gem_object_types.h:587-694`. The page-iterator copies occupy
/// `get_page`, `get_dma_page`, and TTM `get_io_page`.
#[repr(C)]
pub struct I915GemObjectMm {
    pub pages_pin_count: AtomicT,
    pub shrink_pin: AtomicT,
    pub ttm_shrinkable: bool,
    pub unknown_state: bool,
    pub placements: *mut *mut IntelMemoryRegion,
    pub n_placements: i32,
    pub region: *mut IntelMemoryRegion,
    pub res: *mut TtmResource,
    pub region_link: ListHead,
    pub rsgt: *mut I915RefctSgt,
    pub pages: *mut SgTable,
    pub mapping: *mut c_void,
    pub page_sizes: I915PageSizes,
    pub get_page: I915GemObjectPageIter,
    pub get_dma_page: I915GemObjectPageIter,
    pub link: ListHead,
    /// Storage for the adjacent C fields `madv:2` and `dirty:1`.
    /// Bits 0-1 are `madv`; bit 2 is `dirty`.
    pub madv_dirty_bits: u32,
    pub tlb: [u32; I915_MAX_GT],
}

impl I915GemObjectMm {
    #[inline]
    pub fn madv(&self) -> u32 {
        self.madv_dirty_bits & 0x3
    }

    #[inline]
    pub fn set_madv(&mut self, madv: u32) {
        self.madv_dirty_bits = (self.madv_dirty_bits & !0x3) | (madv & 0x3);
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.madv_dirty_bits & (1 << 2) != 0
    }

    #[inline]
    pub fn set_dirty(&mut self, dirty: bool) {
        if dirty {
            self.madv_dirty_bits |= 1 << 2;
        } else {
            self.madv_dirty_bits &= !(1 << 2);
        }
    }
}

/// Anonymous TTM subrecord (`i915_gem_object_types.h:696-701`).
#[repr(C)]
pub struct I915GemObjectTtm {
    pub cached_io_rsgt: *mut I915RefctSgt,
    pub get_io_page: I915GemObjectPageIter,
    pub backup: *mut DrmI915GemObject,
    /// C `bool created:1` storage byte.
    pub created: bool,
}

/// The source `base` union: DRM GEM and TTM share the same embedded storage.
#[repr(C)]
pub union DrmI915GemObjectBase {
    pub base: ManuallyDrop<DrmGemObject>,
    pub __do_not_access: ManuallyDrop<TtmBufferObjectLayout>,
}

/// `struct mmu_interval_notifier` arm of the anonymous userptr union.
/// This is by value in C and must use the canonical LinuxKPI owner.
#[repr(C)]
pub struct I915GemObjectUserptr {
    pub ptr: usize,
    pub notifier_seq: c_ulong,
    pub notifier: MmuIntervalNotifier,
    pub pvec: *mut *mut Page,
    pub page_ref: i32,
}

/// Anonymous tail union in `drm_i915_gem_object` (`i915_gem_object_types.h:713-733`).
#[repr(C)]
pub union DrmI915GemObjectBacking {
    pub userptr: ManuallyDrop<I915GemObjectUserptr>,
    pub stolen: *mut DrmMmNode,
    /// `resource_size_t` on the x86_64 target.
    pub bo_offset: u64,
    pub scratch: c_ulong,
    pub encode: u64,
    pub gvt_info: *mut c_void,
}

/// `struct drm_i915_gem_object` (`i915_gem_object_types.h:239-734`).
#[repr(C)]
pub struct DrmI915GemObject {
    /// C union of `struct drm_gem_object` and `struct ttm_buffer_object`.
    pub base: DrmI915GemObjectBase,
    pub ops: *const DrmI915GemObjectOps,
    pub vma: I915GemObjectVma,
    pub lut_list: ListHead,
    pub lut_lock: Spinlock,
    pub obj_link: ListHead,
    pub shares_resv_from: *mut I915AddressSpace,
    pub client: *mut I915DrmClient,
    pub client_link: ListHead,
    pub rcu_or_freed: I915GemObjectRcuOrFreed,
    pub userfault_count: u32,
    pub userfault_link: ListHead,
    pub mmo: I915GemObjectMmo,
    pub flags: c_ulong,
    pub mem_flags: u32,
    /// Storage for adjacent C `unsigned int` cache bitfields.
    /// Bit 0..5 `pat_index`, bit 6 `pat_set_by_user`, bit 7..8
    /// `cache_coherent`, bit 9 `cache_dirty`, bit 10 `is_dpt`.
    pub cache_state_bits: u32,
    pub read_domains: u16,
    pub write_domain: u16,
    pub frontbuffer: *mut I915Frontbuffer,
    pub tiling_and_stride: u32,
    pub mm: I915GemObjectMm,
    pub ttm: I915GemObjectTtm,
    pub pxp_key_instance: u32,
    pub bit_17: *mut c_ulong,
    pub backing: DrmI915GemObjectBacking,
}

// `intel_bo_to_drm_bo()` and `intel_bo_to_i915()` macros at lines 736-737.
/// Pointer conversion used by `intel_bo_to_drm_bo()`.
///
/// # Safety
/// `bo` must point to a live `DrmI915GemObject`.
pub unsafe fn intel_bo_to_drm_bo(bo: *mut DrmI915GemObject) -> *mut DrmGemObject {
    unsafe { core::ptr::addr_of_mut!((*bo).base).cast::<DrmGemObject>() }
}

/// Pointer conversion used by `intel_bo_to_i915()`.
///
/// # Safety
/// `bo` must point to a live object with a valid DRM device pointer.
pub unsafe fn intel_bo_to_i915(bo: *mut DrmI915GemObject) -> *mut DrmI915Private {
    let gem = unsafe { intel_bo_to_drm_bo(bo) };
    crate::linux::i915::to_i915(unsafe { (*gem).dev })
}

#[macro_export]
macro_rules! intel_bo_to_drm_bo {
    ($bo:expr) => {{
        // SAFETY: the caller must provide a live i915 GEM object.
        unsafe { $crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo($bo) }
    }};
}

#[macro_export]
macro_rules! intel_bo_to_i915 {
    ($bo:expr) => {{
        // SAFETY: the caller must provide a live i915 GEM object.
        unsafe { $crate::i915_gem_object_types_upstream::intel_bo_to_i915($bo) }
    }};
}

/// `to_intel_bo()` (`i915_gem_object_types.h:739-745`).
///
/// # Safety
/// `gem` must be null or point at the base member of a live
/// `DrmI915GemObject`.
pub unsafe fn to_intel_bo(gem: *mut DrmGemObject) -> *mut DrmI915GemObject {
    gem.cast::<DrmI915GemObject>()
}

// The source BUILD_BUG_ON in to_intel_bo() requires the base union at offset 0.
const _: [(); 0] = [(); core::mem::offset_of!(DrmI915GemObject, base)];

// Layout assertions for the current x86_64 target configuration. The GEM
// object size is constrained by its 480-byte DRM/TTM base union and 112-byte
// userptr tail union; CONFIG_PROC_FS=y and CONFIG_MMU_NOTIFIER=y, while
// CONFIG_DRM_I915_SELFTEST=n.
const _: [(); 4] = [(); core::mem::size_of::<I915CacheLevel>()];
const _: [(); 4] = [(); core::mem::size_of::<I915MmapType>()];
const _: [(); 32] = [(); core::mem::size_of::<I915LutHandle>()];
const _: [(); 8] = [(); core::mem::align_of::<I915LutHandle>()];
const _: [(); 128] = [(); core::mem::size_of::<DrmI915GemObjectOps>()];
const _: [(); 8] = [(); core::mem::offset_of!(DrmI915GemObjectOps, get_pages)];
const _: [(); 112] = [(); core::mem::offset_of!(DrmI915GemObjectOps, mmap_ops)];
const _: [(); 120] = [(); core::mem::offset_of!(DrmI915GemObjectOps, name)];
const _: [(); 232] = [(); core::mem::size_of::<I915MmapOffset>()];
const _: [(); 192] = [(); core::mem::offset_of!(I915MmapOffset, obj)];
const _: [(); 200] = [(); core::mem::offset_of!(I915MmapOffset, mmap_type)];
const _: [(); 208] = [(); core::mem::offset_of!(I915MmapOffset, offset)];
const _: [(); 56] = [(); core::mem::size_of::<I915GemObjectPageIter>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915GemObjectPageIter, sg_pos)];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemObjectPageIter, sg_idx)];
const _: [(); 16] = [(); core::mem::offset_of!(I915GemObjectPageIter, radix)];
const _: [(); 32] = [(); core::mem::offset_of!(I915GemObjectPageIter, lock)];
const _: [(); 32] = [(); core::mem::size_of::<I915GemObjectVma>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemObjectVma, list)];
const _: [(); 24] = [(); core::mem::offset_of!(I915GemObjectVma, tree)];
const _: [(); 16] = [(); core::mem::size_of::<I915GemObjectMmo>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemObjectMmo, offsets)];
const _: [(); 240] = [(); core::mem::size_of::<I915GemObjectMm>()];
const _: [(); 16] = [(); core::mem::offset_of!(I915GemObjectMm, placements)];
const _: [(); 88] = [(); core::mem::offset_of!(I915GemObjectMm, page_sizes)];
const _: [(); 96] = [(); core::mem::offset_of!(I915GemObjectMm, get_page)];
const _: [(); 152] = [(); core::mem::offset_of!(I915GemObjectMm, get_dma_page)];
const _: [(); 208] = [(); core::mem::offset_of!(I915GemObjectMm, link)];
const _: [(); 224] = [(); core::mem::offset_of!(I915GemObjectMm, madv_dirty_bits)];
const _: [(); 228] = [(); core::mem::offset_of!(I915GemObjectMm, tlb)];
const _: [(); 80] = [(); core::mem::size_of::<I915GemObjectTtm>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915GemObjectTtm, get_io_page)];
const _: [(); 64] = [(); core::mem::offset_of!(I915GemObjectTtm, backup)];
const _: [(); 72] = [(); core::mem::offset_of!(I915GemObjectTtm, created)];
const _: [(); 480] = [(); core::mem::size_of::<DrmI915GemObjectBase>()];
const _: [(); 120] = [(); core::mem::size_of::<I915GemObjectUserptr>()];
const _: [(); 120] = [(); core::mem::size_of::<DrmI915GemObjectBacking>()];
const _: [(); 1144] = [(); core::mem::size_of::<DrmI915GemObject>()];
const _: [(); 8] = [(); core::mem::align_of::<DrmI915GemObject>()];
const _: [(); 0] = [(); core::mem::offset_of!(DrmI915GemObject, base)];
const _: [(); 480] = [(); core::mem::offset_of!(DrmI915GemObject, ops)];
const _: [(); 488] = [(); core::mem::offset_of!(DrmI915GemObject, vma)];
const _: [(); 520] = [(); core::mem::offset_of!(DrmI915GemObject, lut_list)];
const _: [(); 544] = [(); core::mem::offset_of!(DrmI915GemObject, obj_link)];
const _: [(); 592] = [(); core::mem::offset_of!(DrmI915GemObject, rcu_or_freed)];
const _: [(); 616] = [(); core::mem::offset_of!(DrmI915GemObject, userfault_link)];
const _: [(); 568] = [(); core::mem::offset_of!(DrmI915GemObject, client)];
const _: [(); 576] = [(); core::mem::offset_of!(DrmI915GemObject, client_link)];
const _: [(); 608] = [(); core::mem::offset_of!(DrmI915GemObject, userfault_count)];
const _: [(); 632] = [(); core::mem::offset_of!(DrmI915GemObject, mmo)];
const _: [(); 648] = [(); core::mem::offset_of!(DrmI915GemObject, flags)];
const _: [(); 656] = [(); core::mem::offset_of!(DrmI915GemObject, mem_flags)];
const _: [(); 660] = [(); core::mem::offset_of!(DrmI915GemObject, cache_state_bits)];
const _: [(); 664] = [(); core::mem::offset_of!(DrmI915GemObject, read_domains)];
const _: [(); 666] = [(); core::mem::offset_of!(DrmI915GemObject, write_domain)];
const _: [(); 672] = [(); core::mem::offset_of!(DrmI915GemObject, frontbuffer)];
const _: [(); 680] = [(); core::mem::offset_of!(DrmI915GemObject, tiling_and_stride)];
const _: [(); 688] = [(); core::mem::offset_of!(DrmI915GemObject, mm)];
const _: [(); 928] = [(); core::mem::offset_of!(DrmI915GemObject, ttm)];
const _: [(); 1008] = [(); core::mem::offset_of!(DrmI915GemObject, pxp_key_instance)];
const _: [(); 1016] = [(); core::mem::offset_of!(DrmI915GemObject, bit_17)];
const _: [(); 1024] = [(); core::mem::offset_of!(DrmI915GemObject, backing)];
