// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Source-derived x86_64 Linux v7.2.3 records from
// drivers/gpu/drm/i915/i915_drv.h, intel_memory_region.h, drm/drm_mm.h,
// linux/io-mapping.h, linux/ioport.h, linux/notifier.h, and linux/shrinker.h.
// Field offsets below were measured with the existing wt-dev Linux 7.2.3 GCC
// compile oracle; keep opaque storage only for unrelated nested records.

#![allow(non_camel_case_types)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{align_of, offset_of, size_of},
};

pub use tk_linux_kapi::gem_memory::*;

use crate::{
    intel_context_upstream::{DrmI915GemObject, DrmMmNode},
    intel_engine_cs_upstream::{
        AtomicT, ListHead, LlistHead, Mutex, RbRoot, RbRootCached, Spinlock, WorkStruct,
    },
    linux_i915_private::DrmI915Private,
};

/// `enum intel_region_id` values from `intel_memory_region.h`.
pub type IntelRegionId = c_int;
pub const INTEL_REGION_SMEM: IntelRegionId = 0;
pub const INTEL_REGION_LMEM_0: IntelRegionId = 1;
pub const INTEL_REGION_LMEM_1: IntelRegionId = 2;
pub const INTEL_REGION_LMEM_2: IntelRegionId = 3;
pub const INTEL_REGION_LMEM_3: IntelRegionId = 4;
pub const INTEL_REGION_STOLEN_SMEM: IntelRegionId = 5;
pub const INTEL_REGION_STOLEN_LMEM: IntelRegionId = 6;
pub const INTEL_REGION_UNKNOWN: IntelRegionId = 7;

/// `enum intel_memory_type` values from `intel_memory_region.h`.
pub const INTEL_MEMORY_SYSTEM: u16 = 0;
pub const INTEL_MEMORY_LOCAL: u16 = 1;
pub const INTEL_MEMORY_STOLEN_SYSTEM: u16 = 2;
pub const INTEL_MEMORY_STOLEN_LOCAL: u16 = 3;
pub const INTEL_MEMORY_MOCK: u16 = 4;

#[inline]
pub const fn intel_memory_type_is_local(memory_type: u16) -> bool {
    memory_type == INTEL_MEMORY_LOCAL || memory_type == INTEL_MEMORY_STOLEN_LOCAL
}








// `cachemode2protval()` is an exported x86 helper. `pgprot_noncached()` is
// the x86 header macro selecting UC-minus (cache mode 2) on supported CPUs.
unsafe extern "C" {
    fn cachemode2protval(cache_mode: i32) -> c_ulong;
}




/// The anonymous `objects` member nested in `struct intel_memory_region`.
#[repr(C)]
pub struct IntelMemoryRegionObjects {
    pub lock: Mutex,
    pub list: ListHead,
}

/// Linux v7.2.3 `struct intel_memory_region` from `intel_memory_region.h`.
#[repr(C)]
pub struct IntelMemoryRegion {
    pub i915: *mut DrmI915Private,
    /// Pointer to source `struct intel_memory_region_ops`.
    pub ops: *const c_void,
    pub iomap: IoMapping,
    pub region: Resource,
    pub io: Resource,
    pub min_page_size: u64,
    pub total: u64,
    pub r#type: u16,
    pub instance: u16,
    /// C `enum intel_region_id` has `int` storage in this ABI.
    pub id: c_int,
    pub name: [c_char; 16],
    pub uabi_name: [c_char; 20],
    pub private: bool,
    pub objects: IntelMemoryRegionObjects,
    pub is_range_manager: bool,
    pub region_private: *mut c_void,
}

/// Linux v7.2.3 `struct i915_gem_mm` from `i915_drv.h`.
#[repr(C)]
pub struct I915GemMm {
    pub stolen_region: *mut IntelMemoryRegion,
    pub stolen: DrmMm,
    pub stolen_lock: Mutex,
    pub obj_lock: Spinlock,
    pub purge_list: ListHead,
    pub shrink_list: ListHead,
    pub free_list: LlistHead,
    pub free_work: WorkStruct,
    pub free_count: AtomicT,
    pub regions: [*mut IntelMemoryRegion; INTEL_REGION_UNKNOWN as usize],
    pub oom_notifier: NotifierBlock,
    pub vmap_notifier: NotifierBlock,
    pub shrinker: *mut Shrinker,
    pub shrink_memory: u64,
    pub shrink_count: u32,
}

