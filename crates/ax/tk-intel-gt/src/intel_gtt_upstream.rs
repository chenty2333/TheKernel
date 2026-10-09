// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gtt.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::offset_of,
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    i915_drm_client_upstream::i915_drm_client_add_object,
    i915_gem_context_types_upstream::I915DrmClient,
    i915_gem_lmem_upstream::__i915_gem_object_create_lmem_with_ps,
    i915_gem_object_api_upstream::{
        i915_gem_object_get_rcu, i915_gem_object_lock, i915_gem_object_put,
    },
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_MAP_WC, I915CacheLevel, I915MapType, Page,
    },
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_pages_upstream::{
        Scatterlist, i915_gem_object_pin_map, i915_gem_object_pin_map_unlocked, page_mask_bits,
        sg_page,
    },
    i915_gem_shrinker_upstream::{
        i915_gem_object_make_unshrinkable, i915_gem_shrink_all, i915_gem_shrinker_taints_mutex,
    },
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_utils_upstream::i915_vtd_active,
    i915_vma_api_upstream::{
        __i915_vma_unbind, i915_vma_destroy_locked, i915_vma_instance, i915_vma_pin, i915_vma_put,
    },
    i915_vma_types_upstream::{I915_VMA_PIN_MASK, I915Vma},
    intel_context_upstream::{DrmGemObjectBaseLayout, I915GttView, Kref},
    intel_engine_cs_upstream::WorkStruct,
    intel_gt_api_upstream::intel_gt_coherent_map_type,
    intel_gt_mcr_upstream::{
        intel_gt_mcr_lock, intel_gt_mcr_multicast_write, intel_gt_mcr_multicast_write_fw,
        intel_gt_mcr_unlock,
    },
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_gtt_api_upstream::{
        GEN8_PPAT_AGE, I915_GTT_MIN_ALIGNMENT, I915_GTT_PAGE_SIZE_2M, I915_GTT_PAGE_SIZE_4K,
        I915_GTT_PAGE_SIZE_64K, I915AddressSpace, i915_is_ggtt, i915_vm_is_4lvl, i915_vm_resv_get,
        i915_vm_resv_put, i915_vm_to_ppgtt,
    },
    intel_ring_upstream::i915_gem_object_create_internal,
    intel_uncore_types_upstream::{
        FW_REG_WRITE, IntelUncore, intel_uncore_forcewake_for_reg, intel_uncore_forcewake_get,
        intel_uncore_forcewake_put, intel_uncore_read, intel_uncore_rmw, intel_uncore_write,
    },
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT},
    linux::{
        gem_memory::DrmMm,
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, IP_VER, IS_BROADWELL, IS_CHERRYVIEW, IS_GEN9_LP,
            IS_GRAPHICS_VER, IS_METEORLAKE, MEDIA_VER_FULL,
        },
        memory::{kfree, kref_init, kref_put, kref_read, memset},
        mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_unlock},
        workqueue::{INIT_WORK_C, queue_work},
    },
    linux_config::{
        CONFIG_DRM_I915_DEBUG_GEM, ENOMEM, INTEL_MEMORY_LOCAL, INTEL_MEMORY_STOLEN_LOCAL,
        PAGE_SHIFT, PAGE_SIZE,
    },
    linux_i915_private::DrmI915Private,
    linux_list::{INIT_LIST_HEAD, list_del_init, list_empty},
};

