// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/gen8_ppgtt.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unexpected_cfgs
)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_gem_lmem_upstream::{i915_gem_object_create_lmem, i915_gem_object_is_lmem},
    i915_gem_object_api_upstream::{
        __i915_gem_object_pin_pages, i915_gem_object_get, i915_gem_object_put,
    },
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_BO_ALLOC_GPU_ONLY, I915_BO_ALLOC_VOLATILE, I915CacheLevel,
    },
    i915_gem_pages_upstream::{Scatterlist, sg_next},
    i915_vma_api_upstream::{i915_vma_instance, i915_vma_make_unshrinkable, i915_vma_pin},
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::{I915_VMA_LOCAL_BIND, I915Vma},
    intel_context_upstream::SgEntry,
    intel_gt_api_upstream::intel_gt_needs_wa_16018031267,
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{
        GEN8_PAGE_PRESENT, GEN8_PAGE_RW, GEN8_PDE_PS_2M, GEN12_PDE_64K, GEN12_PPGTT_PTE_LM,
        GEN12_PTE_PS64, I915_GTT_PAGE_SIZE, I915_GTT_PAGE_SIZE_2M, I915_GTT_PAGE_SIZE_4K,
        I915_GTT_PAGE_SIZE_64K, I915_PDES, I915AddressSpace, I915PageDirectory, I915PageTable,
        I915Ppgtt, I915VmPtStash, MTL_PPGTT_PTE_PAT3, NALLOC, PPAT_CACHED, PPAT_CACHED_PDE,
        PPAT_DISPLAY_ELLC, PPAT_UNCACHED, PTE_LM, PTE_READ_ONLY, SgtDma, alloc_pd, clear_pd_entry,
        fill_page_dma, fill_px, free_pd, free_px, gen8_pte_t, i915_page_dir_dma_addr,
        i915_pd_entry, i915_pt_entry, i915_vm_has_scratch_64K, i915_vm_is_4lvl, i915_vm_put,
        px_base, px_dma, px_used, px_vaddr, release_pd_entry, set_pd_entry_with_encoder, sgt_dma,
    },
    intel_ppgtt_upstream::__alloc_pd,
    intel_ring_upstream::i915_gem_object_create_internal,
    linux::{
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_LMEM, IP_VER, IS_GRAPHICS_VER,
            i915_gem_get_pat_index,
        },
        locks::{spin_lock, spin_unlock},
        memory::{
            atomic_add, atomic_dec, atomic_fetch_inc, atomic_inc, atomic_read, atomic_set,
            atomic_sub, kzalloc_obj,
        },
        primitives::{ilog2, wmb},
        registers::PIN_USER,
    },
    linux_config::{ENOMEM, PAGE_SIZE, PIN_HIGH},
    linux_i915_private::DrmI915Private,
};

const GEN8_PAGE_SIZE: u64 = 1 << 12;
const GEN8_PTE_SHIFT: u32 = 12;
const GEN8_NUM_PDES: u32 = 512;
const SZ_64K: u64 = 64 * 1024;

#[inline]
const fn gen8_pd_shift(level: u32) -> u32 {
    level * 9 // ilog2(GEN8_PDES)
}

#[inline]
const fn gen8_pd_index(index: u64, level: u32) -> u32 {
    crate::intel_gtt_api_upstream::i915_pde_index(index, gen8_pd_shift(level))
}

#[inline]
const fn gen8_pte_shift(level: u32) -> u32 {
    GEN8_PTE_SHIFT + gen8_pd_shift(level)
}

#[inline]
const fn gen8_pte_index(address: u64, level: u32) -> u32 {
    crate::intel_gtt_api_upstream::i915_pde_index(address, gen8_pte_shift(level))
}

#[inline]
fn range_overflows(start: u64, size: u64, max: u64) -> bool {
    start >= max || size > max - start
}

#[inline]
fn sg_dma_address(sg: *mut SgEntry) -> u64 {
    unsafe { (*sg.cast::<Scatterlist>()).dma_address }
}

#[inline]
fn sg_dma_len(sg: *mut SgEntry) -> u32 {
    unsafe { (*sg.cast::<Scatterlist>()).dma_length }
}

#[inline]
unsafe fn next_sg(sg: *mut SgEntry) -> *mut SgEntry {
    unsafe { sg_next(sg.cast::<Scatterlist>()).cast::<SgEntry>() }
}

#[inline]
unsafe fn has_64k_pages(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { crate::linux::i915::INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[0] & (1 << 4) != 0 }
}

#[inline]
unsafe fn dword_fill64(dst: *mut u64, value: u64, count: usize) {
    for index in 0..count {
        unsafe { ptr::write(dst.add(index), value) };
    }
}

// upstream: gen8_ppgtt.c gen8_pde_encode()
unsafe extern "C" fn gen8_pde_encode(addr: u64, level: I915CacheLevel) -> u64 {
    let mut pde = addr | GEN8_PAGE_PRESENT | GEN8_PAGE_RW;
    if level != I915CacheLevel::I915_CACHE_NONE {
        pde |= PPAT_CACHED_PDE;
    } else {
        pde |= PPAT_UNCACHED;
    }
    pde
}

// upstream: gen8_ppgtt.c gen8_pte_encode()
unsafe extern "C" fn gen8_pte_encode(addr: u64, pat_index: u32, flags: u32) -> u64 {
    let mut pte = addr | GEN8_PAGE_PRESENT | GEN8_PAGE_RW;
    if flags & PTE_READ_ONLY != 0 {
        pte &= !GEN8_PAGE_RW;
    }
    match pat_index {
        value if value == I915CacheLevel::I915_CACHE_NONE as u32 => pte |= PPAT_UNCACHED,
        value if value == I915CacheLevel::I915_CACHE_WT as u32 => pte |= PPAT_DISPLAY_ELLC,
        _ => pte |= PPAT_CACHED,
    }
    pte
}

