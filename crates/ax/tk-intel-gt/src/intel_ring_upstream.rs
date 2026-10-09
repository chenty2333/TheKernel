// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gt/intel_ring.c` translation.
//!
//! This source-order implementation is separate from the narrow `intel_ring.rs`
//! model. It uses the canonical ring, request, VMA, object, engine, and timeline
//! owner records. The target configuration has CONFIG_DRM_I915_SELFTEST=n.
#![allow(unsafe_code)]

use core::{
    ffi::{c_long, c_void},
    mem::size_of,
};

use crate::{
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_BO_ALLOC_PM_VOLATILE, I915_BO_ALLOC_VOLATILE, I915_BO_READONLY,
    },
    i915_gem_lmem_upstream::i915_gem_object_create_lmem,
    i915_request_types_upstream::I915Request,
    i915_vma_types_upstream::{I915_VMA_CAN_FENCE_BIT, I915_VMA_GGTT_WRITE_BIT, I915Vma},
    intel_context_upstream::{I915AddressSpace, I915GemWwCtx, I915GttView, Kref},
    intel_engine_cs_upstream::{AtomicT, IntelEngineCs},
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{I915Ggtt, i915_ggtt_has_aperture},
    intel_ring_types_upstream::{CACHELINE_BYTES, IntelRing},
    intel_timeline_types_upstream::IntelTimeline,
    linux::memory::{kref_get, kref_put},
    linux_i915_private::DrmI915Private,
};

const PIN_MAPPABLE: u32 = 1 << 3;
const PIN_HIGH: u32 = 1 << 5;
const PIN_OFFSET_BIAS: u32 = 1 << 6;
const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;
const INTEL_I830: u32 = 1;
const INTEL_I845G: u32 = 2;

unsafe extern "C" {
    fn i915_gem_object_create_stolen(i915: *mut DrmI915Private, size: u64)
    -> *mut DrmI915GemObject;
    pub fn i915_gem_object_create_internal(
        i915: *mut DrmI915Private,
        size: u64,
    ) -> *mut DrmI915GemObject;
    fn i915_gem_object_is_stolen(obj: *const DrmI915GemObject) -> bool;
    fn i915_gem_object_pin_map(obj: *mut DrmI915GemObject, map_type: u32) -> *mut c_void;
    fn i915_vma_instance(
        obj: *mut DrmI915GemObject,
        vm: *mut I915AddressSpace,
        view: *const I915GttView,
    ) -> *mut I915Vma;
    fn i915_ggtt_pin(vma: *mut I915Vma, ww: *mut I915GemWwCtx, align: u32, flags: u32) -> i32;
    fn i915_vma_pin_iomap(vma: *mut I915Vma) -> *mut c_void;
    fn i915_vma_unpin_iomap(vma: *mut I915Vma);
    fn i915_vma_make_unshrinkable(vma: *mut I915Vma) -> *mut I915Vma;
    fn i915_vma_make_purgeable(vma: *mut I915Vma);
    fn intel_gt_coherent_map_type(
        gt: *mut IntelGt,
        obj: *mut DrmI915GemObject,
        always_coherent: bool,
    ) -> u32;
    fn drm_gem_object_free(refcount: *mut Kref);
}

unsafe extern "C" fn drm_gem_object_release(refcount: *mut Kref) {
    // SAFETY: this is the DRM GEM kref finalizer used by upstream's inline put.
    unsafe { drm_gem_object_free(refcount) }
}

/// Exact inline dependency of `i915_vma_put()` used by ring destruction.
///
/// # Safety
/// `vma` must point to a live VMA whose caller-owned object reference is being
/// released.
unsafe fn i915_vma_put(vma: *mut I915Vma) {
    let base = unsafe { core::ptr::addr_of_mut!((*(*vma).obj).base) };
    // The GEM kref is the first field in the DRM base object.
    unsafe { crate::linux::memory::kref_put(base.cast::<Kref>(), drm_gem_object_release) };
}