const I915_COLOR_UNEVICTABLE: c_ulong = c_ulong::MAX;
const POISON_FREE: u8 = 0x6b;
const GEN8_L3_LRA_1_GPGPU_DEFAULT_VALUE_BDW: u32 = 0x67f1_427f;
const GEN8_L3_LRA_1_GPGPU_DEFAULT_VALUE_CHV: u32 = 0x5ff1_01ff;
const GEN9_L3_LRA_1_GPGPU_DEFAULT_VALUE_BXT: u32 = 0x5ff1_01ff;
const GEN9_L3_LRA_1_GPGPU_DEFAULT_VALUE_SKL: u32 = 0x67f1_427f;
const GAMW_ECO_ENABLE_64K_IPS_FIELD: u32 = 0x0f;
const GTT_CACHE_EN_ALL: u32 = 0xf000_7fff;
const GEN8_PRIVATE_PAT_LO: I915RegT = I915RegT { reg: 0x40e0 };
const GEN8_PRIVATE_PAT_HI: I915RegT = I915RegT { reg: 0x40e4 };
const GEN8_GAMW_ECO_DEV_RW_IA: I915RegT = I915RegT { reg: 0x4080 };
const GEN8_L3_LRA_1_GPGPU: I915RegT = I915RegT { reg: 0x4dd4 };
const HSW_GTT_CACHE_EN: I915RegT = I915RegT { reg: 0x4024 };
const GEN8_PPAT_WB: u32 = 3;
const GEN8_PPAT_WC: u32 = 1;
const GEN8_PPAT_WT: u32 = 2;
const GEN8_PPAT_UC: u32 = 0;
const GEN8_PPAT_LLC: u32 = 1 << 2;
const GEN8_PPAT_LLCELLC: u32 = 2 << 2;
const GEN8_PPAT_ELLC_OVERRIDE: u32 = 0;
const CHV_PPAT_SNOOP: u32 = 1 << 6;

// `struct drm_i915_file_private` is opaque in its canonical type owner. Its
// `client` field is at byte 112 in Linux 7.2.3 on x86_64; this view matches
// the independently asserted source view in i915_drm_client_upstream.rs.
#[repr(C)]
struct DrmI915FilePrivateClientView {
    _prefix: [u8; 112],
    client: *mut I915DrmClient,
}
const _: [(); 112] = [(); offset_of!(DrmI915FilePrivateClientView, client)];

unsafe extern "C" {
    // These are real out-of-line kernel/i915 services, not local stand-ins.
    // Their LinuxKPI/source owners are tracked as integration dependencies.
    fn drm_mm_init(mm: *mut DrmMm, start: u64, size: u64);
    fn drm_mm_takedown(mm: *mut DrmMm);
    fn i915_direct_stolen_access(i915: *mut DrmI915Private) -> bool;
    fn i915_vma_resource_bind_dep_sync_all(vm: *mut I915AddressSpace);
}

#[inline]
unsafe fn gem_base(obj: *mut DrmI915GemObject) -> *mut DrmGemObjectBaseLayout {
    unsafe { obj.cast::<DrmGemObjectBaseLayout>() }
}

#[inline]
unsafe fn vm_has_page_sizes(i915: *mut DrmI915Private, sizes: u64) -> bool {
    GEM_BUG_ON!(sizes == 0);
    let info = unsafe { crate::linux::i915::INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).runtime.page_sizes & sizes as u32 == sizes as u32 }
}

#[inline]
unsafe fn has_64k_pages(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { crate::linux::i915::INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[0] & (1 << 4) != 0 }
}

#[inline]
const fn gen8_ppat(index: u32, value: u32) -> u64 {
    (value as u64) << (index * 8)
}

#[inline]
fn get_order(size: u64) -> u8 {
    assert!(size.is_power_of_two() && size >= PAGE_SIZE as u64);
    (size.trailing_zeros() - PAGE_SHIFT as u32) as u8
}

// upstream: intel_gtt.c i915_ggtt_require_binder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_require_binder(i915: *mut DrmI915Private) -> bool {
    !unsafe { i915_direct_stolen_access(i915) } && unsafe { MEDIA_VER_FULL(i915) == IP_VER(13, 0) }
}

// upstream: intel_gtt.c intel_ggtt_update_needs_vtd_wa()
unsafe fn intel_ggtt_update_needs_vtd_wa(i915: *mut DrmI915Private) -> bool {
    unsafe { crate::linux::i915::IS_BROXTON(i915) && i915_vtd_active(i915) }
}

// upstream: intel_gtt.c intel_vm_no_concurrent_access_wa()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_vm_no_concurrent_access_wa(i915: *mut DrmI915Private) -> bool {
    unsafe { IS_CHERRYVIEW(i915) || intel_ggtt_update_needs_vtd_wa(i915) }
}

