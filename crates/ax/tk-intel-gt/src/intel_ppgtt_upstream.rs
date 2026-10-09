// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_ppgtt.c.
// The full MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    ptr,
};

use crate::{
    i915_gem_clflush_upstream::dma_resv_init,
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915CacheLevel},
    i915_vma_api_upstream::vma_invalidate_tlb,
    i915_vma_resource_types_upstream::I915VmaResource,
    intel_engine_api_upstream::drm_clflush_virt_range as clflush_range,
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{
        __px_vaddr, I915_GFP_ALLOW_FAIL, I915_GTT_PAGE_SIZE_4K, I915_PDES, I915AddressSpace,
        I915PageDirectory, I915PageTable, I915Ppgtt, I915VmPtStash, NALLOC, PTE_LM, PTE_READ_ONLY,
        VM_CLASS_PPGTT, i915_address_space_init, map_pt_dma_locked, px_base,
        px_dma,
    },
    linux::{
        config::ENOMEM,
        gem::DmaResv,
        i915::{GRAPHICS_VER, IS_DGFX},
        locks::{spin_lock, spin_lock_init, spin_unlock},
        memory::{
            atomic_add_unless, atomic_dec, atomic_dec_and_test, atomic_inc, atomic_read,
            atomic_set, kfree, kmalloc_obj, kzalloc_obj_flags, kzalloc_objs_flags,
        },
        primitives::{ilog2, is_power_of_2, wmb},
    },
};

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn gen6_ppgtt_enable(gt: *mut IntelGt);
    fn gen7_ppgtt_enable(gt: *mut IntelGt);
    fn gen6_ppgtt_create(gt: *mut IntelGt) -> *mut I915Ppgtt;
    fn gen8_ppgtt_create(gt: *mut IntelGt, lmem_pt_obj_flags: c_ulong) -> *mut I915Ppgtt;
}

const _: () = assert!(core::mem::offset_of!(I915PageDirectory, pt) == 0);

// upstream: intel_ppgtt.c alloc_pt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_pt(vm: *mut I915AddressSpace, size: c_int) -> *mut I915PageTable {
    let pt = kmalloc_obj::<I915PageTable>(I915_GFP_ALLOW_FAIL);
    if pt.is_null() {
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    let alloc =
        unsafe { (*vm).alloc_pt_dma }.expect("i915_address_space.alloc_pt_dma is uninitialized");
    let base = unsafe { alloc(vm, size) };
    if crate::linux_config::IS_ERR(base) {
        unsafe { kfree(pt.cast::<c_void>()) };
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    unsafe {
        (*pt).base = base;
        (*pt).is_compact = false;
        atomic_set(&mut (*pt).used_or_stash.used, 0);
    }
    pt
}

// upstream: intel_ppgtt.c __alloc_pd()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __alloc_pd(count: c_int) -> *mut I915PageDirectory {
    let pd = kzalloc_obj_flags::<I915PageDirectory>(I915_GFP_ALLOW_FAIL);
    if pd.is_null() {
        return ptr::null_mut();
    }
    let entries = kzalloc_objs_flags::<*mut c_void, _>(count as usize, I915_GFP_ALLOW_FAIL);
    if entries.is_null() {
        unsafe { kfree(pd.cast::<c_void>()) };
        return ptr::null_mut();
    }
    unsafe {
        (*pd).entry = entries;
        spin_lock_init(&mut (*pd).lock);
    }
    pd
}

// upstream: intel_ppgtt.c alloc_pd()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_pd(vm: *mut I915AddressSpace) -> *mut I915PageDirectory {
    let pd = unsafe { __alloc_pd(I915_PDES as c_int) };
    if pd.is_null() {
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    let alloc =
        unsafe { (*vm).alloc_pt_dma }.expect("i915_address_space.alloc_pt_dma is uninitialized");
    let base = unsafe { alloc(vm, I915_GTT_PAGE_SIZE_4K as c_int) };
    if crate::linux_config::IS_ERR(base) {
        unsafe {
            kfree((*pd).entry.cast::<c_void>());
            kfree(pd.cast::<c_void>());
        }
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    unsafe { (*pd).pt.base = base };
    pd
}

// upstream: intel_ppgtt.c free_px()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free_px(vm: *mut I915AddressSpace, pt: *mut I915PageTable, level: c_int) {
    let _ = vm;
    BUILD_BUG_ON!(core::mem::offset_of!(I915PageDirectory, pt) != 0);
    if level != 0 {
        let pd = pt.cast::<I915PageDirectory>();
        unsafe { kfree((*pd).entry.cast::<c_void>()) };
    }
    if !unsafe { (*pt).base.is_null() } {
        unsafe { i915_gem_object_put((*pt).base) };
    }
    unsafe { kfree(pt.cast::<c_void>()) };
}