// upstream: gen8_ppgtt.c gen12_pte_encode()
unsafe extern "C" fn gen12_pte_encode(addr: u64, pat_index: u32, flags: u32) -> u64 {
    let mut pte = addr | GEN8_PAGE_PRESENT | GEN8_PAGE_RW;
    if flags & PTE_READ_ONLY != 0 {
        pte &= !GEN8_PAGE_RW;
    }
    if flags & PTE_LM != 0 {
        pte |= GEN12_PPGTT_PTE_LM;
    }
    if pat_index & 1 != 0 {
        pte |= crate::intel_gtt_api_upstream::GEN12_PPGTT_PTE_PAT0;
    }
    if pat_index & 2 != 0 {
        pte |= crate::intel_gtt_api_upstream::GEN12_PPGTT_PTE_PAT1;
    }
    if pat_index & 4 != 0 {
        pte |= crate::intel_gtt_api_upstream::GEN12_PPGTT_PTE_PAT2;
    }
    if pat_index & 8 != 0 {
        pte |= MTL_PPGTT_PTE_PAT3;
    }
    pte
}

// upstream: gen8_ppgtt.c gen8_ppgtt_notify_vgt()
// This function and its call sites are excluded from the selected
// CONFIG_DRM_I915_GVT=n build; the canonical DrmI915Private owner intentionally
// does not expose the optional i915->vgpu.lock record.
#[cfg(CONFIG_DRM_I915_GVT)]
unsafe fn gen8_ppgtt_notify_vgt(_ppgtt: *mut I915Ppgtt, _create: bool) {
    // The selected Linux configuration has CONFIG_DRM_I915_GVT=n. If enabled,
    // the canonical i915_private owner must expose `i915->vgpu.lock` before
    // this source path can be translated without a duplicate layout.
    compile_error!("CONFIG_DRM_I915_GVT needs the canonical i915_virtual_gpu lock owner");
}

// Index shifts into the pagetable are offset by GEN8_PTE_SHIFT [12].

// upstream: gen8_ppgtt.c gen8_pd_range()
unsafe fn gen8_pd_range(start: u64, mut end: u64, level: u32, index: &mut u32) -> u32 {
    let shift = gen8_pd_shift(level);
    let mask = u64::MAX << gen8_pd_shift(level + 1);
    GEM_BUG_ON!(start >= end);
    end = end.wrapping_add(!mask >> gen8_pd_shift(1));
    *index = gen8_pd_index(start, level);
    if (start ^ end) & mask != 0 {
        GEN8_NUM_PDES - *index
    } else {
        gen8_pd_index(end, level) - *index
    }
}

// upstream: gen8_ppgtt.c gen8_pd_contains()
unsafe fn gen8_pd_contains(start: u64, end: u64, level: u32) -> bool {
    let mask = u64::MAX << gen8_pd_shift(level + 1);
    GEM_BUG_ON!(start >= end);
    ((start ^ end) & mask) != 0 && (start & !mask) == 0
}

// upstream: gen8_ppgtt.c gen8_pt_count()
unsafe fn gen8_pt_count(start: u64, end: u64) -> u32 {
    GEM_BUG_ON!(start >= end);
    if (start ^ end) >> gen8_pd_shift(1) != 0 {
        GEN8_NUM_PDES - (start as u32 & (GEN8_NUM_PDES - 1))
    } else {
        (end - start) as u32
    }
}

// upstream: gen8_ppgtt.c gen8_pd_top_count()
unsafe fn gen8_pd_top_count(vm: *const I915AddressSpace) -> u32 {
    let shift = gen8_pte_shift(unsafe { (*vm).top as u32 });
    (unsafe { (*vm).total.wrapping_add((1u64 << shift) - 1) } >> shift) as u32
}

// upstream: gen8_ppgtt.c gen8_pdp_for_page_index()
unsafe fn gen8_pdp_for_page_index(vm: *mut I915AddressSpace, index: u64) -> *mut I915PageDirectory {
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    if unsafe { (*vm).top == 2 } {
        unsafe { (*ppgtt).pd }
    } else {
        unsafe { i915_pd_entry((*ppgtt).pd, gen8_pd_index(index, (*vm).top as u32) as u16) }
    }
}

// upstream: gen8_ppgtt.c gen8_pdp_for_page_address()
unsafe fn gen8_pdp_for_page_address(
    vm: *mut I915AddressSpace,
    address: u64,
) -> *mut I915PageDirectory {
    unsafe { gen8_pdp_for_page_index(vm, address >> GEN8_PTE_SHIFT) }
}

// upstream: gen8_ppgtt.c __gen8_ppgtt_cleanup()
unsafe fn __gen8_ppgtt_cleanup(
    vm: *mut I915AddressSpace,
    pd: *mut I915PageDirectory,
    mut count: u32,
    level: u32,
) {
    if level != 0 {
        let mut pde = unsafe { (*pd).entry };
        while count != 0 {
            let pt = unsafe { *pde }.cast::<I915PageTable>();
            if !pt.is_null() {
                unsafe {
                    __gen8_ppgtt_cleanup(
                        vm,
                        pt.cast::<I915PageDirectory>(),
                        GEN8_NUM_PDES,
                        level - 1,
                    )
                };
            }
            pde = unsafe { pde.add(1) };
            count -= 1;
        }
    }
    unsafe { free_px(vm, ptr::addr_of_mut!((*pd).pt), level as c_int) };
}

// upstream: gen8_ppgtt.c gen8_ppgtt_cleanup()
unsafe extern "C" fn gen8_ppgtt_cleanup(vm: *mut I915AddressSpace) {
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    let rsvd = unsafe { (*vm).rsvd.obj };
    if !rsvd.is_null() {
        unsafe { i915_gem_object_put(rsvd) };
    }
    #[cfg(CONFIG_DRM_I915_GVT)]
    if unsafe { intel_vgpu_active((*vm).i915) } {
        unsafe { gen8_ppgtt_notify_vgt(ppgtt, false) };
    }
    let pd = unsafe { (*ppgtt).pd };
    if !pd.is_null() {
        let count = unsafe { gen8_pd_top_count(vm) };
        unsafe { __gen8_ppgtt_cleanup(vm, pd, count, (*vm).top as u32) };
    }
    unsafe { crate::intel_gtt_api_upstream::free_scratch(vm) };
}