// upstream: intel_gtt.c alloc_pt_lmem()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_pt_lmem(
    vm: *mut I915AddressSpace,
    size: c_int,
) -> *mut DrmI915GemObject {
    let obj = unsafe {
        __i915_gem_object_create_lmem_with_ps(
            (*vm).i915,
            size as u64,
            size as u64,
            (*vm).lmem_pt_obj_flags as u32,
        )
    };
    if !crate::linux_config::IS_ERR(obj) {
        let resv = unsafe { i915_vm_resv_get(vm) };
        unsafe { (*gem_base(obj)).resv = resv };
        unsafe { (*obj).shares_resv_from = vm };
        let fpriv = unsafe { (*vm).fpriv };
        if !fpriv.is_null() {
            let client = unsafe { (*fpriv.cast::<DrmI915FilePrivateClientView>()).client };
            unsafe { i915_drm_client_add_object(client, obj) };
        }
    }
    obj
}

// upstream: intel_gtt.c alloc_pt_dma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_pt_dma(
    vm: *mut I915AddressSpace,
    size: c_int,
) -> *mut DrmI915GemObject {
    if I915_SELFTEST_ONLY!({ crate::i915_selftest_upstream::should_fail(&(*vm).fault_attr, 1) }) {
        unsafe { i915_gem_shrink_all((*vm).i915) };
    }
    let obj = unsafe { i915_gem_object_create_internal((*vm).i915, size as u64) };
    if !crate::linux_config::IS_ERR(obj) {
        let resv = unsafe { i915_vm_resv_get(vm) };
        unsafe { (*gem_base(obj)).resv = resv };
        unsafe { (*obj).shares_resv_from = vm };
        let fpriv = unsafe { (*vm).fpriv };
        if !fpriv.is_null() {
            let client = unsafe { (*fpriv.cast::<DrmI915FilePrivateClientView>()).client };
            unsafe { i915_drm_client_add_object(client, obj) };
        }
    }
    obj
}

// upstream: intel_gtt.c map_pt_dma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn map_pt_dma(
    vm: *mut I915AddressSpace,
    obj: *mut DrmI915GemObject,
) -> c_int {
    let mut map_type: I915MapType = unsafe { intel_gt_coherent_map_type((*vm).gt, obj, true) };
    if unsafe { IS_METEORLAKE((*vm).i915) } {
        map_type = I915_MAP_WC;
    }
    let vaddr = unsafe { i915_gem_object_pin_map_unlocked(obj, map_type) };
    if crate::linux_config::IS_ERR(vaddr) {
        return crate::linux_config::PTR_ERR(vaddr);
    }
    unsafe { i915_gem_object_make_unshrinkable(obj) };
    0
}

// upstream: intel_gtt.c map_pt_dma_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn map_pt_dma_locked(
    vm: *mut I915AddressSpace,
    obj: *mut DrmI915GemObject,
) -> c_int {
    let mut map_type: I915MapType = unsafe { intel_gt_coherent_map_type((*vm).gt, obj, true) };
    if unsafe { IS_METEORLAKE((*vm).i915) } {
        map_type = I915_MAP_WC;
    }
    let vaddr = unsafe { i915_gem_object_pin_map(obj, map_type) };
    if crate::linux_config::IS_ERR(vaddr) {
        return crate::linux_config::PTR_ERR(vaddr);
    }
    unsafe { i915_gem_object_make_unshrinkable(obj) };
    0
}

// upstream: intel_gtt.c clear_vm_list()
unsafe fn clear_vm_list(head: *mut crate::intel_engine_cs_upstream::ListHead) {
    let mut vma: *mut I915Vma = ptr::null_mut();
    let mut next: *mut I915Vma = ptr::null_mut();
    list_for_each_entry_safe!(vma, next, head, vm_link, {
        let obj = unsafe { (*vma).obj };
        let got = unsafe { i915_gem_object_get_rcu(obj) };
        if got.is_null() {
            // The object is dying but has not yet cleared its VMA list. Drain
            // this entry, leaving destruction to the object's destructor.
            let flags = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*vma).flags.counter)) };
            flags.fetch_and(!I915_VMA_PIN_MASK, Ordering::Relaxed);
            WARN_ON!(unsafe { __i915_vma_unbind(vma) } != 0);
            unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
            unsafe { i915_vm_resv_get((*vma).vm) };
            unsafe { (*vma).vm_ddestroy = true };
        } else {
            unsafe { i915_vma_destroy_locked(vma) };
            unsafe { i915_gem_object_put(obj) };
        }
    });
}