// upstream: intel_ppgtt.c write_dma_entry()
unsafe fn write_dma_entry(pdma: *mut DrmI915GemObject, index: u16, encoded_entry: u64) {
    let vaddr = unsafe { __px_vaddr(pdma) }.cast::<u64>();
    let entry = unsafe { vaddr.add(index as usize) };
    unsafe { entry.write(encoded_entry) };
    unsafe { clflush_range(entry.cast(), core::mem::size_of::<u64>() as c_ulong) };
}

// upstream: intel_ppgtt.c __set_pd_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __set_pd_entry(
    pd: *mut I915PageDirectory,
    index: u16,
    to: *mut I915PageTable,
    encode: unsafe extern "C" fn(u64, I915CacheLevel) -> u64,
) {
    let used = unsafe { &mut (*pd).pt.used_or_stash.used };
    GEM_BUG_ON!(atomic_read(used) > (NALLOC * I915_PDES) as i32);
    atomic_inc(used);
    unsafe {
        *(*pd).entry.add(index as usize) = to.cast::<c_void>();
        write_dma_entry(
            px_base(pd),
            index,
            encode(px_dma(to), I915CacheLevel::I915_CACHE_LLC),
        );
    }
}

// upstream: intel_ppgtt.c clear_pd_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clear_pd_entry(
    pd: *mut I915PageDirectory,
    index: u16,
    scratch: *const DrmI915GemObject,
) {
    let used = unsafe { &mut (*pd).pt.used_or_stash.used };
    GEM_BUG_ON!(atomic_read(used) == 0);
    let encoded = unsafe { (*scratch).backing.encode };
    unsafe {
        write_dma_entry(px_base(pd), index, encoded);
        *(*pd).entry.add(index as usize) = ptr::null_mut();
        atomic_dec(used);
    }
}

// upstream: intel_ppgtt.c release_pd_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn release_pd_entry(
    pd: *mut I915PageDirectory,
    index: u16,
    pt: *mut I915PageTable,
    scratch: *const DrmI915GemObject,
) -> bool {
    let used = unsafe { &mut (*pt).used_or_stash.used };
    if atomic_add_unless(used, -1, 1) {
        return false;
    }

    unsafe { spin_lock(&mut (*pd).lock) };
    let free = unsafe { atomic_dec_and_test(&mut (*pt).used_or_stash.used) };
    if free {
        unsafe { clear_pd_entry(pd, index, scratch) };
    }
    unsafe { spin_unlock(&mut (*pd).lock) };
    free
}

// upstream: intel_ppgtt.c i915_ppgtt_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ppgtt_init_hw(gt: *mut IntelGt) -> c_int {
    let i915 = unsafe { (*gt).i915 };
    unsafe { crate::intel_gtt_api_upstream::gtt_write_workarounds(gt) };
    match unsafe { GRAPHICS_VER(i915) } {
        6 => unsafe { gen6_ppgtt_enable(gt) },
        7 => unsafe { gen7_ppgtt_enable(gt) },
        _ => {}
    }
    0
}

// upstream: intel_ppgtt.c __ppgtt_create()
unsafe fn __ppgtt_create(gt: *mut IntelGt, lmem_pt_obj_flags: c_ulong) -> *mut I915Ppgtt {
    if unsafe { GRAPHICS_VER((*gt).i915) } < 8 {
        unsafe { gen6_ppgtt_create(gt) }
    } else {
        unsafe { gen8_ppgtt_create(gt, lmem_pt_obj_flags) }
    }
}

// upstream: intel_ppgtt.c i915_ppgtt_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ppgtt_create(
    gt: *mut IntelGt,
    lmem_pt_obj_flags: c_ulong,
) -> *mut I915Ppgtt {
    let ppgtt = unsafe { __ppgtt_create(gt, lmem_pt_obj_flags) };
    if crate::linux_config::IS_ERR(ppgtt) {
        return ppgtt;
    }
    crate::linux::i915_trace::trace_i915_ppgtt_create(unsafe { ptr::addr_of_mut!((*ppgtt).vm) });
    ppgtt
}

// upstream: intel_ppgtt.c ppgtt_bind_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppgtt_bind_vma(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    vma_res: *mut I915VmaResource,
    pat_index: u32,
    _flags: u32,
) {
    if unsafe { (*vma_res).state_bits & 1 == 0 } {
        let allocate = unsafe { (*vm).allocate_va_range }
            .expect("i915_address_space.allocate_va_range is uninitialized");
        unsafe { allocate(vm, stash, (*vma_res).start, (*vma_res).vma_size) };
        unsafe { (*vma_res).state_bits |= 1 };
    }

    let mut pte_flags = 0;
    let bind_flags = unsafe { (*vma_res).bi.flags };
    if bind_flags & 1 != 0 {
        pte_flags |= PTE_READ_ONLY;
    }
    if bind_flags & 2 != 0 {
        pte_flags |= PTE_LM;
    }
    let insert = unsafe { (*vm).insert_entries }
        .expect("i915_address_space.insert_entries is uninitialized");
    unsafe { insert(vm, vma_res, pat_index, pte_flags) };
    wmb();
}