// upstream: gen8_ppgtt.c __gen8_ppgtt_clear()
unsafe fn __gen8_ppgtt_clear(
    vm: *mut I915AddressSpace,
    pd: *mut I915PageDirectory,
    mut start: u64,
    end: u64,
    level: u32,
) -> u64 {
    let scratch = unsafe { (*vm).scratch[level as usize] };
    GEM_BUG_ON!(end > (unsafe { (*vm).total } >> GEN8_PTE_SHIFT));
    let mut index = 0;
    let mut len = unsafe { gen8_pd_range(start, end, level, &mut index) };
    let level = level - 1;
    crate::GTT_TRACE!(
        "{}({:p}):{{ lvl:{}, start:{:x}, end:{:x}, idx:{}, len:{}, used:{} }}\n",
        "__gen8_ppgtt_clear",
        vm,
        level + 1,
        start,
        end,
        index,
        len,
        unsafe { atomic_read(&(*px_used(pd)).used) },
    );
    GEM_BUG_ON!(len == 0 || len >= unsafe { atomic_read(&(*px_used(pd)).used) as u32 });
    while len != 0 {
        let pt = unsafe { (*pd).entry.add(index as usize).read() }.cast::<I915PageTable>();
        let old_used = unsafe { atomic_fetch_inc(&mut (*px_used(pt)).used) };
        if old_used >> gen8_pd_shift(1) != 0 && unsafe { gen8_pd_contains(start, end, level) } {
            unsafe { clear_pd_entry(pd, index as u16, scratch) };
            unsafe {
                __gen8_ppgtt_cleanup(vm, pt.cast::<I915PageDirectory>(), GEN8_NUM_PDES, level)
            };
            start = start.wrapping_add((GEN8_NUM_PDES as u64) << gen8_pd_shift(level));
            index += 1;
            len -= 1;
            continue;
        }
        if level != 0 {
            start = unsafe {
                __gen8_ppgtt_clear(vm, pt.cast::<I915PageDirectory>(), start, end, level)
            };
        } else {
            let count = unsafe { gen8_pt_count(start, end) };
            let mut pte = gen8_pd_index(start, 0);
            let mut num_ptes = count;
            let mut vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
            crate::GTT_TRACE!(
                "{}({:p}):{{ lvl:{}, start:{:x}, end:{:x}, idx:{}, len:{}, used:{} }} removing \
                 pte\n",
                "__gen8_ppgtt_clear",
                vm,
                level,
                start,
                end,
                gen8_pd_index(start, 0),
                count,
                unsafe { atomic_read(&(*px_used(pt)).used) },
            );
            GEM_BUG_ON!(count == 0 || count >= unsafe { atomic_read(&(*px_used(pt)).used) as u32 });
            if unsafe { (*pt).is_compact } {
                GEM_BUG_ON!(num_ptes % 16 != 0 || pte % 16 != 0);
                num_ptes /= 16;
                pte /= 16;
            }
            let encode = unsafe { (*(*vm).scratch[0]).backing.encode };
            unsafe { dword_fill64(vaddr.add(pte as usize), encode, num_ptes as usize) };
            unsafe { atomic_sub(count as i32, &mut (*px_used(pt)).used) };
            start = start.wrapping_add(count as u64);
        }
        if unsafe { release_pd_entry(pd, index as u16, pt, scratch) } {
            unsafe { free_px(vm, pt, level as c_int) };
        }
        index += 1;
        len -= 1;
    }
    start
}

// upstream: gen8_ppgtt.c gen8_ppgtt_clear()
unsafe extern "C" fn gen8_ppgtt_clear(vm: *mut I915AddressSpace, start: u64, length: u64) {
    GEM_BUG_ON!(!IS_ALIGNED!(start, 1u64 << GEN8_PTE_SHIFT));
    GEM_BUG_ON!(!IS_ALIGNED!(length, 1u64 << GEN8_PTE_SHIFT));
    GEM_BUG_ON!(range_overflows(start, length, unsafe { (*vm).total }));
    let start = start >> GEN8_PTE_SHIFT;
    let length = length >> GEN8_PTE_SHIFT;
    GEM_BUG_ON!(length == 0);
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    unsafe { __gen8_ppgtt_clear(vm, (*ppgtt).pd, start, start + length, (*vm).top as u32) };
}

// upstream: gen8_ppgtt.c __gen8_ppgtt_alloc()
unsafe fn __gen8_ppgtt_alloc(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    pd: *mut I915PageDirectory,
    start: *mut u64,
    end: u64,
    level: u32,
) {
    GEM_BUG_ON!(end > (unsafe { (*vm).total } >> GEN8_PTE_SHIFT));
    let mut index = 0;
    let mut len = unsafe { gen8_pd_range(*start, end, level, &mut index) };
    let level = level - 1;
    crate::GTT_TRACE!(
        "{}({:p}):{{ lvl:{}, start:{:x}, end:{:x}, idx:{}, len:{}, used:{} }}\n",
        "__gen8_ppgtt_alloc",
        vm,
        level + 1,
        unsafe { *start },
        end,
        index,
        len,
        unsafe { atomic_read(&(*px_used(pd)).used) },
    );
    GEM_BUG_ON!(len == 0 || (index + len - 1) >> gen8_pd_shift(1) != 0);
    unsafe { spin_lock(&mut (*pd).lock) };
    GEM_BUG_ON!(unsafe { atomic_read(&(*px_used(pd)).used) } == 0);
    while len != 0 {
        let mut pt = unsafe { i915_pt_entry(pd, index as u16) };
        if pt.is_null() {
            unsafe { spin_unlock(&mut (*pd).lock) };
            crate::GTT_TRACE!(
                "{}({:p}):{{ lvl:{}, idx:{} }} allocating new tree\n",
                "__gen8_ppgtt_alloc",
                vm,
                level + 1,
                index,
            );
            let slot = usize::from(level != 0);
            pt = unsafe { (*stash).pt[slot] };
            unsafe { __i915_gem_object_pin_pages((*pt).base) };
            unsafe { fill_px(pt, (*(*vm).scratch[level as usize]).backing.encode) };
            unsafe { spin_lock(&mut (*pd).lock) };
            if unsafe { (*pd).entry.add(index as usize).read().is_null() } {
                unsafe {
                    (*stash).pt[slot] = (*pt).used_or_stash.stash;
                    atomic_set(&mut (*px_used(pt)).used, 0);
                    set_pd_entry_with_encoder(pd, index as u16, pt, gen8_pde_encode);
                }
            } else {
                pt = unsafe { i915_pt_entry(pd, index as u16) };
            }
        }

        if level != 0 {
            unsafe { atomic_inc(&mut (*px_used(pt)).used) };
            unsafe { spin_unlock(&mut (*pd).lock) };
            unsafe {
                __gen8_ppgtt_alloc(vm, stash, pt.cast::<I915PageDirectory>(), start, end, level)
            };
            unsafe { spin_lock(&mut (*pd).lock) };
            unsafe { atomic_dec(&mut (*px_used(pt)).used) };
            GEM_BUG_ON!(unsafe { atomic_read(&(*px_used(pt)).used) } == 0);
        } else {
            let count = unsafe { gen8_pt_count(*start, end) };
            crate::GTT_TRACE!(
                "{}({:p}):{{ lvl:{}, start:{:x}, end:{:x}, idx:{}, len:{}, used:{} }} inserting \
                 pte\n",
                "__gen8_ppgtt_alloc",
                vm,
                level,
                unsafe { *start },
                end,
                gen8_pd_index(unsafe { *start }, 0),
                count,
                unsafe { atomic_read(&(*px_used(pt)).used) },
            );
            unsafe { atomic_add(count as i32, &mut (*px_used(pt)).used) };
            GEM_BUG_ON!(unsafe { atomic_read(&(*px_used(pt)).used) } > (NALLOC * I915_PDES) as i32);
            unsafe { *start = (*start).wrapping_add(count as u64) };
        }
        index += 1;
        len -= 1;
    }
    unsafe { spin_unlock(&mut (*pd).lock) };
}