// upstream: intel_gtt.c __i915_vm_close()
unsafe fn __i915_vm_close(vm: *mut I915AddressSpace) {
    unsafe { mutex_lock(ptr::addr_of_mut!((*vm).mutex)) };
    unsafe { clear_vm_list(ptr::addr_of_mut!((*vm).bound_list)) };
    unsafe { clear_vm_list(ptr::addr_of_mut!((*vm).unbound_list)) };
    GEM_BUG_ON!(
        !list_empty(unsafe { &(*vm).bound_list }) || !list_empty(unsafe { &(*vm).unbound_list })
    );
    unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
}

// upstream: intel_gtt.c i915_vm_lock_objects()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_lock_objects(
    vm: *mut I915AddressSpace,
    ww: *mut I915GemWwCtx,
) -> c_int {
    let scratch = unsafe { (*vm).scratch[0] };
    if unsafe { (*gem_base(scratch)).resv == ptr::addr_of_mut!((*vm)._resv).cast() } {
        unsafe { i915_gem_object_lock(scratch, ww) }
    } else {
        let ppgtt = unsafe { i915_vm_to_ppgtt(vm) };
        let obj = unsafe { (*(*ppgtt).pd).pt.base };
        unsafe { i915_gem_object_lock(obj, ww) }
    }
}

// upstream: intel_gtt.c i915_address_space_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_address_space_fini(vm: *mut I915AddressSpace) {
    unsafe { drm_mm_takedown(ptr::addr_of_mut!((*vm).mm)) };
}

// upstream: intel_gtt.c i915_vm_resv_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_resv_release(kref: *mut Kref) {
    let vm = unsafe {
        kref.cast::<u8>()
            .sub(offset_of!(I915AddressSpace, resv_ref))
            .cast::<I915AddressSpace>()
    };
    let resv = unsafe { ptr::addr_of_mut!((*vm)._resv).cast() };
    unsafe { crate::linux::gem::dma_resv_fini(resv) };
    unsafe { mutex_destroy(&mut (*vm).mutex) };
    unsafe { kfree(vm) };
}

// upstream: intel_gtt.c __i915_vm_release()
unsafe extern "C" fn __i915_vm_release(work: *mut WorkStruct) {
    let vm = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(I915AddressSpace, release_work))
            .cast::<I915AddressSpace>()
    };
    unsafe { __i915_vm_close(vm) };
    unsafe { i915_vma_resource_bind_dep_sync_all(vm) };
    let cleanup = unsafe {
        (*vm)
            .cleanup
            .expect("address-space cleanup callback missing")
    };
    unsafe { cleanup(vm) };
    unsafe { i915_address_space_fini(vm) };
    unsafe { i915_vm_resv_put(vm) };
}

// upstream: intel_gtt.c i915_vm_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vm_release(kref: *mut Kref) {
    let vm = kref.cast::<I915AddressSpace>(); // `ref` is the first member.
    GEM_BUG_ON!(unsafe { i915_is_ggtt(vm) });
    crate::linux::i915_trace::trace_i915_ppgtt_release(vm);
    let i915 = unsafe { (*vm).i915 };
    unsafe {
        queue_work((*i915).wq, ptr::addr_of_mut!((*vm).release_work));
    }
}

