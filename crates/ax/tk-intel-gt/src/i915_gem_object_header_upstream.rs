// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
// Source-order transcription of the active inline functions in Linux 7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_object.h. The target config has
// CONFIG_LOCKDEP=n and CONFIG_MMU_NOTIFIER=y; inactive preprocessor branches
// are omitted. The bodies use canonical LinuxKPI and upstream GEM/page APIs.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::offset_of,
};

use crate::{
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_BO_ALLOC_CONTIGUOUS, I915_BO_ALLOC_VOLATILE, I915_BO_PROTECTED,
        I915_BO_READONLY, I915_GEM_OBJECT_IS_PROXY, I915_GEM_OBJECT_IS_SHRINKABLE,
        I915_GEM_OBJECT_NO_MMAP, I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST, I915_TILING_QUIRK_BIT,
        STRIDE_MASK, TILING_MASK,
    },
    i915_gem_tiling_upstream::{I915_TILING_NONE, I915_TILING_Y},
    i915_gem_ww_upstream::I915GemWwCtx,
    intel_context_upstream::{Kref, RadixTreeRoot},
    intel_engine_cs_upstream::ListHead,
    linux::{bits, gem, memory, rcu},
};

#[repr(C)]
struct Idr {
    idr_rt: RadixTreeRoot,
    idr_base: u32,
    idr_next: u32,
}

/// The Linux 7.2.3 `drm_file` prefix through `object_idr` (`drm_file.h`).
#[repr(C)]
pub struct DrmFileObjectLookup {
    _prefix: [u8; 80],
    object_idr: Idr,
}

const _: [(); 80] = [(); offset_of!(DrmFileObjectLookup, object_idr)];
const _: [(); 24] = [(); core::mem::size_of::<Idr>()];

unsafe extern "C" {
    fn idr_find(idr: *const Idr, id: c_ulong) -> *mut c_void;
    fn drm_gem_object_free(refcount: *mut Kref);
}

#[inline]
unsafe fn object_base(obj: *mut DrmI915GemObject) -> *mut gem::DrmGemObject {
    unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) }
}
#[inline]
unsafe fn object_base_const(obj: *const DrmI915GemObject) -> *const gem::DrmGemObject {
    unsafe { object_base(obj.cast_mut()) }
}

#[inline]
pub unsafe fn assert_object_held(_obj: *const DrmI915GemObject) {
    // CONFIG_LOCKDEP=n: dma_resv_assert_held() compiles away.
}

// upstream: i915_gem_object.h i915_gem_object_size_2big()
pub fn i915_gem_object_size_2big(size: u64) -> bool {
    size > usize::MAX as u64
}

// upstream: i915_gem_object.h i915_gem_object_lookup_rcu()
pub unsafe fn i915_gem_object_lookup_rcu(
    file: *mut DrmFileObjectLookup,
    handle: u32,
) -> *mut DrmI915GemObject {
    unsafe { idr_find(core::ptr::addr_of!((*file).object_idr), handle as c_ulong).cast() }
}

// upstream: i915_gem_object.h i915_gem_object_get_rcu()
pub unsafe fn i915_gem_object_get_rcu(obj: *mut DrmI915GemObject) -> *mut DrmI915GemObject {
    if obj.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        if memory::kref_get_unless_zero(&mut (*object_base(obj)).refcount) {
            obj
        } else {
            core::ptr::null_mut()
        }
    }
}

// upstream: i915_gem_object.h i915_gem_object_lookup()
pub unsafe fn i915_gem_object_lookup(
    file: *mut DrmFileObjectLookup,
    handle: u32,
) -> *mut DrmI915GemObject {
    rcu::rcu_read_lock();
    let obj = unsafe { i915_gem_object_lookup_rcu(file, handle) };
    let obj = unsafe { i915_gem_object_get_rcu(obj) };
    rcu::rcu_read_unlock();
    obj
}

// upstream: i915_gem_object.h i915_gem_object_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_get(obj: *mut DrmI915GemObject) -> *mut DrmI915GemObject {
    unsafe { memory::kref_get(core::ptr::addr_of_mut!((*object_base(obj)).refcount)) };
    obj
}

// upstream: i915_gem_object.h i915_gem_object_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_put(obj: *mut DrmI915GemObject) {
    unsafe {
        memory::kref_put(
            core::ptr::addr_of_mut!((*object_base(obj)).refcount),
            drm_gem_object_free,
        );
    }
}