/// `i915_vma_is_map_and_fenceable()` inline dependency from i915_vma.h.
unsafe fn i915_vma_is_map_and_fenceable(vma: *const I915Vma) -> bool {
    unsafe { crate::linux::bits::test_bit(I915_VMA_CAN_FENCE_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_unset_ggtt_write()` inline dependency from i915_vma.h.
unsafe fn i915_vma_unset_ggtt_write(vma: *mut I915Vma) -> bool {
    unsafe {
        crate::linux::bits::test_and_clear_bit(I915_VMA_GGTT_WRITE_BIT, &mut (*vma).flags.counter)
    }
}

/// `i915_gem_object_set_readonly()` inline dependency from i915_gem_object.h.
unsafe fn i915_gem_object_set_readonly(obj: *mut DrmI915GemObject) {
    unsafe { (*obj).flags |= I915_BO_READONLY }
}

/// `i915_gem_object_unpin_map()` inline dependency from i915_gem_object.h.
unsafe fn i915_gem_object_unpin_map(obj: *mut DrmI915GemObject) {
    let pages = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*obj).mm.pages)) };
    GEM_BUG_ON!(pages.is_null() || crate::linux_config::IS_ERR(pages));
    GEM_BUG_ON!(unsafe { crate::linux::memory::atomic_read(&(*obj).mm.pages_pin_count) == 0 });
    unsafe { crate::linux::memory::atomic_dec(&mut (*obj).mm.pages_pin_count) };
}

/// Source-header `__intel_ring_space()` helper; C unsigned arithmetic wraps.
#[inline]
fn intel_ring_space(head: u32, tail: u32, size: u32) -> u32 {
    GEM_BUG_ON!(size == 0 || !size.is_power_of_two());
    head.wrapping_sub(tail).wrapping_sub(CACHELINE_BYTES as u32) & size.wrapping_sub(1)
}

// upstream: intel_ring.c intel_ring_update_space()
pub unsafe fn intel_ring_update_space(ring: *mut IntelRing) -> u32 {
    let space = unsafe { intel_ring_space((*ring).head, (*ring).emit, (*ring).size) };
    unsafe { (*ring).space = space };
    space
}

// upstream: intel_ring.c __intel_ring_pin()
pub unsafe fn __intel_ring_pin(ring: *mut IntelRing) {
    GEM_BUG_ON!(crate::linux::memory::atomic_read(unsafe { &(*ring).pin_count }) == 0);
    unsafe { crate::linux::memory::atomic_inc(&mut (*ring).pin_count) };
}

// upstream: intel_ring.c intel_ring_pin()
pub unsafe fn intel_ring_pin(ring: *mut IntelRing, ww: *mut I915GemWwCtx) -> i32 {
    let vma = unsafe { (*ring).vma };
    if crate::linux::memory::atomic_fetch_inc(unsafe { &mut (*ring).pin_count }) != 0 {
        return 0;
    }

    // The ring-wrap bias and aperture query are inline dependencies whose
    // complete GGTT owner is not yet available; bind to their VMA/GT API owners.
    let mut flags = crate::i915_vma_api_upstream::i915_ggtt_pin_bias(vma) | PIN_OFFSET_BIAS;
    if unsafe { i915_gem_object_is_stolen((*vma).obj) } {
        flags |= PIN_MAPPABLE;
    } else {
        flags |= PIN_HIGH;
    }

    let mut ret = unsafe { i915_ggtt_pin(vma, ww, 0, flags) };
    if ret != 0 {
        crate::linux::memory::atomic_dec(unsafe { &mut (*ring).pin_count });
        return ret;
    }

    let addr = if unsafe { i915_vma_is_map_and_fenceable(vma) }
        && !unsafe { crate::linux::i915::HAS_LLC((*(*vma).vm).i915) }
    {
        unsafe { i915_vma_pin_iomap(vma) }
    } else {
        let i915 = unsafe { (*(*vma).vm).i915.cast::<DrmI915Private>() };
        let gt = unsafe { (*(*vma).vm).gt.cast::<IntelGt>() };
        let map_type = unsafe { intel_gt_coherent_map_type(gt, (*vma).obj, false) };
        let _ = i915;
        unsafe { i915_gem_object_pin_map((*vma).obj, map_type) }
    };

    if crate::linux_config::IS_ERR(addr) {
        ret = crate::linux_config::PTR_ERR(addr);
        unsafe { crate::i915_vma_api_upstream::i915_vma_unpin(vma) };
        crate::linux::memory::atomic_dec(unsafe { &mut (*ring).pin_count });
        return ret;
    }

    unsafe { i915_vma_make_unshrinkable(vma) };
    // Discard bytes not submitted to hardware before publishing the mapping.
    unsafe { intel_ring_reset(ring, (*ring).emit) };
    unsafe { (*ring).vaddr = addr };
    0
}

// upstream: intel_ring.c intel_ring_reset()
pub unsafe fn intel_ring_reset(ring: *mut IntelRing, tail: u32) {
    let tail = tail & unsafe { (*ring).size.wrapping_sub(1) };
    unsafe {
        (*ring).tail = tail;
        (*ring).head = tail;
        (*ring).emit = tail;
        intel_ring_update_space(ring);
    }
}

// upstream: intel_ring.c intel_ring_unpin()
pub unsafe fn intel_ring_unpin(ring: *mut IntelRing) {
    let vma = unsafe { (*ring).vma };
    if !crate::linux::memory::atomic_dec_and_test(unsafe { &mut (*ring).pin_count }) {
        return;
    }

    let _ = unsafe { i915_vma_unset_ggtt_write(vma) };
    if unsafe { i915_vma_is_map_and_fenceable(vma) }
        && !unsafe { crate::linux::i915::HAS_LLC((*(*vma).vm).i915) }
    {
        unsafe { i915_vma_unpin_iomap(vma) };
    } else {
        unsafe { i915_gem_object_unpin_map((*vma).obj) };
    }

    unsafe { i915_vma_make_purgeable(vma) };
    unsafe { crate::i915_vma_api_upstream::i915_vma_unpin(vma) };
}

// upstream: intel_ring.c create_ring_vma()
unsafe fn create_ring_vma(ggtt: *mut I915Ggtt, size: i32) -> *mut I915Vma {
    // `struct i915_ggtt` begins with its embedded `i915_address_space vm`.
    let vm = ggtt.cast::<I915AddressSpace>();
    let i915 = unsafe { (*vm).i915.cast::<DrmI915Private>() };
    let mut obj = unsafe {
        i915_gem_object_create_lmem(
            i915,
            size as u64,
            (I915_BO_ALLOC_VOLATILE | I915_BO_ALLOC_PM_VOLATILE) as u32,
        )
    };

    if crate::linux_config::IS_ERR(obj)
        && i915_ggtt_has_aperture(ggtt)
        && !unsafe { crate::linux::i915::HAS_LLC(i915) }
    {
        obj = unsafe { i915_gem_object_create_stolen(i915, size as u64) };
    }
    if crate::linux_config::IS_ERR(obj) {
        obj = unsafe { i915_gem_object_create_internal(i915, size as u64) };
    }
    if crate::linux_config::IS_ERR(obj) {
        return obj.cast::<I915Vma>();
    }

    // `has_read_only` is bit 2 in intel_gtt.h's address-space bitfield byte.
    if unsafe { (*vm).vm_flags & (1 << 2) != 0 } {
        unsafe { i915_gem_object_set_readonly(obj) };
    }

    let vma = unsafe { i915_vma_instance(obj, vm, core::ptr::null()) };
    if crate::linux_config::IS_ERR(vma) {
        // Source i915_gem_object_put() is an inline kref_put through DRM.
        let base = unsafe { core::ptr::addr_of_mut!((*obj).base) };
        unsafe {
            crate::linux::memory::kref_put(base.cast::<Kref>(), drm_gem_object_release);
        }
        return vma;
    }
    vma
}

// upstream: intel_ring.c intel_engine_create_ring()
pub unsafe fn intel_engine_create_ring(engine: *mut IntelEngineCs, size: i32) -> *mut IntelRing {
    let i915 = unsafe { (*engine).i915 };
    GEM_BUG_ON!(size <= 0 || !(size as u32).is_power_of_two());
    let size_u32 = size as u32;
    GEM_BUG_ON!(
        crate::intel_engine_regs_upstream::RING_CTL_SIZE(size_u32)
            & !crate::intel_engine_regs_upstream::RING_NR_PAGES
            != 0
    );

    let ring = crate::linux::memory::kzalloc_obj::<IntelRing>();
    if ring.is_null() {
        return crate::linux_config::ERR_PTR(-crate::linux_config::ENOMEM);
    }

    unsafe {
        crate::linux::memory::kref_init(core::ptr::addr_of_mut!((*ring).ref_));
        (*ring).size = size_u32;
        (*ring).wrap = u32::BITS - crate::linux::primitives::ilog2(size_u32);
        (*ring).effective_size = size_u32;
    }

    if unsafe {
        crate::linux::i915::IS_PLATFORM(i915, INTEL_I830)
            || crate::linux::i915::IS_PLATFORM(i915, INTEL_I845G)
    } {
        unsafe {
            (*ring).effective_size = (*ring)
                .effective_size
                .wrapping_sub(2 * CACHELINE_BYTES as u32);
        }
    }
    unsafe { intel_ring_update_space(ring) };

    let ggtt = unsafe { (*(*engine).gt).ggtt };
    let vma = unsafe { create_ring_vma(ggtt, size) };
    if crate::linux_config::IS_ERR(vma) {
        unsafe { crate::linux::memory::kfree(ring) };
        return vma.cast::<IntelRing>();
    }
    unsafe { (*ring).vma = vma };
    ring
}

// upstream: intel_ring.c intel_ring_free()
pub unsafe extern "C" fn intel_ring_free(reference: *mut Kref) {
    // `ref` is the first `intel_ring` member.
    let ring = reference.cast::<IntelRing>();
    unsafe {
        i915_vma_put((*ring).vma);
        crate::linux::memory::kfree(ring);
    }
}

// upstream: intel_ring.h intel_ring_get()
#[inline]
pub unsafe fn intel_ring_get(ring: *mut IntelRing) -> *mut IntelRing {
    unsafe { kref_get(core::ptr::addr_of_mut!((*ring).ref_)) };
    ring
}

// upstream: intel_ring.h intel_ring_put()
#[inline]
pub unsafe fn intel_ring_put(ring: *mut IntelRing) {
    unsafe {
        kref_put(core::ptr::addr_of_mut!((*ring).ref_), intel_ring_free);
    }
}

#[inline(never)]
// upstream: intel_ring.c wait_for_space()
unsafe fn wait_for_space(ring: *mut IntelRing, tl: *mut IntelTimeline, bytes: u32) -> i32 {
    if unsafe { intel_ring_update_space(ring) } >= bytes {
        return 0;
    }

    let head = unsafe { core::ptr::addr_of_mut!((*tl).requests) };
    GEM_BUG_ON!(crate::linux::list::list_empty(unsafe { &*head }));
    let mut entry = unsafe { (*head).next };
    let mut target = core::ptr::null_mut::<I915Request>();
    while entry != head {
        let request = unsafe {
            entry
                .cast::<u8>()
                .sub(core::mem::offset_of!(I915Request, link))
                .cast::<I915Request>()
        };
        if unsafe { (*request).ring != ring } {
            entry = unsafe { (*entry).next };
            continue;
        }

        if bytes
            <= intel_ring_space(
                unsafe { (*request).postfix },
                unsafe { (*ring).emit },
                unsafe { (*ring).size },
            )
        {
            target = request;
            break;
        }
        entry = unsafe { (*entry).next };
    }

    if GEM_WARN_ON!(target.is_null()) {
        return -crate::linux_config::ENOSPC;
    }

    let timeout = unsafe {
        crate::i915_request_upstream::i915_request_wait(
            target,
            I915_WAIT_INTERRUPTIBLE,
            crate::linux_config::MAX_SCHEDULE_TIMEOUT as c_long,
        )
    };
    if timeout < 0 {
        return timeout as i32;
    }

    unsafe { crate::i915_request_upstream::i915_request_retire_upto(target) };
    unsafe { intel_ring_update_space(ring) };
    GEM_BUG_ON!(unsafe { (*ring).space } < bytes);
    0
}

// upstream: intel_ring.c intel_ring_begin()
pub unsafe fn intel_ring_begin(rq: *mut I915Request, num_dwords: u32) -> *mut u32 {
    let ring = unsafe { (*rq).ring };
    let remain_usable = unsafe { (*ring).effective_size.wrapping_sub((*ring).emit) };
    let bytes = num_dwords.wrapping_mul(size_of::<u32>() as u32);
    let mut need_wrap = 0u32;
    let mut total_bytes = bytes.wrapping_add(unsafe { (*rq).reserved_space });

    // Packets must be qword aligned.
    GEM_BUG_ON!(num_dwords & 1 != 0);
    GEM_BUG_ON!(total_bytes > unsafe { (*ring).effective_size });

    if total_bytes > remain_usable {
        let remain_actual = unsafe { (*ring).size.wrapping_sub((*ring).emit) };
        if bytes > remain_usable {
            total_bytes = total_bytes.wrapping_add(remain_actual);
            need_wrap = remain_actual | 1;
        } else {
            total_bytes = unsafe { (*rq).reserved_space }.wrapping_add(remain_actual);
        }
    }

    if total_bytes > unsafe { (*ring).space } {
        // Request finalization reserves enough space to be infallible.
        GEM_BUG_ON!(unsafe { (*rq).reserved_space } == 0);
        let ret = unsafe { wait_for_space(ring, (*rq).timeline, total_bytes) };
        if ret != 0 {
            return crate::linux_config::ERR_PTR(ret);
        }
    }

    if need_wrap != 0 {
        need_wrap &= !1;
        GEM_BUG_ON!(need_wrap > unsafe { (*ring).space });
        GEM_BUG_ON!(unsafe { (*ring).emit.wrapping_add(need_wrap) > (*ring).size });
        GEM_BUG_ON!(need_wrap & 7 != 0);

        // Source memset64 fills the ring tail with MI_NOOP (zero).
        unsafe {
            core::ptr::write_bytes(
                (*ring).vaddr.add((*ring).emit as usize).cast::<u64>(),
                0,
                (need_wrap / size_of::<u64>() as u32) as usize,
            );
            (*ring).space = (*ring).space.wrapping_sub(need_wrap);
            (*ring).emit = 0;
        }
    }

    GEM_BUG_ON!(unsafe { (*ring).emit > (*ring).size.wrapping_sub(bytes) });
    GEM_BUG_ON!(unsafe { (*ring).space < bytes });
    let cs = unsafe {
        (*ring)
            .vaddr
            .cast::<u8>()
            .add((*ring).emit as usize)
            .cast::<u32>()
    };
    if crate::linux_config::IS_ENABLED(crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM) {
        // Preserve memset32's repeated u32 pattern (0x0000005a on the target).
        unsafe {
            for index in 0..(bytes / size_of::<u32>() as u32) as usize {
                cs.add(index).write(crate::linux::registers::POISON_INUSE);
            }
        }
    }
    unsafe {
        (*ring).emit = (*ring).emit.wrapping_add(bytes);
        (*ring).space = (*ring).space.wrapping_sub(bytes);
    }
    cs
}

// This source conditionally includes `selftest_ring.c` only when
// CONFIG_DRM_I915_SELFTEST is enabled; the target configuration disables it.