// upstream: intel_gtt.c i915_address_space_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_address_space_init(vm: *mut I915AddressSpace, subclass: c_int) {
    unsafe { kref_init(ptr::addr_of_mut!((*vm).r#ref)) };
    if unsafe { kref_read(&(*vm).resv_ref) } == 0 {
        unsafe { kref_init(ptr::addr_of_mut!((*vm).resv_ref)) };
    }
    unsafe { (*vm).pending_unbind = RB_ROOT_CACHED!() };
    unsafe { INIT_WORK_C(&mut (*vm).release_work, __i915_vm_release) };
    unsafe { mutex_init(ptr::addr_of_mut!((*vm).mutex)) };
    // lockdep_set_subclass() compiles out for this kernel's CONFIG_LOCKDEP=n.
    if !unsafe { intel_vm_no_concurrent_access_wa((*vm).i915) } {
        unsafe { i915_gem_shrinker_taints_mutex((*vm).i915, ptr::addr_of_mut!((*vm).mutex)) };
    } else {
        // mutex_acquire(), might_alloc(), and mutex_release() are annotations
        // compiled out for CONFIG_LOCKDEP=n.
    }
    unsafe {
        crate::i915_gem_clflush_upstream::dma_resv_init(ptr::addr_of_mut!((*vm)._resv).cast());
    }
    GEM_BUG_ON!(unsafe { (*vm).total == 0 });
    unsafe { drm_mm_init(ptr::addr_of_mut!((*vm).mm), 0, (*vm).total) };
    for alignment in unsafe { &mut (*vm).min_alignment } {
        *alignment = I915_GTT_MIN_ALIGNMENT;
    }
    if unsafe { has_64k_pages((*vm).i915) } {
        unsafe {
            (*vm).min_alignment[INTEL_MEMORY_LOCAL as usize] = I915_GTT_PAGE_SIZE_64K;
            (*vm).min_alignment[INTEL_MEMORY_STOLEN_LOCAL as usize] = I915_GTT_PAGE_SIZE_64K;
        }
    }
    unsafe { (*vm).mm.head_node.color = I915_COLOR_UNEVICTABLE };
    unsafe {
        INIT_LIST_HEAD(ptr::addr_of_mut!((*vm).bound_list));
        INIT_LIST_HEAD(ptr::addr_of_mut!((*vm).unbound_list));
    }
}

// upstream: intel_gtt.c __px_vaddr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __px_vaddr(obj: *mut DrmI915GemObject) -> *mut c_void {
    GEM_BUG_ON!(!unsafe { crate::i915_gem_object_api_upstream::i915_gem_object_has_pages(obj) });
    unsafe { page_mask_bits((*obj).mm.mapping) }
}

// upstream: intel_gtt.c __px_dma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __px_dma(obj: *mut DrmI915GemObject) -> u64 {
    GEM_BUG_ON!(!unsafe { crate::i915_gem_object_api_upstream::i915_gem_object_has_pages(obj) });
    let sgl = unsafe { (*(*obj).mm.pages).sgl };
    unsafe { (*sgl.cast::<Scatterlist>()).dma_address }
}

// upstream: intel_gtt.c __px_page()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __px_page(obj: *mut DrmI915GemObject) -> *mut Page {
    GEM_BUG_ON!(!unsafe { crate::i915_gem_object_api_upstream::i915_gem_object_has_pages(obj) });
    let sgl = unsafe { (*(*obj).mm.pages).sgl };
    unsafe { sg_page(sgl) }
}

// upstream: intel_gtt.c fill_page_dma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fill_page_dma(obj: *mut DrmI915GemObject, value: u64, count: u32) {
    let vaddr = unsafe { __px_vaddr(obj) }.cast::<u64>();
    let mut index = 0;
    while index < count as usize {
        unsafe { ptr::write(vaddr.add(index), value) };
        index += 1;
    }
    unsafe {
        crate::i915_gem_clflush_upstream::drm_clflush_virt_range(vaddr.cast(), PAGE_SIZE as u64)
    };
}

// upstream: intel_gtt.c poison_scratch_page()
unsafe fn poison_scratch_page(scratch: *mut DrmI915GemObject) {
    let vaddr = unsafe { __px_vaddr(scratch) };
    let value = if CONFIG_DRM_I915_DEBUG_GEM {
        POISON_FREE as i32
    } else {
        0
    };
    let size = unsafe { (*gem_base(scratch)).size } as usize;
    unsafe { memset(vaddr, value, size) };
    unsafe { crate::i915_gem_clflush_upstream::drm_clflush_virt_range(vaddr, size as u64) };
}