// upstream: gen8_ppgtt.c gen8_ppgtt_alloc()
unsafe extern "C" fn gen8_ppgtt_alloc(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    start: u64,
    length: u64,
) {
    GEM_BUG_ON!(!IS_ALIGNED!(start, 1u64 << GEN8_PTE_SHIFT));
    GEM_BUG_ON!(!IS_ALIGNED!(length, 1u64 << GEN8_PTE_SHIFT));
    GEM_BUG_ON!(range_overflows(start, length, unsafe { (*vm).total }));
    let mut start = start >> GEN8_PTE_SHIFT;
    let length = length >> GEN8_PTE_SHIFT;
    GEM_BUG_ON!(length == 0);
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    unsafe {
        __gen8_ppgtt_alloc(
            vm,
            stash,
            (*ppgtt).pd,
            &mut start,
            start + length,
            (*vm).top as u32,
        )
    };
}

// upstream: gen8_ppgtt.c __gen8_ppgtt_foreach()
unsafe fn __gen8_ppgtt_foreach(
    vm: *mut I915AddressSpace,
    pd: *mut I915PageDirectory,
    start: *mut u64,
    end: u64,
    level: u32,
    callback: Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915PageTable, *mut c_void)>,
    data: *mut c_void,
) {
    let mut index = 0;
    let mut len = unsafe { gen8_pd_range(*start, end, level, &mut index) };
    let level = level - 1;
    unsafe { spin_lock(&mut (*pd).lock) };
    while len != 0 {
        let pt = unsafe { i915_pt_entry(pd, index as u16) };
        unsafe { atomic_inc(&mut (*px_used(pt)).used) };
        unsafe { spin_unlock(&mut (*pd).lock) };
        if level != 0 {
            unsafe {
                __gen8_ppgtt_foreach(
                    vm,
                    pt.cast::<I915PageDirectory>(),
                    start,
                    end,
                    level,
                    callback,
                    data,
                )
            };
        } else {
            unsafe { callback.expect("PPGTT foreach callback is not initialized")(vm, pt, data) };
            unsafe { *start = (*start).wrapping_add(gen8_pt_count(*start, end) as u64) };
        }
        unsafe { spin_lock(&mut (*pd).lock) };
        unsafe { atomic_dec(&mut (*px_used(pt)).used) };
        index += 1;
        len -= 1;
    }
    unsafe { spin_unlock(&mut (*pd).lock) };
}

// upstream: gen8_ppgtt.c gen8_ppgtt_foreach()
unsafe extern "C" fn gen8_ppgtt_foreach(
    vm: *mut I915AddressSpace,
    start: u64,
    length: u64,
    callback: Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915PageTable, *mut c_void)>,
    data: *mut c_void,
) {
    let mut start = start >> GEN8_PTE_SHIFT;
    let length = length >> GEN8_PTE_SHIFT;
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    unsafe {
        __gen8_ppgtt_foreach(
            vm,
            (*ppgtt).pd,
            &mut start,
            start + length,
            (*vm).top as u32,
            callback,
            data,
        )
    };
}

// upstream: gen8_ppgtt.c gen8_ppgtt_insert_pte()
unsafe fn gen8_ppgtt_insert_pte(
    ppgtt: *mut I915Ppgtt,
    pdp: *mut I915PageDirectory,
    iter: *mut SgtDma,
    mut index: u64,
    pat_index: u32,
    flags: u32,
) -> u64 {
    let vm = unsafe { ptr::addr_of_mut!((*ppgtt).vm) };
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pte_encode = unsafe { encoder(0, pat_index, flags) };
    let mut pd = unsafe { i915_pd_entry(pdp, gen8_pd_index(index, 2) as u16) };
    let mut pt = unsafe { i915_pt_entry(pd, gen8_pd_index(index, 1) as u16) };
    let mut vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
    loop {
        GEM_BUG_ON!(sg_dma_len(unsafe { (*iter).sg }) < I915_GTT_PAGE_SIZE as u32);
        unsafe {
            vaddr
                .add(gen8_pd_index(index, 0) as usize)
                .write(pte_encode | (*iter).dma)
        };
        unsafe { (*iter).dma = (*iter).dma.wrapping_add(I915_GTT_PAGE_SIZE) };
        if unsafe { (*iter).dma >= (*iter).max } {
            unsafe { (*iter).sg = next_sg((*iter).sg) };
            if unsafe { (*iter).sg.is_null() || sg_dma_len((*iter).sg) == 0 } {
                index = 0;
                break;
            }
            unsafe {
                (*iter).dma = sg_dma_address((*iter).sg);
                (*iter).max = (*iter).dma.wrapping_add(sg_dma_len((*iter).sg) as u64);
            }
        }
        index = index.wrapping_add(1);
        if gen8_pd_index(index, 0) == 0 {
            if gen8_pd_index(index, 1) == 0 {
                if gen8_pd_index(index, 2) == 0 {
                    break;
                }
                pd = unsafe { (*pdp).entry.add(gen8_pd_index(index, 2) as usize).read() }
                    .cast::<I915PageDirectory>();
            }
            unsafe {
                crate::i915_gem_clflush_upstream::drm_clflush_virt_range(
                    vaddr.cast(),
                    PAGE_SIZE as c_ulong,
                )
            };
            pt = unsafe { i915_pt_entry(pd, gen8_pd_index(index, 1) as u16) };
            vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
        }
    }
    unsafe {
        crate::i915_gem_clflush_upstream::drm_clflush_virt_range(vaddr.cast(), PAGE_SIZE as c_ulong)
    };
    index
}