// upstream: intel_ppgtt.c ppgtt_unbind_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppgtt_unbind_vma(
    vm: *mut I915AddressSpace,
    vma_res: *mut I915VmaResource,
) {
    if unsafe { (*vma_res).state_bits & 1 == 0 } {
        return;
    }
    let clear =
        unsafe { (*vm).clear_range }.expect("i915_address_space.clear_range is uninitialized");
    unsafe {
        clear(vm, (*vma_res).start, (*vma_res).vma_size);
        vma_invalidate_tlb(vm, (*vma_res).tlb);
    }
}

// upstream: intel_ppgtt.c pd_count()
unsafe fn pd_count(size: u64, shift: c_int) -> u64 {
    assert!((0..64).contains(&shift));
    let unit = 1u64 << shift;
    size.wrapping_add(2u64.wrapping_mul(unit.wrapping_sub(1))) >> shift
}

// upstream: intel_ppgtt.c i915_vm_alloc_pt_stash()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_alloc_pt_stash(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    size: u64,
) -> c_int {
    let mut shift = unsafe { (*vm).pd_shift } as c_int;
    if shift == 0 {
        return 0;
    }

    let mut pt_size = unsafe { (*stash).pt_sz };
    if pt_size == 0 {
        pt_size = I915_GTT_PAGE_SIZE_4K as c_int;
    } else {
        GEM_BUG_ON!(!unsafe { IS_DGFX((*vm).i915) });
    }
    GEM_BUG_ON!(!is_power_of_2(pt_size as u64));

    let mut count = unsafe { pd_count(size, shift) };
    while count != 0 {
        let pt = unsafe { alloc_pt(vm, pt_size) };
        if crate::linux_config::IS_ERR(pt) {
            unsafe { i915_vm_free_pt_stash(vm, stash) };
            return crate::linux_config::PTR_ERR(pt);
        }
        unsafe {
            (*pt).used_or_stash.stash = (*stash).pt[0];
            (*stash).pt[0] = pt;
        }
        count -= 1;
    }

    for _level in 1..unsafe { (*vm).top } {
        shift += ilog2(I915_PDES) as c_int;
        count = unsafe { pd_count(size, shift) };
        while count != 0 {
            let pd = unsafe { alloc_pd(vm) };
            if crate::linux_config::IS_ERR(pd) {
                unsafe { i915_vm_free_pt_stash(vm, stash) };
                return crate::linux_config::PTR_ERR(pd);
            }
            unsafe {
                (*pd).pt.used_or_stash.stash = (*stash).pt[1];
                (*stash).pt[1] = ptr::addr_of_mut!((*pd).pt);
            }
            count -= 1;
        }
    }
    0
}

// upstream: intel_ppgtt.c i915_vm_map_pt_stash()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_map_pt_stash(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
) -> c_int {
    for slot in 0..2 {
        let mut pt = unsafe { (*stash).pt[slot] };
        while !pt.is_null() {
            let next = unsafe { (*pt).used_or_stash.stash };
            let err = unsafe { map_pt_dma_locked(vm, (*pt).base) };
            if err != 0 {
                return err;
            }
            pt = next;
        }
    }
    0
}

// upstream: intel_ppgtt.c i915_vm_free_pt_stash()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_free_pt_stash(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
) {
    for slot in 0..2 {
        while !unsafe { (*stash).pt[slot] }.is_null() {
            let pt = unsafe { (*stash).pt[slot] };
            unsafe {
                (*stash).pt[slot] = (*pt).used_or_stash.stash;
                free_px(vm, pt, slot as c_int);
            }
        }
    }
}

// upstream: intel_ppgtt.c ppgtt_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppgtt_init(
    ppgtt: *mut I915Ppgtt,
    gt: *mut IntelGt,
    lmem_pt_obj_flags: c_ulong,
) {
    let i915 = unsafe { (*gt).i915 };
    unsafe {
        (*ppgtt).vm.gt = gt;
        (*ppgtt).vm.i915 = i915;
        (*ppgtt).vm.dma = (*i915).drm.dev;
        (*ppgtt).vm.total = 1u64 << (*i915).runtime.ppgtt_size;
        (*ppgtt).vm.lmem_pt_obj_flags = lmem_pt_obj_flags;
        dma_resv_init(ptr::addr_of_mut!((*ppgtt).vm._resv).cast::<DmaResv>());
        i915_address_space_init(ptr::addr_of_mut!((*ppgtt).vm), VM_CLASS_PPGTT as c_int);
        (*ppgtt).vm.vma_ops.bind_vma = Some(ppgtt_bind_vma);
        (*ppgtt).vm.vma_ops.unbind_vma = Some(ppgtt_unbind_vma);
    }
}

const _: [(); 24] = [(); core::mem::size_of::<I915PageTable>()];
const _: [(); 40] = [(); core::mem::size_of::<I915PageDirectory>()];
const _: [(); 24] = [(); core::mem::size_of::<I915VmPtStash>()];