// upstream: intel_gtt.c setup_scratch_page()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setup_scratch_page(vm: *mut I915AddressSpace) -> c_int {
    let mut size = I915_GTT_PAGE_SIZE_4K;
    if unsafe { i915_vm_is_4lvl(vm) }
        && unsafe { vm_has_page_sizes((*vm).i915, I915_GTT_PAGE_SIZE_64K) }
        && !unsafe { has_64k_pages((*vm).i915) }
    {
        size = I915_GTT_PAGE_SIZE_64K;
    }
    loop {
        let obj = unsafe {
            (*vm)
                .alloc_scratch_dma
                .expect("scratch DMA allocator is not initialized")(vm, size as c_int)
        };
        if crate::linux_config::IS_ERR(obj) {
            // Keep Linux's shared skip path below.
        } else if unsafe { map_pt_dma(vm, obj) } != 0 {
            unsafe { i915_gem_object_put(obj) };
        } else if unsafe { (*obj).mm.page_sizes.sg } < size as u32 {
            unsafe { i915_gem_object_put(obj) };
        } else if unsafe { __px_dma(obj) } & (size - 1) != 0 {
            unsafe { i915_gem_object_put(obj) };
        } else {
            unsafe { poison_scratch_page(obj) };
            unsafe { (*vm).scratch[0] = obj };
            unsafe { (*vm).scratch_order = get_order(size) };
            return 0;
        }
        if size == I915_GTT_PAGE_SIZE_4K {
            return -ENOMEM;
        }
        size = I915_GTT_PAGE_SIZE_4K;
    }
}

// upstream: intel_gtt.c free_scratch()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free_scratch(vm: *mut I915AddressSpace) {
    if unsafe { (*vm).scratch[0].is_null() } {
        return;
    }
    let mut index = 0;
    while index <= unsafe { (*vm).top } {
        let scratch = unsafe { (*vm).scratch[index as usize] };
        unsafe { i915_gem_object_put(scratch) };
        index += 1;
    }
}

// upstream: intel_gtt.c gtt_write_workarounds()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_write_workarounds(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    if unsafe { IS_BROADWELL(i915) } {
        unsafe {
            intel_uncore_write(
                uncore,
                GEN8_L3_LRA_1_GPGPU,
                GEN8_L3_LRA_1_GPGPU_DEFAULT_VALUE_BDW,
            )
        };
    } else if unsafe { IS_CHERRYVIEW(i915) } {
        unsafe {
            intel_uncore_write(
                uncore,
                GEN8_L3_LRA_1_GPGPU,
                GEN8_L3_LRA_1_GPGPU_DEFAULT_VALUE_CHV,
            )
        };
    } else if unsafe { IS_GEN9_LP(i915) } {
        unsafe {
            intel_uncore_write(
                uncore,
                GEN8_L3_LRA_1_GPGPU,
                GEN9_L3_LRA_1_GPGPU_DEFAULT_VALUE_BXT,
            )
        };
    } else if unsafe { GRAPHICS_VER(i915) >= 9 && GRAPHICS_VER(i915) <= 11 } {
        unsafe {
            intel_uncore_write(
                uncore,
                GEN8_L3_LRA_1_GPGPU,
                GEN9_L3_LRA_1_GPGPU_DEFAULT_VALUE_SKL,
            )
        };
    }
    if unsafe { vm_has_page_sizes(i915, I915_GTT_PAGE_SIZE_64K) }
        && unsafe { GRAPHICS_VER(i915) <= 10 }
    {
        unsafe {
            intel_uncore_rmw(
                uncore,
                GEN8_GAMW_ECO_DEV_RW_IA,
                0,
                GAMW_ECO_ENABLE_64K_IPS_FIELD,
            )
        };
    }
    if unsafe { IS_GRAPHICS_VER(i915, 8, 11) } {
        let mut can_use_gtt_cache = true;
        if unsafe { vm_has_page_sizes(i915, I915_GTT_PAGE_SIZE_2M) } {
            can_use_gtt_cache = false;
        }
        unsafe {
            intel_uncore_write(
                uncore,
                HSW_GTT_CACHE_EN,
                if can_use_gtt_cache {
                    GTT_CACHE_EN_ALL
                } else {
                    0
                },
            );
        }
        gt_WARN_ON_ONCE!(
            gt,
            can_use_gtt_cache && unsafe { intel_uncore_read(uncore, HSW_GTT_CACHE_EN) == 0 }
        );
    }
}

// upstream: intel_gtt.c xelpmp_setup_private_ppat()
unsafe fn xelpmp_setup_private_ppat(uncore: *mut IntelUncore) {
    for (index, value) in [
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_1_WT,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_3_UC,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB
            | crate::intel_gtt_api_upstream::MTL_2_COH_1W,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB
            | crate::intel_gtt_api_upstream::MTL_3_COH_2W,
    ]
    .into_iter()
    .enumerate()
    {
        unsafe {
            intel_uncore_write(
                uncore,
                I915RegT {
                    reg: 0x4800 + index as u32 * 4,
                },
                value,
            );
        }
    }
}