// upstream: i915_gem_object.h assert_object_held_shared()
pub unsafe fn assert_object_held_shared(obj: *const DrmI915GemObject) {
    if crate::linux_config::CONFIG_LOCKDEP
        && memory::kref_read(unsafe { &(*object_base_const(obj)).refcount }) > 0
    {
        crate::linux::assertion::lockdep_assert_held(unsafe {
            &*(*object_base_const(obj)).resv.cast::<gem::DmaResv>()
        });
    }
}

// upstream: i915_gem_object.h __i915_gem_object_lock()
pub unsafe fn __i915_gem_object_lock(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    intr: bool,
) -> c_int {
    unsafe {
        let resv = (*object_base(obj)).resv;
        let ctx = if ww.is_null() {
            core::ptr::null_mut()
        } else {
            core::ptr::addr_of_mut!((*ww).ctx)
        };
        let mut ret = gem::dma_resv_lock(resv, ctx, intr, false);
        if ret == 0 && !ww.is_null() {
            let _ = i915_gem_object_get(obj);
            crate::linux_list::list_add_tail(
                core::ptr::addr_of_mut!((*obj).obj_link),
                core::ptr::addr_of_mut!((*ww).obj_list),
            );
        }
        if ret == -crate::linux_config::EALREADY {
            ret = 0;
        }
        if ret == -crate::linux_config::EDEADLK {
            let _ = i915_gem_object_get(obj);
            (*ww).contended = obj;
        }
        ret
    }
}

// upstream: i915_gem_object.h i915_gem_object_lock()
pub unsafe fn i915_gem_object_lock(obj: *mut DrmI915GemObject, ww: *mut I915GemWwCtx) -> c_int {
    unsafe { __i915_gem_object_lock(obj, ww, !ww.is_null() && (*ww).intr) }
}

// upstream: i915_gem_object.h i915_gem_object_lock_interruptible()
pub unsafe fn i915_gem_object_lock_interruptible(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
) -> c_int {
    crate::linux::assertion::warn_on(!ww.is_null() && unsafe { !(*ww).intr });
    unsafe { __i915_gem_object_lock(obj, ww, true) }
}

// upstream: i915_gem_object.h i915_gem_object_trylock()
pub unsafe fn i915_gem_object_trylock(obj: *mut DrmI915GemObject, ww: *mut I915GemWwCtx) -> bool {
    unsafe {
        let resv = (*object_base(obj)).resv;
        if ww.is_null() {
            gem::dma_resv_lock(resv, core::ptr::null_mut(), false, true) == 0
        } else {
            let resv = resv.cast::<gem::DmaResv>();
            crate::linux::ww_mutex::ww_mutex_trylock(
                core::ptr::addr_of_mut!((*resv).lock),
                core::ptr::addr_of_mut!((*ww).ctx),
            )
        }
    }
}

// upstream: i915_gem_object.h i915_gem_object_unlock()
pub unsafe fn i915_gem_object_unlock(obj: *mut DrmI915GemObject) {
    unsafe {
        if let Some(adjust_lru) = (*(*obj).ops).adjust_lru {
            adjust_lru(obj);
        }
        gem::dma_resv_unlock((*object_base(obj)).resv);
    }
}

