// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
//
//! Selected source-order inline API from Linux v7.2.3
//! `drivers/gpu/drm/i915/gem/i915_gem_object.h` used by the integrated
//! userptr/shmem/domain paths. Field access is through the canonical GEM owner.
#![allow(unsafe_code)]

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_ww_upstream::I915GemWwCtx,
    intel_context_upstream::Kref,
    linux::memory::{
        atomic_dec, atomic_inc, atomic_inc_not_zero, atomic_read, kref_get, kref_get_unless_zero,
        kref_put,
    },
};

unsafe extern "C" {
    fn __i915_gem_object_get_pages(obj: *mut DrmI915GemObject) -> i32;
    fn drm_gem_object_free(refcount: *mut Kref);
}

#[inline]
unsafe fn assert_object_held(_obj: *const DrmI915GemObject) {
    // `dma_resv_assert_held()` expands to no operations for the selected
    // Linux configuration (CONFIG_LOCKDEP=n).
}

// upstream: i915_gem_object.h i915_gem_object_get_rcu()
#[inline]
pub unsafe fn i915_gem_object_get_rcu(obj: *mut DrmI915GemObject) -> *mut DrmI915GemObject {
    if obj.is_null() {
        return core::ptr::null_mut();
    }
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let acquired = unsafe { kref_get_unless_zero(&mut *core::ptr::addr_of_mut!((*base).refcount)) };
    if acquired { obj } else { core::ptr::null_mut() }
}

// upstream: i915_gem_object.h i915_gem_object_get()
#[inline]
pub unsafe fn i915_gem_object_get(obj: *mut DrmI915GemObject) -> *mut DrmI915GemObject {
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    unsafe { kref_get(core::ptr::addr_of_mut!((*base).refcount)) };
    obj
}

// upstream: i915_gem_object.h i915_gem_object_put()
#[inline]
pub unsafe fn i915_gem_object_put(obj: *mut DrmI915GemObject) {
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    unsafe {
        kref_put(
            core::ptr::addr_of_mut!((*base).refcount),
            drm_gem_object_free,
        )
    };
}

// upstream: i915_gem_object.h __i915_gem_object_lock()
#[inline]
pub unsafe fn __i915_gem_object_lock(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    intr: bool,
) -> i32 {
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let ctx = if ww.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { core::ptr::addr_of_mut!((*ww).ctx) }
    };
    let mut ret = unsafe { crate::linux::gem::dma_resv_lock((*base).resv, ctx, intr, false) };
    if ret == 0 && !ww.is_null() {
        unsafe { i915_gem_object_get(obj) };
        unsafe {
            crate::linux::list::list_add_tail(
                core::ptr::addr_of_mut!((*obj).obj_link),
                core::ptr::addr_of_mut!((*ww).obj_list),
            )
        };
    }
    if ret == -crate::linux_config::EALREADY {
        ret = 0;
    }
    if ret == -crate::linux_config::EDEADLK {
        unsafe { i915_gem_object_get(obj) };
        unsafe { (*ww).contended = obj };
    }
    ret
}

// upstream: i915_gem_object.h i915_gem_object_lock()
#[inline]
pub unsafe fn i915_gem_object_lock(obj: *mut DrmI915GemObject, ww: *mut I915GemWwCtx) -> i32 {
    let intr = !ww.is_null() && unsafe { (*ww).intr };
    unsafe { __i915_gem_object_lock(obj, ww, intr) }
}

// upstream: i915_gem_object.h i915_gem_object_lock_interruptible()
#[inline]
pub unsafe fn i915_gem_object_lock_interruptible(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
) -> i32 {
    WARN_ON!(!ww.is_null() && unsafe { !(*ww).intr });
    unsafe { __i915_gem_object_lock(obj, ww, true) }
}

// upstream: i915_gem_object.h i915_gem_object_trylock()
#[inline]
pub unsafe fn i915_gem_object_trylock(obj: *mut DrmI915GemObject, ww: *mut I915GemWwCtx) -> bool {
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    let ctx = if ww.is_null() {
        core::ptr::null_mut()
    } else {
        unsafe { core::ptr::addr_of_mut!((*ww).ctx) }
    };
    unsafe { crate::linux::gem::dma_resv_lock((*base).resv, ctx, false, true) == 0 }
}

// upstream: i915_gem_object.h i915_gem_object_unlock()
#[inline]
pub unsafe fn i915_gem_object_unlock(obj: *mut DrmI915GemObject) {
    let ops = unsafe { (*obj).ops };
    if let Some(adjust_lru) = unsafe { (*ops).adjust_lru } {
        unsafe { adjust_lru(obj) };
    }
    let base = unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) };
    unsafe { crate::linux::gem::dma_resv_unlock((*base).resv) };
}