// upstream: intel_gtt.c xelpg_setup_private_ppat()
unsafe fn xelpg_setup_private_ppat(gt: *mut IntelGt) {
    for (index, value) in [
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_1_WT,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_3_UC,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB
            | crate::intel_gtt_api_upstream::MTL_2_COH_1W,
        crate::intel_gtt_api_upstream::MTL_PPAT_L4_0_WB
            | crate::intel_gtt_api_upstream::MTL_3_COH_2W,
    ]
    .into_iter()
    .enumerate()
    {
        unsafe {
            intel_gt_mcr_multicast_write(
                gt,
                I915McrRegT {
                    reg: 0x4800 + index as u32 * 4,
                },
                value,
            );
        }
    }
}

// upstream: intel_gtt.c tgl_setup_private_ppat()
unsafe fn tgl_setup_private_ppat(uncore: *mut IntelUncore) {
    let values = [
        GEN8_PPAT_WB,
        GEN8_PPAT_WC,
        GEN8_PPAT_WT,
        GEN8_PPAT_UC,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
    ];
    for (index, value) in values.into_iter().enumerate() {
        unsafe {
            intel_uncore_write(
                uncore,
                I915RegT {
                    reg: 0x4800 + index as u32 * 4,
                },
                value,
            )
        };
    }
}

// upstream: intel_gtt.c xehp_setup_private_ppat()
unsafe fn xehp_setup_private_ppat(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    let fw =
        unsafe { intel_uncore_forcewake_for_reg(uncore, I915RegT { reg: 0x4800 }, FW_REG_WRITE) };
    unsafe { intel_uncore_forcewake_get(uncore, fw) };
    let mut flags = 0usize;
    unsafe { intel_gt_mcr_lock(gt, &mut flags) };
    let values = [
        GEN8_PPAT_WB,
        GEN8_PPAT_WC,
        GEN8_PPAT_WT,
        GEN8_PPAT_UC,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
        GEN8_PPAT_WB,
    ];
    for (index, value) in values.into_iter().enumerate() {
        unsafe {
            intel_gt_mcr_multicast_write_fw(
                gt,
                I915McrRegT {
                    reg: 0x4800 + index as u32 * 4,
                },
                value,
            );
        }
    }
    unsafe { intel_gt_mcr_unlock(gt, flags) };
    unsafe { intel_uncore_forcewake_put(uncore, fw) };
}

// upstream: intel_gtt.c icl_setup_private_ppat()
unsafe fn icl_setup_private_ppat(uncore: *mut IntelUncore) {
    let values = [
        GEN8_PPAT_WB | GEN8_PPAT_LLC,
        GEN8_PPAT_WC | GEN8_PPAT_LLCELLC,
        GEN8_PPAT_WB | GEN8_PPAT_ELLC_OVERRIDE,
        GEN8_PPAT_UC,
        GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(0),
        GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(1),
        GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(2),
        GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(3),
    ];
    for (index, value) in values.into_iter().enumerate() {
        unsafe {
            intel_uncore_write(
                uncore,
                I915RegT {
                    reg: 0x40e0 + index as u32 * 4,
                },
                value,
            )
        };
    }
}

// upstream: intel_gtt.c bdw_setup_private_ppat()
unsafe fn bdw_setup_private_ppat(uncore: *mut IntelUncore) {
    let i915 = unsafe { (*uncore).i915 };
    let mut pat = gen8_ppat(0, GEN8_PPAT_WB | GEN8_PPAT_LLC)
        | gen8_ppat(1, GEN8_PPAT_WC | GEN8_PPAT_LLCELLC)
        | gen8_ppat(3, GEN8_PPAT_UC)
        | gen8_ppat(4, GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(0))
        | gen8_ppat(5, GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(1))
        | gen8_ppat(6, GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(2))
        | gen8_ppat(7, GEN8_PPAT_WB | GEN8_PPAT_LLCELLC | GEN8_PPAT_AGE(3));
    if unsafe { GRAPHICS_VER(i915) >= 9 } {
        pat |= gen8_ppat(2, GEN8_PPAT_WB | GEN8_PPAT_ELLC_OVERRIDE);
    } else {
        pat |= gen8_ppat(2, GEN8_PPAT_WT | GEN8_PPAT_LLCELLC);
    }
    unsafe {
        intel_uncore_write(
            uncore,
            GEN8_PRIVATE_PAT_LO,
            crate::linux::bits::lower_32_bits(pat),
        );
        intel_uncore_write(
            uncore,
            GEN8_PRIVATE_PAT_HI,
            crate::linux::bits::upper_32_bits(pat),
        );
    }
}