// Component sizes/alignments are from the same wt-dev x86_64 compile oracle.
const _: [(); 8] = [(); align_of::<DrmMm>()];
const _: [(); 240] = [(); size_of::<DrmMm>()];
const _: [(); 0] = [(); offset_of!(DrmMm, color_adjust)];
const _: [(); 8] = [(); offset_of!(DrmMm, hole_stack)];
const _: [(); 24] = [(); offset_of!(DrmMm, head_node)];
const _: [(); 192] = [(); offset_of!(DrmMm, interval_tree)];
const _: [(); 208] = [(); offset_of!(DrmMm, holes_size)];
const _: [(); 224] = [(); offset_of!(DrmMm, holes_addr)];
const _: [(); 232] = [(); offset_of!(DrmMm, scan_active)];
const _: [(); 24] = [(); size_of::<NotifierBlock>()];
const _: [(); 0] = [(); offset_of!(NotifierBlock, notifier_call)];
const _: [(); 8] = [(); offset_of!(NotifierBlock, next)];
const _: [(); 16] = [(); offset_of!(NotifierBlock, priority)];
const _: [(); 120] = [(); size_of::<Shrinker>()];
const _: [(); 32] = [(); size_of::<IoMapping>()];
const _: [(); 0] = [(); offset_of!(IoMapping, base)];
const _: [(); 8] = [(); offset_of!(IoMapping, size)];
const _: [(); 16] = [(); offset_of!(IoMapping, prot)];
const _: [(); 24] = [(); offset_of!(IoMapping, iomem)];
const _: [(); 64] = [(); size_of::<Resource>()];
const _: [(); 0] = [(); offset_of!(Resource, start)];
const _: [(); 8] = [(); offset_of!(Resource, end)];
const _: [(); 16] = [(); offset_of!(Resource, name)];
const _: [(); 24] = [(); offset_of!(Resource, flags)];
const _: [(); 32] = [(); offset_of!(Resource, desc)];
const _: [(); 40] = [(); offset_of!(Resource, parent)];
const _: [(); 48] = [(); offset_of!(Resource, sibling)];
const _: [(); 56] = [(); offset_of!(Resource, child)];
const _: [(); 40] = [(); size_of::<IntelMemoryRegionObjects>()];
const _: [(); 0] = [(); offset_of!(IntelMemoryRegionObjects, lock)];
const _: [(); 24] = [(); offset_of!(IntelMemoryRegionObjects, list)];

// i915_gem_mm offsets captured from the wt-dev compile oracle.
const _: [(); 8] = [(); align_of::<I915GemMm>()];
const _: [(); 488] = [(); size_of::<I915GemMm>()];
const _: [(); 0] = [(); offset_of!(I915GemMm, stolen_region)];
const _: [(); 8] = [(); offset_of!(I915GemMm, stolen)];
const _: [(); 248] = [(); offset_of!(I915GemMm, stolen_lock)];
const _: [(); 272] = [(); offset_of!(I915GemMm, obj_lock)];
const _: [(); 280] = [(); offset_of!(I915GemMm, purge_list)];
const _: [(); 296] = [(); offset_of!(I915GemMm, shrink_list)];
const _: [(); 312] = [(); offset_of!(I915GemMm, free_list)];
const _: [(); 320] = [(); offset_of!(I915GemMm, free_work)];
const _: [(); 352] = [(); offset_of!(I915GemMm, free_count)];
const _: [(); 360] = [(); offset_of!(I915GemMm, regions)];
const _: [(); 416] = [(); offset_of!(I915GemMm, oom_notifier)];
const _: [(); 440] = [(); offset_of!(I915GemMm, vmap_notifier)];
const _: [(); 464] = [(); offset_of!(I915GemMm, shrinker)];
const _: [(); 472] = [(); offset_of!(I915GemMm, shrink_memory)];
const _: [(); 480] = [(); offset_of!(I915GemMm, shrink_count)];
const _: [(); 7] = [(); INTEL_REGION_UNKNOWN as usize];
const _: [(); 56] = [(); size_of::<[*mut IntelMemoryRegion; INTEL_REGION_UNKNOWN as usize]>()];

// intel_memory_region offsets captured from the wt-dev compile oracle.
const _: [(); 8] = [(); align_of::<IntelMemoryRegion>()];
const _: [(); 296] = [(); size_of::<IntelMemoryRegion>()];
const _: [(); 0] = [(); offset_of!(IntelMemoryRegion, i915)];
const _: [(); 8] = [(); offset_of!(IntelMemoryRegion, ops)];
const _: [(); 16] = [(); offset_of!(IntelMemoryRegion, iomap)];
const _: [(); 48] = [(); offset_of!(IntelMemoryRegion, region)];
const _: [(); 112] = [(); offset_of!(IntelMemoryRegion, io)];
const _: [(); 176] = [(); offset_of!(IntelMemoryRegion, min_page_size)];
const _: [(); 184] = [(); offset_of!(IntelMemoryRegion, total)];
const _: [(); 192] = [(); offset_of!(IntelMemoryRegion, r#type)];
const _: [(); 194] = [(); offset_of!(IntelMemoryRegion, instance)];
const _: [(); 196] = [(); offset_of!(IntelMemoryRegion, id)];
const _: [(); 200] = [(); offset_of!(IntelMemoryRegion, name)];
const _: [(); 216] = [(); offset_of!(IntelMemoryRegion, uabi_name)];
const _: [(); 236] = [(); offset_of!(IntelMemoryRegion, private)];
const _: [(); 240] = [(); offset_of!(IntelMemoryRegion, objects)];
const _: [(); 280] = [(); offset_of!(IntelMemoryRegion, is_range_manager)];
const _: [(); 288] = [(); offset_of!(IntelMemoryRegion, region_private)];

// Kernel layouts owned by tk-linux-kapi (generic Linux records; see its types.rs).
pub use tk_linux_kapi::types::{DrmMm};