// upstream: i915_gem_object.h i915_gem_object_set_readonly()
pub unsafe fn i915_gem_object_set_readonly(obj: *mut DrmI915GemObject) {
    unsafe {
        (*obj).flags |= I915_BO_READONLY;
    }
}
// upstream: i915_gem_object.h i915_gem_object_is_readonly()
pub unsafe fn i915_gem_object_is_readonly(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).flags & I915_BO_READONLY != 0 }
}
// upstream: i915_gem_object.h i915_gem_object_is_contiguous()
pub unsafe fn i915_gem_object_is_contiguous(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).flags & I915_BO_ALLOC_CONTIGUOUS != 0 }
}
// upstream: i915_gem_object.h i915_gem_object_is_volatile()
pub unsafe fn i915_gem_object_is_volatile(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).flags & I915_BO_ALLOC_VOLATILE != 0 }
}
// upstream: i915_gem_object.h i915_gem_object_set_volatile()
pub unsafe fn i915_gem_object_set_volatile(obj: *mut DrmI915GemObject) {
    unsafe {
        (*obj).flags |= I915_BO_ALLOC_VOLATILE;
    }
}
// upstream: i915_gem_object.h i915_gem_object_has_tiling_quirk()
pub unsafe fn i915_gem_object_has_tiling_quirk(obj: *mut DrmI915GemObject) -> bool {
    bits::test_bit(I915_TILING_QUIRK_BIT, unsafe { &(*obj).flags })
}
// upstream: i915_gem_object.h i915_gem_object_set_tiling_quirk()
pub unsafe fn i915_gem_object_set_tiling_quirk(obj: *mut DrmI915GemObject) {
    bits::set_bit(I915_TILING_QUIRK_BIT, unsafe { &mut (*obj).flags });
}
// upstream: i915_gem_object.h i915_gem_object_clear_tiling_quirk()
pub unsafe fn i915_gem_object_clear_tiling_quirk(obj: *mut DrmI915GemObject) {
    bits::clear_bit(I915_TILING_QUIRK_BIT, unsafe { &mut (*obj).flags });
}
// upstream: i915_gem_object.h i915_gem_object_is_protected()
pub unsafe fn i915_gem_object_is_protected(obj: *const DrmI915GemObject) -> bool {
    unsafe { (*obj).flags & I915_BO_PROTECTED != 0 }
}
// upstream: i915_gem_object.h i915_gem_object_type_has()
pub unsafe fn i915_gem_object_type_has(obj: *const DrmI915GemObject, flags: c_ulong) -> bool {
    unsafe { (*(*obj).ops).flags as c_ulong & flags != 0 }
}
// upstream: i915_gem_object.h i915_gem_object_is_shrinkable()
pub unsafe fn i915_gem_object_is_shrinkable(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_type_has(obj, I915_GEM_OBJECT_IS_SHRINKABLE as c_ulong) }
}
// upstream: i915_gem_object.h i915_gem_object_has_self_managed_shrink_list()
pub unsafe fn i915_gem_object_has_self_managed_shrink_list(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_type_has(obj, I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST as c_ulong) }
}
// upstream: i915_gem_object.h i915_gem_object_is_proxy()
pub unsafe fn i915_gem_object_is_proxy(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_type_has(obj, I915_GEM_OBJECT_IS_PROXY as c_ulong) }
}
// upstream: i915_gem_object.h i915_gem_object_never_mmap()
pub unsafe fn i915_gem_object_never_mmap(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_type_has(obj, I915_GEM_OBJECT_NO_MMAP as c_ulong) }
}
// upstream: i915_gem_object.h i915_gem_object_is_framebuffer()
pub unsafe fn i915_gem_object_is_framebuffer(obj: *const DrmI915GemObject) -> bool {
    unsafe {
        !core::ptr::read_volatile(core::ptr::addr_of!((*obj).frontbuffer)).is_null()
            || (*obj).cache_state_bits & (1 << 10) != 0
    }
}
// upstream: i915_gem_object.h i915_gem_object_get_tiling()
pub unsafe fn i915_gem_object_get_tiling(obj: *const DrmI915GemObject) -> u32 {
    unsafe { (*obj).tiling_and_stride & TILING_MASK as u32 }
}
// upstream: i915_gem_object.h i915_gem_object_is_tiled()
pub unsafe fn i915_gem_object_is_tiled(obj: *const DrmI915GemObject) -> bool {
    unsafe { i915_gem_object_get_tiling(obj) != I915_TILING_NONE }
}
// upstream: i915_gem_object.h i915_gem_object_get_stride()
pub unsafe fn i915_gem_object_get_stride(obj: *const DrmI915GemObject) -> u32 {
    unsafe { (*obj).tiling_and_stride & STRIDE_MASK as u32 }
}
// upstream: i915_gem_object.h i915_gem_tile_height()
pub fn i915_gem_tile_height(tiling: u32) -> u32 {
    GEM_BUG_ON!(tiling == 0);
    if tiling == I915_TILING_Y { 32 } else { 8 }
}
// upstream: i915_gem_object.h i915_gem_object_get_tile_height()
pub unsafe fn i915_gem_object_get_tile_height(obj: *const DrmI915GemObject) -> u32 {
    i915_gem_tile_height(unsafe { i915_gem_object_get_tiling(obj) })
}
// upstream: i915_gem_object.h i915_gem_object_get_tile_row_size()
pub unsafe fn i915_gem_object_get_tile_row_size(obj: *const DrmI915GemObject) -> u32 {
    unsafe { i915_gem_object_get_stride(obj).wrapping_mul(i915_gem_object_get_tile_height(obj)) }
}