// upstream: gen8_ppgtt.c xehp_ppgtt_insert_huge()
unsafe fn xehp_ppgtt_insert_huge(
    vm: *mut I915AddressSpace,
    vma_res: *mut I915VmaResource,
    iter: *mut SgtDma,
    pat_index: u32,
    flags: u32,
) {
    GEM_BUG_ON!(!unsafe { i915_vm_is_4lvl(vm) });
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pte_encode = unsafe { encoder(0, pat_index, flags) };
    let mut rem = unsafe { sg_dma_len((*iter).sg) };
    let mut start = unsafe { (*vma_res).start };
    let end = start.wrapping_add(unsafe { (*vma_res).vma_size });
    while !unsafe { (*iter).sg.is_null() } && unsafe { sg_dma_len((*iter).sg) } != 0 {
        let pdp = unsafe { gen8_pdp_for_page_address(vm, start) };
        let pd = unsafe { i915_pd_entry(pdp, gen8_pte_index(start, 2) as u16) };
        let pt = unsafe { i915_pt_entry(pd, gen8_pte_index(start, 1) as u16) };
        let mut encode = pte_encode;
        let mut index: u16;
        let mut max: u16 = I915_PDES as u16;
        let mut nent: u16 = 1;
        let mut page_size: u32;
        let mut vaddr: *mut u64;
        if unsafe { (*vma_res).bi.page_sizes.sg } & I915_GTT_PAGE_SIZE_2M as u32 != 0
            && IS_ALIGNED!(unsafe { (*iter).dma }, I915_GTT_PAGE_SIZE_2M)
            && rem as u64 >= I915_GTT_PAGE_SIZE_2M
            && gen8_pte_index(start, 0) == 0
        {
            index = gen8_pte_index(start, 1) as u16;
            encode |= GEN8_PDE_PS_2M as u64;
            page_size = I915_GTT_PAGE_SIZE_2M as u32;
            vaddr = unsafe { px_vaddr(pd) }.cast::<u64>();
        } else {
            index = gen8_pte_index(start, 0) as u16;
            page_size = I915_GTT_PAGE_SIZE as u32;
            if unsafe { (*vma_res).bi.page_sizes.sg } & I915_GTT_PAGE_SIZE_64K as u32 != 0 {
                if encode & GEN12_PPGTT_PTE_LM != 0
                    && end - start >= I915_GTT_PAGE_SIZE_2M
                    && index == 0
                {
                    index = (gen8_pte_index(start, 0) / 16) as u16;
                    page_size = I915_GTT_PAGE_SIZE_64K as u32;
                    max /= 16;
                    let pd_vaddr = unsafe { px_vaddr(pd) }.cast::<u64>();
                    unsafe {
                        *pd_vaddr.add(gen8_pte_index(start, 1) as usize) |= GEN12_PDE_64K as u64;
                        (*pt).is_compact = true;
                    }
                } else if IS_ALIGNED!(unsafe { (*iter).dma }, I915_GTT_PAGE_SIZE_64K)
                    && rem as u64 >= I915_GTT_PAGE_SIZE_64K
                    && index % 16 == 0
                {
                    encode |= GEN12_PTE_PS64 as u64;
                    page_size = I915_GTT_PAGE_SIZE_64K as u32;
                    nent = 16;
                }
            }
            vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
        }

        loop {
            GEM_BUG_ON!(rem < page_size);
            for entry in 0..nent {
                unsafe {
                    vaddr.add(index as usize).write(
                        encode | (*iter).dma.wrapping_add(entry as u64 * I915_GTT_PAGE_SIZE),
                    );
                }
                index += 1;
            }
            start = start.wrapping_add(page_size as u64);
            unsafe { (*iter).dma = (*iter).dma.wrapping_add(page_size as u64) };
            rem -= page_size;
            if unsafe { (*iter).dma >= (*iter).max } {
                unsafe { (*iter).sg = next_sg((*iter).sg) };
                if unsafe { (*iter).sg.is_null() } {
                    break;
                }
                rem = unsafe { sg_dma_len((*iter).sg) };
                if rem == 0 {
                    break;
                }
                unsafe {
                    (*iter).dma = sg_dma_address((*iter).sg);
                    (*iter).max = (*iter).dma.wrapping_add(rem as u64);
                }
                if !IS_ALIGNED!(unsafe { (*iter).dma }, page_size as u64) {
                    break;
                }
            }
            if rem < page_size || index >= max {
                break;
            }
        }
        unsafe {
            crate::i915_gem_clflush_upstream::drm_clflush_virt_range(
                vaddr.cast(),
                PAGE_SIZE as c_ulong,
            )
        };
        unsafe { (*vma_res).page_sizes_gtt |= page_size };
    }
}

