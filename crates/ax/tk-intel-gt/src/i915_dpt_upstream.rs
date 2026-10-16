// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_dpt.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use tk_drm_upstream::drm_gem_types::DrmGemObject;

use crate::{
    i915_gem_core_upstream::i915_gem_object_ggtt_pin_ww,
    i915_gem_domain_upstream::{I915CacheLevel, i915_gem_object_set_cache_level},
    i915_gem_lmem_upstream::i915_gem_object_create_lmem,
    i915_gem_object_api_upstream::{
        i915_gem_object_lock, i915_gem_object_lock_interruptible, i915_gem_object_put,
        i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_shmem_upstream::i915_gem_object_create_shmem,
    i915_gem_stolen_upstream::{i915_gem_object_create_stolen, i915_gem_object_is_stolen},
    i915_gem_ww_upstream::{
        I915GemWwCtx, __i915_gem_ww_fini, i915_gem_ww_ctx_init,
    },
    i915_vma_api_upstream::{i915_vma_get, i915_vma_offset, i915_vma_put, i915_vma_unpin},
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::{I915Vma, I915_VMA_GLOBAL_BIND, I915_VMA_LOCAL_BIND},
    i915_vma_upstream::{PIN_MAPPABLE, i915_vma_pin_iomap, i915_vma_unpin_iomap},
    intel_gtt_api_upstream::{
        I915AddressSpace, I915_GTT_PAGE_SIZE, I915Ggtt, I915VmPtStash, PTE_LM, PTE_READ_ONLY,
        VM_CLASS_DPT, dma_addr_t, for_each_sgt_daddr, for_each_sgt_daddr_next,
        i915_ggtt_has_aperture, i915_is_dpt, i915_vm_put,
    },
    intel_gtt_upstream::i915_address_space_init,
    intel_ggtt_upstream::{i915_ggtt_resume_vm, i915_ggtt_suspend_vm},
    intel_parent_upstream::IntelDisplayDptInterface,
    intel_runtime_pm_upstream::{intel_runtime_pm_get, intel_runtime_pm_put},
    linux::{i915::{HAS_LMEM, to_gt, to_i915}, memory::kzalloc_obj},
    linux_config::{ENOMEM, EDEADLK, ERR_PTR, IS_ERR, PTR_ERR, I915_BO_ALLOC_CONTIGUOUS},
    linux_i915_private::DrmI915Private,
    linux_memory::{atomic_dec, atomic_inc},
};

/// `struct i915_address_space` `is_dpt` bit (`vm_flags` bit 1, intel_gtt_api_upstream).
const VM_FLAG_IS_DPT: u8 = 1 << 1;
/// `struct i915_address_space` `has_read_only` bit (`vm_flags` bit 2).
const VM_FLAG_HAS_READ_ONLY: u8 = 1 << 2;
/// `struct drm_i915_gem_object` `is_dpt` bit in `cache_state_bits` (bit 10).
const OBJ_CACHE_IS_DPT: u32 = 1 << 10;
/// `struct drm_i915_gem_object` `mm.dirty` bit in `madv_dirty_bits` (bit 2).
const OBJ_MM_DIRTY: u32 = 1 << 2;
/// `sizeof(gen8_pte_t)`.
const GEN8_PTE_SIZE: u64 = 8;
/// `drm_i915_private.pending_fb_pin` (`atomic_t`, i915_drv.h:318) byte offset.
/// The field directly follows `struct ttm_device bdev` (i915_drv.h:316-318).
/// Derived from the overlay's `bdev` position; the overlay comment says 4248
/// but offset_of gives 4256, so this is unverified against the C layout.
const PENDING_FB_PIN_OFFSET: usize =
    offset_of!(DrmI915Private, bdev) + size_of::<crate::linux::ttm::TtmDevice>();

/// `struct intel_dpt` from i915_dpt.c. `vm` must stay at offset 0.
#[repr(C)]
pub struct IntelDpt {
    pub vm: I915AddressSpace,
    pub obj: *mut DrmI915GemObject,
    pub vma: *mut I915Vma,
    pub iomem: *mut c_void,
}

const _: () = assert!(offset_of!(IntelDpt, vm) == 0);
const _: [(); 704] = [(); size_of::<IntelDpt>()];

// upstream: i915_dpt.c i915_dpt_to_vm()
pub unsafe fn i915_dpt_to_vm(dpt: *mut IntelDpt) -> *mut I915AddressSpace {
    unsafe { ptr::addr_of_mut!((*dpt).vm) }
}

// upstream: i915_dpt.c i915_vm_to_dpt()
#[inline]
unsafe fn i915_vm_to_dpt(vm: *mut I915AddressSpace) -> *mut IntelDpt {
    // drm_WARN_ON(&vm->i915->drm, !i915_is_dpt(vm));
    WARN_ON!(unsafe { !i915_is_dpt(vm) });
    // container_of(vm, struct intel_dpt, vm) with vm at offset 0.
    vm.cast::<IntelDpt>()
}

// upstream: i915_dpt.c gen8_set_pte()
#[inline]
unsafe fn gen8_set_pte(addr: *mut u64, pte: u64) {
    // writeq(pte, addr)
    unsafe { ptr::write_volatile(addr, pte) };
}

// upstream: i915_dpt.c dpt_insert_page()
unsafe extern "C" fn dpt_insert_page(
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    let dpt = unsafe { i915_vm_to_dpt(vm) };
    let base = unsafe { (*dpt).iomem.cast::<u64>() };
    let pte = unsafe { (*vm).pte_encode.unwrap()(addr, pat_index, flags) };
    unsafe { gen8_set_pte(base.add((offset / I915_GTT_PAGE_SIZE) as usize), pte) };
}

// upstream: i915_dpt.c dpt_insert_entries()
unsafe extern "C" fn dpt_insert_entries(
    vm: *mut I915AddressSpace,
    vma_res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let dpt = unsafe { i915_vm_to_dpt(vm) };
    let base = unsafe { (*dpt).iomem.cast::<u64>() };
    // Note that PTE_READ_ONLY is ignored here, as in the source: the caller
    // must not let users override access to a read-only page.
    let pte_encode = unsafe { (*vm).pte_encode.unwrap()(0, pat_index, flags) };

    let mut i = unsafe { (*vma_res).start } / I915_GTT_PAGE_SIZE;
    let mut iter = unsafe { for_each_sgt_daddr((*vma_res).bi.pages) };
    while let Some(addr) = for_each_sgt_daddr_next(&mut iter) {
        unsafe { gen8_set_pte(base.add(i as usize), pte_encode | addr) };
        i += 1;
    }
}

// upstream: i915_dpt.c dpt_clear_range()
unsafe extern "C" fn dpt_clear_range(_vm: *mut I915AddressSpace, _start: u64, _length: u64) {}

// upstream: i915_dpt.c dpt_bind_vma()
unsafe extern "C" fn dpt_bind_vma(
    vm: *mut I915AddressSpace,
    _stash: *mut I915VmPtStash,
    vma_res: *mut I915VmaResource,
    pat_index: u32,
    _flags: u32,
) {
    unsafe {
        if (*vma_res).bound_flags != 0 {
            return;
        }

        // Applicable to VLV (gen8+ do not support RO in the GGTT).
        let mut pte_flags: u32 = 0;
        // bi.readonly is bit 0 of bi.flags, bi.lmem is bit 1.
        if (*vm).vm_flags & VM_FLAG_HAS_READ_ONLY != 0 && (*vma_res).bi.flags & 1 != 0 {
            pte_flags |= PTE_READ_ONLY;
        }
        if (*vma_res).bi.flags & 2 != 0 {
            pte_flags |= PTE_LM;
        }

        ((*vm).insert_entries.unwrap())(vm, vma_res, pat_index, pte_flags);

        (*vma_res).page_sizes_gtt = I915_GTT_PAGE_SIZE as u32;

        // Without aliasing PPGTT there's no difference between
        // GLOBAL/LOCAL_BIND, so upgrade to both bound if we bind either.
        (*vma_res).bound_flags = (I915_VMA_GLOBAL_BIND | I915_VMA_LOCAL_BIND) as u32;
    }
}

// upstream: i915_dpt.c dpt_unbind_vma()
unsafe extern "C" fn dpt_unbind_vma(vm: *mut I915AddressSpace, vma_res: *mut I915VmaResource) {
    unsafe {
        ((*vm).clear_range.unwrap())(vm, (*vma_res).start, (*vma_res).vma_size);
    }
}

// upstream: i915_dpt.c dpt_cleanup()
unsafe extern "C" fn dpt_cleanup(vm: *mut I915AddressSpace) {
    unsafe {
        let dpt = i915_vm_to_dpt(vm);
        i915_gem_object_put((*dpt).obj);
    }
}

// upstream: i915_dpt.c i915_dpt_pin_to_ggtt()
pub unsafe fn i915_dpt_pin_to_ggtt(dpt: *mut IntelDpt, alignment: u32) -> *mut I915Vma {
    unsafe {
        let i915: *mut DrmI915Private = (*dpt).vm.i915;
        let mut pin_flags: u64 = 0;

        if i915_gem_object_is_stolen((*dpt).obj) {
            pin_flags |= PIN_MAPPABLE;
        }

        let wakeref = intel_runtime_pm_get(ptr::addr_of_mut!((*i915).runtime_pm).cast());
        let pending = i915.cast::<u8>().add(PENDING_FB_PIN_OFFSET).cast::<crate::intel_engine_cs_upstream::AtomicT>();
        atomic_inc(&mut *pending);

        // for_i915_gem_ww(&ww, err, true)
        let mut ww = I915GemWwCtx::default();
        let mut err: c_int = -EDEADLK;
        let mut vma: *mut I915Vma = ptr::null_mut();
        i915_gem_ww_ctx_init(&mut ww, true);
        while err == -EDEADLK {
            err = i915_gem_object_lock((*dpt).obj, &mut ww);
            if err == 0 {
                vma = i915_gem_object_ggtt_pin_ww(
                    (*dpt).obj,
                    &mut ww,
                    ptr::null(),
                    0,
                    alignment as u64,
                    pin_flags,
                );
                if IS_ERR(vma) {
                    err = PTR_ERR(vma);
                } else {
                    let iomem = i915_vma_pin_iomap(vma);
                    i915_vma_unpin(vma);

                    if IS_ERR(iomem) {
                        err = PTR_ERR(iomem);
                    } else {
                        (*dpt).vma = vma;
                        (*dpt).iomem = iomem;

                        i915_vma_get(vma);
                    }
                }
            }
            // `continue` in the C body runs this update expression.
            err = __i915_gem_ww_fini(&mut ww, err);
        }

        (*(*dpt).obj).mm.madv_dirty_bits |= OBJ_MM_DIRTY;

        atomic_dec(&mut *pending);
        intel_runtime_pm_put(ptr::addr_of_mut!((*i915).runtime_pm).cast(), wakeref);

        if err != 0 { ERR_PTR(err) } else { vma }
    }
}

// upstream: i915_dpt.c i915_dpt_unpin_from_ggtt()
pub unsafe fn i915_dpt_unpin_from_ggtt(dpt: *mut IntelDpt) {
    unsafe {
        i915_vma_unpin_iomap((*dpt).vma);
        i915_vma_put((*dpt).vma);
    }
}

// upstream: i915_dpt.c i915_dpt_create()
unsafe extern "C" fn i915_dpt_create(obj: *mut c_void, size: usize) -> *mut c_void {
    unsafe {
        let obj = obj.cast::<DrmGemObject>();
        let i915 = to_i915((*obj).dev.cast());
        let mut size = size as u64;
        let mut dpt_obj: *mut DrmI915GemObject;

        if size == 0 {
            let page = I915_GTT_PAGE_SIZE;
            size = ((*obj).size as u64 + page - 1) / page;
        }

        size = (size * GEN8_PTE_SIZE + I915_GTT_PAGE_SIZE - 1) & !(I915_GTT_PAGE_SIZE - 1);

        dpt_obj = i915_gem_object_create_lmem(i915, size, I915_BO_ALLOC_CONTIGUOUS);
        if IS_ERR(dpt_obj) && i915_ggtt_has_aperture((*to_gt(i915)).ggtt as *const I915Ggtt) {
            dpt_obj = i915_gem_object_create_stolen(i915, size);
        }
        if IS_ERR(dpt_obj) && !HAS_LMEM(i915) {
            drm_dbg!(&(*i915).drm, "Allocating dpt from smem\n");
            dpt_obj = i915_gem_object_create_shmem(i915, size);
        }
        if IS_ERR(dpt_obj) {
            return dpt_obj.cast();
        }

        let mut ret = i915_gem_object_lock_interruptible(dpt_obj, ptr::null_mut());
        if ret == 0 {
            ret = i915_gem_object_set_cache_level(dpt_obj, I915CacheLevel::None);
            i915_gem_object_unlock(dpt_obj);
        }
        if ret != 0 {
            i915_gem_object_put(dpt_obj);
            return ERR_PTR(ret);
        }

        let dpt = kzalloc_obj::<IntelDpt>();
        if dpt.is_null() {
            i915_gem_object_put(dpt_obj);
            return ERR_PTR(-ENOMEM);
        }

        let vm = ptr::addr_of_mut!((*dpt).vm);
        (*vm).gt = to_gt(i915);
        (*vm).i915 = i915;
        (*vm).dma = (*i915).drm.dev;
        (*vm).total = (size / GEN8_PTE_SIZE) * I915_GTT_PAGE_SIZE;
        (*vm).vm_flags |= VM_FLAG_IS_DPT;

        i915_address_space_init(vm, VM_CLASS_DPT as c_int);

        (*vm).insert_page = Some(dpt_insert_page);
        (*vm).clear_range = Some(dpt_clear_range);
        (*vm).insert_entries = Some(dpt_insert_entries);
        (*vm).cleanup = Some(dpt_cleanup);

        (*vm).vma_ops.bind_vma = Some(dpt_bind_vma);
        (*vm).vma_ops.unbind_vma = Some(dpt_unbind_vma);

        (*vm).pte_encode = (*(*(*vm).gt).ggtt).vm.pte_encode;

        (*dpt).obj = dpt_obj;
        (*dpt_obj).cache_state_bits |= OBJ_CACHE_IS_DPT;

        dpt.cast()
    }
}

// upstream: i915_dpt.c i915_dpt_destroy()
unsafe extern "C" fn i915_dpt_destroy(dpt: *mut c_void) {
    unsafe {
        let dpt = dpt.cast::<IntelDpt>();
        (*(*dpt).obj).cache_state_bits &= !OBJ_CACHE_IS_DPT;
        i915_vm_put(ptr::addr_of_mut!((*dpt).vm));
    }
}

// upstream: i915_dpt.c i915_dpt_suspend()
unsafe extern "C" fn i915_dpt_suspend(dpt: *mut c_void) {
    unsafe { i915_ggtt_suspend_vm(ptr::addr_of_mut!((*dpt.cast::<IntelDpt>()).vm), true) };
}

// upstream: i915_dpt.c i915_dpt_resume()
unsafe extern "C" fn i915_dpt_resume(dpt: *mut c_void) {
    // The C helper discards the bool result of i915_ggtt_resume_vm().
    let _ = unsafe { i915_ggtt_resume_vm(ptr::addr_of_mut!((*dpt.cast::<IntelDpt>()).vm), true) };
}

// upstream: i915_dpt.c i915_dpt_offset()
pub unsafe fn i915_dpt_offset(dpt_vma: *mut I915Vma) -> u64 {
    unsafe { i915_vma_offset(dpt_vma) }
}

// upstream: i915_dpt.c i915_display_dpt_interface
#[unsafe(no_mangle)]
pub static i915_display_dpt_interface: IntelDisplayDptInterface = IntelDisplayDptInterface {
    create: Some(i915_dpt_create),
    destroy: Some(i915_dpt_destroy),
    suspend: Some(i915_dpt_suspend),
    resume: Some(i915_dpt_resume),
};