// upstream: i915_gem_object.h i915_gem_object_has_pages()
#[inline]
pub unsafe fn i915_gem_object_has_pages(obj: *const DrmI915GemObject) -> bool {
    let pages = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*obj).mm.pages)) };
    !pages.is_null() && !crate::linux_config::IS_ERR(pages)
}

// upstream: i915_gem_object.h i915_gem_object_pin_pages()
#[inline]
pub unsafe fn i915_gem_object_pin_pages(obj: *mut DrmI915GemObject) -> i32 {
    unsafe { assert_object_held(obj) };
    if unsafe { atomic_inc_not_zero(&mut (*obj).mm.pages_pin_count) } {
        return 0;
    }
    unsafe { __i915_gem_object_get_pages(obj) }
}

// upstream: i915_gem_object.h __i915_gem_object_pin_pages()
#[inline]
pub unsafe fn __i915_gem_object_pin_pages(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pages(obj) });
    unsafe { atomic_inc(&mut (*obj).mm.pages_pin_count) };
}

// upstream: i915_gem_object.h i915_gem_object_has_pinned_pages()
#[inline]
pub unsafe fn i915_gem_object_has_pinned_pages(obj: *const DrmI915GemObject) -> bool {
    unsafe { atomic_read(&(*obj).mm.pages_pin_count) != 0 }
}

// upstream: i915_gem_object.h __i915_gem_object_unpin_pages()
#[inline]
pub unsafe fn __i915_gem_object_unpin_pages(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pages(obj) });
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pinned_pages(obj) });
    unsafe { atomic_dec(&mut (*obj).mm.pages_pin_count) };
}

// upstream: i915_gem_object.h i915_gem_object_unpin_pages()
#[inline]
pub unsafe fn i915_gem_object_unpin_pages(obj: *mut DrmI915GemObject) {
    unsafe { __i915_gem_object_unpin_pages(obj) };
}

// upstream: i915_gem_object.h i915_gem_object_unpin_map()
#[inline]
pub unsafe fn i915_gem_object_unpin_map(obj: *mut DrmI915GemObject) {
    unsafe { i915_gem_object_unpin_pages(obj) };
}

// upstream: i915_gem_object.h i915_gem_object_finish_access()
#[inline]
pub unsafe fn i915_gem_object_finish_access(obj: *mut DrmI915GemObject) {
    unsafe { i915_gem_object_unpin_pages(obj) };
}

#[cfg(test)]
mod tests {
    use super::i915_gem_object_has_pages;
    use crate::{
        i915_gem_object_header_upstream,
        i915_gem_object_types_upstream::DrmI915GemObject,
        i915_gem_pages_upstream::Scatterlist,
        intel_context_upstream::SgTable,
    };
    use core::{mem::MaybeUninit, ptr};

    fn has_pages_in_both_inline_helpers(pages: *mut SgTable) -> (bool, bool) {
        // The helpers inspect only mm.pages; keeping the rest uninitialized makes
        // this fixture independent of unrelated GEM locks and callbacks.
        let mut object = MaybeUninit::<DrmI915GemObject>::uninit();
        let object_ptr = object.as_mut_ptr();
        // SAFETY: object_ptr points into a writable MaybeUninit allocation and
        // this exact field is initialized before either helper reads it.
        unsafe { ptr::addr_of_mut!((*object_ptr).mm.pages).write(pages) };
        // SAFETY: mm.pages is initialized above and both helpers read only that field.
        let api = unsafe { i915_gem_object_has_pages(object_ptr) };
        // SAFETY: same initialized field and lifetime; the header helper delegates
        // to the API helper so both public inline surfaces share semantics.
        let header = unsafe {
            i915_gem_object_header_upstream::i915_gem_object_has_pages(object_ptr)
        };
        (api, header)
    }

    #[test]
    fn has_pages_rejects_null_and_error_pointers_but_accepts_an_sg_table() {
        assert_eq!(has_pages_in_both_inline_helpers(ptr::null_mut()), (false, false));
        let error = crate::linux_config::ERR_PTR::<SgTable>(-12);
        assert_eq!(has_pages_in_both_inline_helpers(error), (false, false));

        // A stack SG record is a non-null, non-error pointer with a plausible
        // first entry; the predicate must accept it without dereferencing it.
        let mut page_storage = [0u64; 1];
        let mut scatterlist = Scatterlist {
            page_link: page_storage.as_mut_ptr().cast::<u8>() as usize,
            offset: 0,
            length: 4096,
            dma_address: 0,
            dma_length: 4096,
            _dma_flags_or_padding: 0,
        };
        let mut table = SgTable {
            sgl: &mut scatterlist,
            nents: 1,
            orig_nents: 1,
        };
        assert_eq!(has_pages_in_both_inline_helpers(&mut table), (true, true));
    }
}