// upstream: gen8_ppgtt.c gen8_ppgtt_insert_huge()
unsafe fn gen8_ppgtt_insert_huge(
    vm: *mut I915AddressSpace,
    vma_res: *mut I915VmaResource,
    iter: *mut SgtDma,
    pat_index: u32,
    flags: u32,
) {
    GEM_BUG_ON!(!unsafe { i915_vm_is_4lvl(vm) });
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pte_encode = unsafe { encoder(0, pat_index, flags) };
    let mut rem = unsafe { sg_dma_len((*iter).sg) };
    let mut start = unsafe { (*vma_res).start };
    while !unsafe { (*iter).sg.is_null() } && unsafe { sg_dma_len((*iter).sg) } != 0 {
        let pdp = unsafe { gen8_pdp_for_page_address(vm, start) };
        let pd = unsafe { i915_pd_entry(pdp, gen8_pte_index(start, 2) as u16) };
        let mut encode = pte_encode;
        let mut maybe_64k = u32::MAX;
        let mut page_size: u32;
        let mut vaddr: *mut u64;
        let mut index: u16;
        if unsafe { (*vma_res).bi.page_sizes.sg } & I915_GTT_PAGE_SIZE_2M as u32 != 0
            && IS_ALIGNED!(unsafe { (*iter).dma }, I915_GTT_PAGE_SIZE_2M)
            && rem as u64 >= I915_GTT_PAGE_SIZE_2M
            && gen8_pte_index(start, 0) == 0
        {
            index = gen8_pte_index(start, 1) as u16;
            encode |= GEN8_PDE_PS_2M as u64;
            page_size = I915_GTT_PAGE_SIZE_2M as u32;
            vaddr = unsafe { px_vaddr(pd) }.cast::<u64>();
        } else {
            let pt = unsafe { i915_pt_entry(pd, gen8_pte_index(start, 1) as u16) };
            index = gen8_pte_index(start, 0) as u16;
            page_size = I915_GTT_PAGE_SIZE as u32;
            if index == 0
                && unsafe { (*vma_res).bi.page_sizes.sg } & I915_GTT_PAGE_SIZE_64K as u32 != 0
                && IS_ALIGNED!(unsafe { (*iter).dma }, I915_GTT_PAGE_SIZE_64K)
                && (IS_ALIGNED!(rem as u64, I915_GTT_PAGE_SIZE_64K)
                    || rem as u64 >= (I915_PDES as u64 - index as u64) * I915_GTT_PAGE_SIZE)
            {
                maybe_64k = gen8_pte_index(start, 1);
            }
            vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
        }

        loop {
            GEM_BUG_ON!(rem < page_size);
            unsafe { vaddr.add(index as usize).write(encode | (*iter).dma) };
            index += 1;
            start = start.wrapping_add(page_size as u64);
            unsafe { (*iter).dma = (*iter).dma.wrapping_add(page_size as u64) };
            rem -= page_size;
            if unsafe { (*iter).dma >= (*iter).max } {
                unsafe { (*iter).sg = next_sg((*iter).sg) };
                if unsafe { (*iter).sg.is_null() } {
                    break;
                }
                rem = unsafe { sg_dma_len((*iter).sg) };
                if rem == 0 {
                    break;
                }
                unsafe {
                    (*iter).dma = sg_dma_address((*iter).sg);
                    (*iter).max = (*iter).dma.wrapping_add(rem as u64);
                }
                if maybe_64k != u32::MAX
                    && (index as u32) < I915_PDES
                    && !(IS_ALIGNED!(unsafe { (*iter).dma }, I915_GTT_PAGE_SIZE_64K)
                        && (IS_ALIGNED!(rem as u64, I915_GTT_PAGE_SIZE_64K)
                            || rem as u64
                                >= (I915_PDES as u64 - index as u64) * I915_GTT_PAGE_SIZE))
                {
                    maybe_64k = u32::MAX;
                }
                if !IS_ALIGNED!(unsafe { (*iter).dma }, page_size as u64) {
                    break;
                }
            }
            if rem < page_size || (index as u32) >= I915_PDES {
                break;
            }
        }
        unsafe {
            crate::i915_gem_clflush_upstream::drm_clflush_virt_range(
                vaddr.cast(),
                PAGE_SIZE as c_ulong,
            )
        };
        if maybe_64k != u32::MAX
            && ((index as u32) == I915_PDES
                || (unsafe { i915_vm_has_scratch_64K(vm) }
                    && unsafe { (*iter).sg.is_null() }
                    && IS_ALIGNED!(
                        unsafe { (*vma_res).start + (*vma_res).node_size },
                        I915_GTT_PAGE_SIZE_2M
                    )))
        {
            vaddr = unsafe { px_vaddr(pd) }.cast::<u64>();
            unsafe {
                *vaddr.add(maybe_64k as usize) |=
                    crate::intel_gtt_api_upstream::GEN8_PDE_IPS_64K as u64
            };
            unsafe {
                crate::i915_gem_clflush_upstream::drm_clflush_virt_range(
                    vaddr.cast(),
                    PAGE_SIZE as c_ulong,
                )
            };
            page_size = I915_GTT_PAGE_SIZE_64K as u32;
            // The following scrub_64K verification block is compiled only
            // with CONFIG_DRM_I915_SELFTEST; this target selects it off.
        }
        unsafe { (*vma_res).page_sizes_gtt |= page_size };
    }
}

// upstream: gen8_ppgtt.c gen8_ppgtt_insert()
unsafe extern "C" fn gen8_ppgtt_insert(
    vm: *mut I915AddressSpace,
    vma_res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_vm_to_ppgtt(vm) };
    let mut iter = unsafe { sgt_dma(vma_res) };
    if unsafe { (*vma_res).bi.page_sizes.sg } > I915_GTT_PAGE_SIZE as u32 {
        if unsafe { GRAPHICS_VER_FULL((*vm).i915) >= IP_VER(12, 55) } {
            unsafe { xehp_ppgtt_insert_huge(vm, vma_res, &mut iter, pat_index, flags) };
        } else {
            unsafe { gen8_ppgtt_insert_huge(vm, vma_res, &mut iter, pat_index, flags) };
        }
    } else {
        let mut index = unsafe { (*vma_res).start } >> GEN8_PTE_SHIFT;
        loop {
            let pdp = unsafe { gen8_pdp_for_page_index(vm, index) };
            index =
                unsafe { gen8_ppgtt_insert_pte(ppgtt, pdp, &mut iter, index, pat_index, flags) };
            if index == 0 {
                break;
            }
        }
        unsafe { (*vma_res).page_sizes_gtt = I915_GTT_PAGE_SIZE as u32 };
    }
}

// upstream: gen8_ppgtt.c gen8_ppgtt_insert_entry()
unsafe extern "C" fn gen8_ppgtt_insert_entry(
    vm: *mut I915AddressSpace,
    addr: u64,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    let index = offset >> GEN8_PTE_SHIFT;
    let pdp = unsafe { gen8_pdp_for_page_index(vm, index) };
    let pd = unsafe { i915_pd_entry(pdp, gen8_pd_index(index, 2) as u16) };
    let pt = unsafe { i915_pt_entry(pd, gen8_pd_index(index, 1) as u16) };
    GEM_BUG_ON!(unsafe { (*pt).is_compact });
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pte = unsafe { encoder(addr, pat_index, flags) };
    let vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
    let entry = unsafe { vaddr.add(gen8_pd_index(index, 0) as usize) };
    unsafe { entry.write(pte) };
    unsafe {
        crate::i915_gem_clflush_upstream::drm_clflush_virt_range(
            entry.cast(),
            size_of::<u64>() as c_ulong,
        )
    };
}