// upstream: i915_gem_object.h __i915_gem_object_get_sg()
pub unsafe fn __i915_gem_object_get_sg(
    obj: *mut DrmI915GemObject,
    n: u64,
    offset: *mut u32,
) -> *mut crate::i915_gem_pages_upstream::Scatterlist {
    unsafe {
        crate::i915_gem_pages_upstream::__i915_gem_object_page_iter_get_sg(
            obj,
            core::ptr::addr_of_mut!((*obj).mm.get_page),
            n,
            offset,
        )
    }
}
// upstream: i915_gem_object.h __i915_gem_object_get_sg_dma()
pub unsafe fn __i915_gem_object_get_sg_dma(
    obj: *mut DrmI915GemObject,
    n: u64,
    offset: *mut u32,
) -> *mut crate::i915_gem_pages_upstream::Scatterlist {
    unsafe {
        crate::i915_gem_pages_upstream::__i915_gem_object_page_iter_get_sg(
            obj,
            core::ptr::addr_of_mut!((*obj).mm.get_dma_page),
            n,
            offset,
        )
    }
}

// upstream: i915_gem_object.h i915_gem_object_pin_pages()
pub unsafe fn i915_gem_object_pin_pages(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        assert_object_held(obj);
        if memory::atomic_inc_not_zero(&mut (*obj).mm.pages_pin_count) {
            return 0;
        }
        crate::i915_gem_pages_upstream::__i915_gem_object_get_pages(obj)
    }
}
// upstream: i915_gem_object.h i915_gem_object_has_pages()
pub unsafe fn i915_gem_object_has_pages(obj: *mut DrmI915GemObject) -> bool {
    unsafe {
        let pages = core::ptr::read_volatile(core::ptr::addr_of!((*obj).mm.pages));
        !pages.is_null() && !crate::linux_config::IS_ERR(pages)
    }
}
// upstream: i915_gem_object.h __i915_gem_object_pin_pages()
pub unsafe fn __i915_gem_object_pin_pages(obj: *mut DrmI915GemObject) {
    unsafe {
        GEM_BUG_ON!(!i915_gem_object_has_pages(obj));
        memory::atomic_inc(&mut (*obj).mm.pages_pin_count);
    }
}
// upstream: i915_gem_object.h i915_gem_object_has_pinned_pages()
pub unsafe fn i915_gem_object_has_pinned_pages(obj: *mut DrmI915GemObject) -> bool {
    memory::atomic_read(unsafe { &(*obj).mm.pages_pin_count }) != 0
}
// upstream: i915_gem_object.h __i915_gem_object_unpin_pages()
pub unsafe fn __i915_gem_object_unpin_pages(obj: *mut DrmI915GemObject) {
    unsafe {
        GEM_BUG_ON!(!i915_gem_object_has_pages(obj));
        GEM_BUG_ON!(!i915_gem_object_has_pinned_pages(obj));
        memory::atomic_dec(&mut (*obj).mm.pages_pin_count);
    }
}
// upstream: i915_gem_object.h i915_gem_object_unpin_pages()
pub unsafe fn i915_gem_object_unpin_pages(obj: *mut DrmI915GemObject) {
    unsafe { __i915_gem_object_unpin_pages(obj) }
}
// upstream: i915_gem_object.h i915_gem_object_flush_map()
pub unsafe fn i915_gem_object_flush_map(obj: *mut DrmI915GemObject) {
    unsafe {
        crate::i915_gem_pages_upstream::__i915_gem_object_flush_map(
            obj,
            0,
            (*object_base(obj)).size,
        )
    }
}
// upstream: i915_gem_object.h i915_gem_object_unpin_map()
pub unsafe fn i915_gem_object_unpin_map(obj: *mut DrmI915GemObject) {
    unsafe { i915_gem_object_unpin_pages(obj) }
}
// upstream: i915_gem_object.h i915_gem_object_finish_access()
pub unsafe fn i915_gem_object_finish_access(obj: *mut DrmI915GemObject) {
    unsafe { i915_gem_object_unpin_pages(obj) }
}
// upstream: i915_gem_object.h __start_cpu_write()
pub unsafe fn __start_cpu_write(obj: *mut DrmI915GemObject) {
    const I915_GEM_DOMAIN_CPU: u16 = 1;
    unsafe {
        (*obj).read_domains = I915_GEM_DOMAIN_CPU;
        (*obj).write_domain = I915_GEM_DOMAIN_CPU;
        if crate::i915_gem_domain_upstream::i915_gem_cpu_write_needs_clflush(obj) {
            (*obj).cache_state_bits |= 1 << 9;
        }
    }
}
// upstream: i915_gem_object.h i915_gem_object_is_userptr()
pub unsafe fn i915_gem_object_is_userptr(obj: *mut DrmI915GemObject) -> bool {
    unsafe { !(&(*obj).backing.userptr).notifier.mm.is_null() }
}