// upstream: intel_gtt.c chv_setup_private_ppat()
unsafe fn chv_setup_private_ppat(uncore: *mut IntelUncore) {
    let pat = gen8_ppat(0, CHV_PPAT_SNOOP)
        | gen8_ppat(1, 0)
        | gen8_ppat(2, 0)
        | gen8_ppat(3, 0)
        | gen8_ppat(4, CHV_PPAT_SNOOP)
        | gen8_ppat(5, CHV_PPAT_SNOOP)
        | gen8_ppat(6, CHV_PPAT_SNOOP)
        | gen8_ppat(7, CHV_PPAT_SNOOP);
    unsafe {
        intel_uncore_write(
            uncore,
            GEN8_PRIVATE_PAT_LO,
            crate::linux::bits::lower_32_bits(pat),
        );
        intel_uncore_write(
            uncore,
            GEN8_PRIVATE_PAT_HI,
            crate::linux::bits::upper_32_bits(pat),
        );
    }
}

// upstream: intel_gtt.c setup_private_pat()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setup_private_pat(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    let i915 = unsafe { (*gt).i915 };
    GEM_BUG_ON!(unsafe { GRAPHICS_VER(i915) < 8 });
    if unsafe { (*gt).type_ == GT_MEDIA } {
        unsafe { xelpmp_setup_private_ppat(uncore) };
        return;
    }
    if unsafe { GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) } {
        unsafe { xelpg_setup_private_ppat(gt) };
    } else if unsafe { GRAPHICS_VER_FULL(i915) >= IP_VER(12, 55) } {
        unsafe { xehp_setup_private_ppat(gt) };
    } else if unsafe { GRAPHICS_VER(i915) >= 12 } {
        unsafe { tgl_setup_private_ppat(uncore) };
    } else if unsafe { GRAPHICS_VER(i915) >= 11 } {
        unsafe { icl_setup_private_ppat(uncore) };
    } else if unsafe { IS_CHERRYVIEW(i915) || IS_GEN9_LP(i915) } {
        unsafe { chv_setup_private_ppat(uncore) };
    } else {
        unsafe { bdw_setup_private_ppat(uncore) };
    }
}

// upstream: intel_gtt.c __vm_create_scratch_for_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vm_create_scratch_for_read(
    vm: *mut I915AddressSpace,
    size: c_ulong,
) -> *mut I915Vma {
    let aligned_size = (size as u64).wrapping_add(PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let obj = unsafe { i915_gem_object_create_internal((*vm).i915, aligned_size) };
    if crate::linux_config::IS_ERR(obj) {
        return obj.cast::<I915Vma>();
    }
    unsafe {
        i915_gem_object_set_cache_coherency(obj, I915CacheLevel::I915_CACHE_LLC as u32);
    }
    let vma = unsafe { i915_vma_instance(obj, vm, ptr::null::<I915GttView>()) };
    if crate::linux_config::IS_ERR(vma) {
        unsafe { i915_gem_object_put(obj) };
        return vma;
    }
    vma
}

// upstream: intel_gtt.c __vm_create_scratch_for_read_pinned()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vm_create_scratch_for_read_pinned(
    vm: *mut I915AddressSpace,
    size: c_ulong,
) -> *mut I915Vma {
    let vma = unsafe { __vm_create_scratch_for_read(vm, size) };
    if crate::linux_config::IS_ERR(vma) {
        return vma;
    }
    let flags = if unsafe { crate::i915_vma_api_upstream::i915_vma_is_ggtt(vma) } {
        crate::linux::registers::PIN_GLOBAL
    } else {
        crate::linux::registers::PIN_USER
    };
    let err = unsafe { i915_vma_pin(vma, 0, 0, flags) };
    if err != 0 {
        unsafe { i915_vma_put(vma) };
        return crate::linux_config::ERR_PTR(err);
    }
    vma
}