// upstream: gen8_ppgtt.c xehp_ppgtt_insert_entry_lm()
unsafe fn xehp_ppgtt_insert_entry_lm(
    vm: *mut I915AddressSpace,
    addr: u64,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    GEM_BUG_ON!(!IS_ALIGNED!(addr, SZ_64K));
    GEM_BUG_ON!(!IS_ALIGNED!(offset, SZ_64K));
    let index = offset >> GEN8_PTE_SHIFT;
    let pdp = unsafe { gen8_pdp_for_page_index(vm, index) };
    let pd = unsafe { i915_pd_entry(pdp, gen8_pd_index(index, 2) as u16) };
    let pt = unsafe { i915_pt_entry(pd, gen8_pd_index(index, 1) as u16) };
    if !unsafe { (*pt).is_compact } {
        let vaddr = unsafe { px_vaddr(pd) }.cast::<u64>();
        unsafe {
            *vaddr.add(gen8_pd_index(index, 1) as usize) |= GEN12_PDE_64K as u64;
            (*pt).is_compact = true;
        }
    }
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pte = unsafe { encoder(addr, pat_index, flags) };
    let vaddr = unsafe { px_vaddr(pt) }.cast::<u64>();
    unsafe {
        vaddr
            .add((gen8_pd_index(index, 0) / 16) as usize)
            .write(pte)
    };
}

// upstream: gen8_ppgtt.c xehp_ppgtt_insert_entry()
unsafe extern "C" fn xehp_ppgtt_insert_entry(
    vm: *mut I915AddressSpace,
    addr: u64,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    if flags & PTE_LM != 0 {
        unsafe { xehp_ppgtt_insert_entry_lm(vm, addr, offset, pat_index, flags) };
    } else {
        unsafe { gen8_ppgtt_insert_entry(vm, addr, offset, pat_index, flags) };
    }
}

// upstream: gen8_ppgtt.c gen8_init_scratch()
unsafe fn gen8_init_scratch(vm: *mut I915AddressSpace) -> c_int {
    if unsafe { (*vm).vm_flags & (1 << 2) != 0 }
        && !unsafe { (*(*vm).gt).vm.is_null() }
        && !unsafe { crate::intel_gtt_api_upstream::i915_is_ggtt((*(*vm).gt).vm) }
    {
        let clone = unsafe { (*(*vm).gt).vm };
        GEM_BUG_ON!(unsafe { (*clone).vm_flags & (1 << 2) == 0 });
        unsafe { (*vm).scratch_order = (*clone).scratch_order };
        let mut index = 0;
        while index <= unsafe { (*vm).top } {
            let scratch = unsafe { (*clone).scratch[index as usize] };
            unsafe { (*vm).scratch[index as usize] = i915_gem_object_get(scratch) };
            index += 1;
        }
        return 0;
    }

    let mut ret = unsafe { crate::intel_gtt_api_upstream::setup_scratch_page(vm) };
    if ret != 0 {
        return ret;
    }
    let mut pte_flags = u32::from(unsafe { (*vm).vm_flags & (1 << 2) != 0 });
    if unsafe { i915_gem_object_is_lmem((*vm).scratch[0]) } {
        pte_flags |= PTE_LM;
    }
    let encoder = unsafe {
        (*vm)
            .pte_encode
            .expect("PPGTT PTE encoder is not initialized")
    };
    let pat_index =
        unsafe { i915_gem_get_pat_index((*vm).i915, I915CacheLevel::I915_CACHE_NONE as u32) };
    let scratch0 = unsafe { (*vm).scratch[0] };
    unsafe {
        (*scratch0).backing.encode = encoder(px_dma(scratch0), pat_index, pte_flags);
    }

    let mut index = 1;
    while index <= unsafe { (*vm).top } {
        let alloc = unsafe { (*vm).alloc_pt_dma }.expect("PT DMA allocator is not initialized");
        let obj = unsafe { alloc(vm, I915_GTT_PAGE_SIZE_4K as c_int) };
        if crate::linux_config::IS_ERR(obj) {
            ret = crate::linux_config::PTR_ERR(obj);
            break;
        }
        ret = unsafe { crate::intel_gtt_api_upstream::map_pt_dma(vm, obj) };
        if ret != 0 {
            unsafe { i915_gem_object_put(obj) };
            break;
        }
        let fill = unsafe { (*vm).scratch[index as usize - 1] };
        unsafe { fill_px(obj, (*fill).backing.encode) };
        unsafe {
            (*obj).backing.encode = gen8_pde_encode(px_dma(obj), I915CacheLevel::I915_CACHE_NONE);
            (*vm).scratch[index as usize] = obj;
        }
        index += 1;
    }
    if ret == 0 {
        return 0;
    }
    while index > 0 {
        index -= 1;
        let scratch = unsafe { (*vm).scratch[index as usize] };
        if !scratch.is_null() {
            unsafe { i915_gem_object_put(scratch) };
        }
    }
    unsafe { (*vm).scratch[0] = ptr::null_mut() };
    ret
}

// upstream: gen8_ppgtt.c gen8_preallocate_top_level_pdp()
unsafe fn gen8_preallocate_top_level_pdp(ppgtt: *mut I915Ppgtt) -> c_int {
    let vm = unsafe { ptr::addr_of_mut!((*ppgtt).vm) };
    let pd = unsafe { (*ppgtt).pd };
    GEM_BUG_ON!(unsafe { (*vm).top != 2 });
    GEM_BUG_ON!(unsafe { gen8_pd_top_count(vm) != crate::intel_gtt_api_upstream::GEN8_3LVL_PDPES });
    let mut index = 0;
    while index < crate::intel_gtt_api_upstream::GEN8_3LVL_PDPES {
        let pde = unsafe { alloc_pd(vm) };
        if crate::linux_config::IS_ERR(pde) {
            return crate::linux_config::PTR_ERR(pde);
        }
        let base = unsafe { (*pde).pt.base };
        let ret = unsafe { crate::intel_gtt_api_upstream::map_pt_dma(vm, base) };
        if ret != 0 {
            unsafe { free_pd(vm, pde) };
            return ret;
        }
        let scratch = unsafe { (*vm).scratch[1] };
        unsafe { fill_px(pde, (*scratch).backing.encode) };
        unsafe { set_pd_entry_with_encoder(pd, index as u16, pde, gen8_pde_encode) };
        unsafe { atomic_inc(&mut (*px_used(pde)).used) };
        index += 1;
    }
    unsafe { wmb() };
    0
}

// upstream: gen8_ppgtt.c gen8_alloc_top_pd()
unsafe fn gen8_alloc_top_pd(vm: *mut I915AddressSpace) -> *mut I915PageDirectory {
    let count = unsafe { gen8_pd_top_count(vm) };
    GEM_BUG_ON!(count > I915_PDES);
    let pd = unsafe { __alloc_pd(count as c_int) };
    if pd.is_null() {
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    let alloc = unsafe { (*vm).alloc_pt_dma }.expect("PT DMA allocator is not initialized");
    let base = unsafe { alloc(vm, I915_GTT_PAGE_SIZE_4K as c_int) };
    if crate::linux_config::IS_ERR(base) {
        let err = crate::linux_config::PTR_ERR(base);
        unsafe { (*pd).pt.base = ptr::null_mut() };
        unsafe { free_pd(vm, pd) };
        return crate::linux_config::ERR_PTR(err);
    }
    unsafe { (*pd).pt.base = base };
    let ret = unsafe { crate::intel_gtt_api_upstream::map_pt_dma(vm, base) };
    if ret != 0 {
        unsafe { free_pd(vm, pd) };
        return crate::linux_config::ERR_PTR(ret);
    }
    let fill = unsafe { (*vm).scratch[(*vm).top as usize] };
    unsafe { fill_page_dma(px_base(pd), (*fill).backing.encode, count) };
    unsafe { atomic_inc(&mut (*px_used(pd)).used) };
    pd
}

// upstream: gen8_ppgtt.c gen8_init_rsvd()
unsafe fn gen8_init_rsvd(vm: *mut I915AddressSpace) -> c_int {
    let i915 = unsafe { (*vm).i915 };
    if !unsafe { intel_gt_needs_wa_16018031267((*vm).gt) } {
        return 0;
    }
    let mut obj = unsafe {
        i915_gem_object_create_lmem(
            i915,
            PAGE_SIZE as u64,
            (I915_BO_ALLOC_VOLATILE | I915_BO_ALLOC_GPU_ONLY) as u32,
        )
    };
    if crate::linux_config::IS_ERR(obj) {
        obj = unsafe { i915_gem_object_create_internal(i915, PAGE_SIZE as u64) };
    }
    if crate::linux_config::IS_ERR(obj) {
        return crate::linux_config::PTR_ERR(obj);
    }
    let vma = unsafe { i915_vma_instance(obj, vm, ptr::null()) };
    if crate::linux_config::IS_ERR(vma) {
        let err = crate::linux_config::PTR_ERR(vma);
        unsafe { i915_gem_object_put(obj) };
        return err;
    }
    let ret = unsafe { i915_vma_pin(vma, 0, 0, PIN_USER | PIN_HIGH) };
    if ret != 0 {
        unsafe { i915_gem_object_put(obj) };
        return ret;
    }
    unsafe {
        (*vm).rsvd.vma = i915_vma_make_unshrinkable(vma);
        (*vm).rsvd.obj = obj;
        (*vm).total = (*vm).total.wrapping_sub((*vma).node.size);
    }
    0
}

// upstream: gen8_ppgtt.c gen8_ppgtt_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_ppgtt_create(
    gt: *mut IntelGt,
    lmem_pt_obj_flags: c_ulong,
) -> *mut I915Ppgtt {
    let ppgtt = crate::linux::memory::kzalloc_obj::<I915Ppgtt>();
    if ppgtt.is_null() {
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    unsafe { crate::intel_gtt_api_upstream::ppgtt_init(ppgtt, gt, lmem_pt_obj_flags) };
    let vm = unsafe { ptr::addr_of_mut!((*ppgtt).vm) };
    unsafe {
        (*vm).top = if i915_vm_is_4lvl(vm) { 3 } else { 2 };
        (*vm).pd_shift =
            ilog2((GEN8_PAGE_SIZE * GEN8_PAGE_SIZE / size_of::<gen8_pte_t>() as u64) as usize)
                as u8;
    }
    let i915 = unsafe { (*gt).i915 };
    let has_read_only = !unsafe { IS_GRAPHICS_VER(i915, 11, 12) };
    unsafe {
        if has_read_only {
            (*vm).vm_flags |= 1 << 2;
        } else {
            (*vm).vm_flags &= !(1 << 2);
        }
        (*vm).alloc_pt_dma = Some(if HAS_LMEM(i915) {
            crate::intel_gtt_upstream::alloc_pt_lmem
        } else {
            crate::intel_gtt_upstream::alloc_pt_dma
        });
        (*vm).alloc_scratch_dma = Some(crate::intel_gtt_upstream::alloc_pt_dma);
        (*vm).pte_encode = Some(if GRAPHICS_VER(i915) >= 12 {
            gen12_pte_encode
        } else {
            gen8_pte_encode
        });
        (*vm).bind_async_flags = I915_VMA_LOCAL_BIND as u32;
        (*vm).insert_entries = Some(gen8_ppgtt_insert);
        (*vm).insert_page = Some(if has_64k_pages(i915) {
            xehp_ppgtt_insert_entry
        } else {
            gen8_ppgtt_insert_entry
        });
        (*vm).allocate_va_range = Some(gen8_ppgtt_alloc);
        (*vm).clear_range = Some(gen8_ppgtt_clear);
        (*vm).foreach = Some(gen8_ppgtt_foreach);
        (*vm).cleanup = Some(gen8_ppgtt_cleanup);
    }

    let mut err = unsafe { gen8_init_scratch(vm) };
    if err != 0 {
        unsafe { i915_vm_put(vm) };
        return crate::linux_config::ERR_PTR(err);
    }
    let pd = unsafe { gen8_alloc_top_pd(vm) };
    if crate::linux_config::IS_ERR(pd) {
        err = crate::linux_config::PTR_ERR(pd);
        unsafe { i915_vm_put(vm) };
        return crate::linux_config::ERR_PTR(err);
    }
    unsafe { (*ppgtt).pd = pd };
    if !unsafe { i915_vm_is_4lvl(vm) } {
        err = unsafe { gen8_preallocate_top_level_pdp(ppgtt) };
        if err != 0 {
            unsafe { i915_vm_put(vm) };
            return crate::linux_config::ERR_PTR(err);
        }
    }
    #[cfg(CONFIG_DRM_I915_GVT)]
    if unsafe { intel_vgpu_active(i915) } {
        unsafe { gen8_ppgtt_notify_vgt(ppgtt, true) };
    }
    err = unsafe { gen8_init_rsvd(vm) };
    if err != 0 {
        unsafe { i915_vm_put(vm) };
        return crate::linux_config::ERR_PTR(err);
    }
    ppgtt
}
