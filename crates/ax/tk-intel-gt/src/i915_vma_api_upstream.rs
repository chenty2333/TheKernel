// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
//
//! API declarations and source-transcribed inline helpers from Linux v7.2.3
//! `drivers/gpu/drm/i915/i915_vma.h`.
//!
//! This file intentionally does not replace the owning VMA/resource/layout
//! modules. DRM MM, reservation-object, GGTT, and fence-register helpers whose
//! owner records/APIs are still opaque are listed below rather than modeled by
//! guessed fields or no-op substitutes.
#![allow(unsafe_code)]

use core::ffi::{c_ulong, c_void};

/// `I915_VMA_PIN_MASK` from the included `i915_vma_types.h`.
pub use crate::i915_vma_types_upstream::{
    I915_VMA_CAN_FENCE_BIT, I915_VMA_GGTT_BIT, I915_VMA_GGTT_WRITE_BIT, I915_VMA_PIN_MASK,
    I915_VMA_SCANOUT_BIT, I915_VMA_USERFAULT_BIT,
};
use crate::{
    i915_request_types_upstream::I915Request,
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::I915Vma,
    intel_context_upstream::{
        DmaFence, DrmI915GemObject, I915AddressSpace, I915GemWwCtx, I915GttView, Kref,
    },
    intel_engine_cs_upstream::AtomicT,
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::i915_vm_to_ggtt,
};

/// `I915_VMA_RELEASE_MAP` from `i915_vma.h:48-49`.
pub const I915_VMA_RELEASE_MAP: u32 = 1 << 0;

// upstream: i915_vma.h __i915_vma_offset()
#[inline]
pub unsafe fn __i915_vma_offset(vma: *const I915Vma) -> u64 {
    unsafe { (*vma).node.start + (*vma).guard as u64 }
}

// upstream: i915_vma.h i915_vma_offset()
#[inline]
pub unsafe fn i915_vma_offset(vma: *const I915Vma) -> u64 {
    GEM_BUG_ON!(!unsafe { crate::linux::gem_memory::drm_mm_node_allocated(&(*vma).node) });
    unsafe { __i915_vma_offset(vma) }
}

unsafe extern "C" {
    /// `i915_vma_instance()` (`i915_vma.h:43-46`).
    pub fn i915_vma_instance(
        obj: *mut DrmI915GemObject,
        vm: *mut I915AddressSpace,
        view: *const I915GttView,
    ) -> *mut I915Vma;
    /// `i915_vma_unpin_and_release()` (`i915_vma.h:48`).
    pub fn i915_vma_unpin_and_release(vma: *mut *mut I915Vma, flags: u32);
}

/// `i915_vma_is_active()` (`i915_vma.h:51-54`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_active(vma: *const I915Vma) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { crate::linux::memory::atomic_read(&(*vma).active.count) != 0 }
}

/// `BIT(31)` from `i915_vma.h:56-58`; execbuffer-only, do not reserve.
pub const __EXEC_OBJECT_NO_RESERVE: u32 = 1 << 31;
/// `BIT(30)` from `i915_vma.h:56-58`.
pub const __EXEC_OBJECT_NO_REQUEST_AWAIT: u32 = 1 << 30;

unsafe extern "C" {
    /// Out-of-line implementation declared at `i915_vma.h:60-63`.
    pub fn _i915_vma_move_to_active(
        vma: *mut I915Vma,
        rq: *mut I915Request,
        fence: *mut DmaFence,
        flags: u32,
    ) -> i32;
}

/// Inline `i915_vma_move_to_active()` (`i915_vma.h:64-69`).
///
/// # Safety
/// `vma` and `rq` must be live; `rq` must remain live through the call.
pub unsafe fn i915_vma_move_to_active(vma: *mut I915Vma, rq: *mut I915Request, flags: u32) -> i32 {
    // SAFETY: guaranteed by the caller; the fence is embedded in `rq`.
    unsafe { _i915_vma_move_to_active(vma, rq, core::ptr::addr_of_mut!((*rq).fence), flags) }
}

/// `__i915_vma_flags(v)` from `i915_vma.h:71`; the pointer addresses the
/// atomic flags counter exactly as the C macro does.
#[macro_export]
macro_rules! __i915_vma_flags {
    ($vma:expr) => {{
        // SAFETY: the expression must designate a live `i915_vma`.
        unsafe { core::ptr::addr_of_mut!((*($vma)).flags.counter).cast::<core::ffi::c_ulong>() }
    }};
}

/// `i915_vma_is_ggtt()` (`i915_vma.h:73-76`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_ggtt(vma: *const I915Vma) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { crate::linux::bits::test_bit(I915_VMA_GGTT_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_is_dpt()` (`i915_vma.h:78-81`), using `intel_gtt.h`'s
/// adjacent `is_dpt:1` bit in the address-space `vm_flags` byte.
///
/// # Safety
/// `vma` and `vma.vm` must point to live records.
pub unsafe fn i915_vma_is_dpt(vma: *const I915Vma) -> bool {
    // C bitfield order in intel_gtt.h: is_ggtt is bit 0, is_dpt is bit 1.
    unsafe { (*(*vma).vm).vm_flags & (1 << 1) != 0 }
}

/// `i915_vma_has_ggtt_write()` (`i915_vma.h:83-86`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_has_ggtt_write(vma: *const I915Vma) -> bool {
    unsafe { crate::linux::bits::test_bit(I915_VMA_GGTT_WRITE_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_set_ggtt_write()` (`i915_vma.h:88-92`).
///
/// # Safety
/// `vma` must point to a live GGTT `i915_vma`.
pub unsafe fn i915_vma_set_ggtt_write(vma: *mut I915Vma) {
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });
    unsafe { crate::linux::bits::set_bit(I915_VMA_GGTT_WRITE_BIT, &mut (*vma).flags.counter) }
}

/// `i915_vma_unset_ggtt_write()` (`i915_vma.h:94-98`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_unset_ggtt_write(vma: *mut I915Vma) -> bool {
    unsafe {
        crate::linux::bits::test_and_clear_bit(I915_VMA_GGTT_WRITE_BIT, &mut (*vma).flags.counter)
    }
}

unsafe extern "C" {
    /// `i915_vma_flush_writes()` (`i915_vma.h:100`).
    pub fn i915_vma_flush_writes(vma: *mut I915Vma);
}

/// `i915_vma_is_map_and_fenceable()` (`i915_vma.h:102-105`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_map_and_fenceable(vma: *const I915Vma) -> bool {
    unsafe { crate::linux::bits::test_bit(I915_VMA_CAN_FENCE_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_set_userfault()` (`i915_vma.h:107-111`).
///
/// # Safety
/// `vma` must point to a live map-and-fenceable `i915_vma`.
pub unsafe fn i915_vma_set_userfault(vma: *mut I915Vma) -> bool {
    GEM_BUG_ON!(!unsafe { i915_vma_is_map_and_fenceable(vma) });
    unsafe {
        crate::linux::bits::test_and_set_bit(I915_VMA_USERFAULT_BIT, &mut (*vma).flags.counter)
    }
}

/// `i915_vma_unset_userfault()` (`i915_vma.h:113-116`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_unset_userfault(vma: *mut I915Vma) {
    unsafe { crate::linux::bits::clear_bit(I915_VMA_USERFAULT_BIT, &mut (*vma).flags.counter) }
}

/// `i915_vma_has_userfault()` (`i915_vma.h:118-121`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_has_userfault(vma: *const I915Vma) -> bool {
    unsafe { crate::linux::bits::test_bit(I915_VMA_USERFAULT_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_is_closed()` (`i915_vma.h:123-126`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_closed(vma: *const I915Vma) -> bool {
    unsafe { !crate::linux::list::list_empty(&*core::ptr::addr_of!((*vma).closed_link)) }
}

/// `i915_ggtt_pin_bias()` (`i915_vma.h:183-186`).
///
/// # Safety
/// `vma` must be a live VMA attached to the GGTT.
pub unsafe fn i915_ggtt_pin_bias(vma: *const I915Vma) -> u32 {
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });
    let ggtt = unsafe { i915_vm_to_ggtt((*vma).vm) };
    unsafe { (*ggtt).pin_bias }
}

// `__i915_vma_size`, `i915_vma_size`, `__i915_vma_offset`,
// `i915_vma_offset`, `i915_ggtt_offset`, and `i915_node_color_differs`
// (i915_vma.h:128-181, 332-336) need `drm_mm_node.start/size/color` and the
// node-allocation bit. The current DRM MM binding deliberately keeps
// `DrmMmNode` opaque, so these inline accessors cannot be represented here
// faithfully until the drm_mm.h owner binding exposes those members/helpers.
// `i915_ggtt_pin_bias` (183-186) is implemented below from the owning GGTT
// header's `pin_bias` member and `i915_vm_to_ggtt()` helper.
//
// `i915_vma_compare` (207-249) requires the full `i915_gtt_view` union payload
// and DRM `memcmp` semantics; the current I915GttView binding exposes only its
// discriminant, so no partial comparator is provided.
//
// The get/tryget/put helpers below follow the source's embedded GEM base kref.
// The base refcount is its first member in Linux's drm_gem_object layout.
unsafe extern "C" {
    fn drm_gem_object_free(refcount: *mut Kref);
}

unsafe extern "C" fn release_gem_object(refcount: *mut Kref) {
    // SAFETY: invoked only as the final-reference callback for a GEM object.
    unsafe { drm_gem_object_free(refcount) }
}

/// `i915_vma_get()` (`i915_vma.h:188-192`).
///
/// # Safety
/// `vma` must point to a live VMA whose object has a live reference.
pub unsafe fn i915_vma_get(vma: *mut I915Vma) -> *mut I915Vma {
    let obj_base = unsafe { core::ptr::addr_of_mut!((*(*vma).obj).base) };
    // SAFETY: drm_gem_object's kref is the first field of its base record.
    unsafe { crate::linux::memory::kref_get(obj_base.cast::<Kref>()) };
    vma
}

/// `i915_vma_tryget()` (`i915_vma.h:194-200`).
///
/// # Safety
/// `vma` must point to a live VMA; its object storage must remain valid while
/// attempting to acquire the reference.
pub unsafe fn i915_vma_tryget(vma: *mut I915Vma) -> *mut I915Vma {
    let obj_base = unsafe { core::ptr::addr_of_mut!((*(*vma).obj).base) };
    // SAFETY: the kref is the first field of the GEM base record.
    if unsafe { crate::linux::memory::kref_get_unless_zero(&mut *obj_base.cast::<Kref>()) } {
        vma
    } else {
        core::ptr::null_mut()
    }
}

/// `i915_vma_put()` (`i915_vma.h:202-205`).
///
/// # Safety
/// `vma` must point to a live VMA and the caller must own the corresponding
/// object reference.
pub unsafe fn i915_vma_put(vma: *mut I915Vma) {
    let obj_base = unsafe { core::ptr::addr_of_mut!((*(*vma).obj).base) };
    // SAFETY: `vma`'s object owns this DRM GEM kref and the callback is the
    // upstream DRM GEM finalizer used by drm_gem_object_put.
    unsafe { crate::linux::memory::kref_put(obj_base.cast::<Kref>(), release_gem_object) };
}

unsafe extern "C" {
    /// Pool/work record factory (`i915_vma.h:251`).
    pub fn i915_vma_work() -> *mut I915VmaWork;
    /// `i915_vma_bind()` (`i915_vma.h:252-256`).
    pub fn i915_vma_bind(
        vma: *mut I915Vma,
        pat_index: u32,
        flags: u32,
        work: *mut I915VmaWork,
        vma_res: *mut I915VmaResource,
    ) -> i32;
    pub fn i915_gem_valid_gtt_space(vma: *mut I915Vma, color: c_ulong) -> bool;
    pub fn i915_vma_misplaced(vma: *const I915Vma, size: u64, alignment: u64, flags: u64) -> bool;
    pub fn __i915_vma_set_map_and_fenceable(vma: *mut I915Vma);
    pub fn i915_vma_revoke_mmap(vma: *mut I915Vma);
    pub fn vma_invalidate_tlb(vm: *mut I915AddressSpace, tlb: *mut u32);
    pub fn __i915_vma_evict(vma: *mut I915Vma, async_: bool) -> *mut DmaFence;
    pub fn __i915_vma_unbind(vma: *mut I915Vma) -> i32;
    #[must_use]
    pub fn i915_vma_unbind(vma: *mut I915Vma) -> i32;
    #[must_use]
    pub fn i915_vma_unbind_async(vma: *mut I915Vma, trylock_vm: bool) -> i32;
    #[must_use]
    pub fn i915_vma_unbind_unlocked(vma: *mut I915Vma) -> i32;
    pub fn i915_vma_unlink_ctx(vma: *mut I915Vma);
    pub fn i915_vma_close(vma: *mut I915Vma);
    pub fn i915_vma_reopen(vma: *mut I915Vma);
    pub fn i915_vma_destroy_locked(vma: *mut I915Vma);
    pub fn i915_vma_destroy(vma: *mut I915Vma);
}

#[repr(C)]
pub struct I915VmaWork {
    _opaque: [u8; 0],
}

// `assert_vma_held`, `i915_vma_lock`, and `i915_vma_unlock` (i915_vma.h:276-
// 286) depend on DRM's reservation-object API and its ww-mutex internals. No
// reservation locking/assertion binding is currently available here; avoid
// substituting a plain mutex or a no-op assertion.

unsafe extern "C" {
    #[must_use]
    pub fn i915_vma_pin_ww(
        vma: *mut I915Vma,
        ww: *mut I915GemWwCtx,
        size: u64,
        alignment: u64,
        flags: u64,
    ) -> i32;
    #[must_use]
    pub fn i915_vma_pin(vma: *mut I915Vma, size: u64, alignment: u64, flags: u64) -> i32;
    pub fn i915_ggtt_pin(vma: *mut I915Vma, ww: *mut I915GemWwCtx, align: u32, flags: u32) -> i32;
}

/// `i915_vma_pin_count()` (`i915_vma.h:298-301`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_pin_count(vma: *const I915Vma) -> i32 {
    unsafe { crate::linux::memory::atomic_read(&(*vma).flags) & I915_VMA_PIN_MASK }
}

/// `i915_vma_is_pinned()` (`i915_vma.h:303-306`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_pinned(vma: *const I915Vma) -> bool {
    unsafe { i915_vma_pin_count(vma) != 0 }
}

/// `__i915_vma_pin()` (`i915_vma.h:308-312`).
///
/// # Safety
/// `vma` must point to a live, exclusively updated `i915_vma`.
pub unsafe fn __i915_vma_pin(vma: *mut I915Vma) {
    unsafe { crate::linux::memory::atomic_inc(&mut (*vma).flags) };
    GEM_BUG_ON!(!unsafe { i915_vma_is_pinned(vma) });
}

/// `__i915_vma_unpin()` (`i915_vma.h:314-318`).
///
/// # Safety
/// `vma` must point to a live, pinned `i915_vma`.
pub unsafe fn __i915_vma_unpin(vma: *mut I915Vma) {
    GEM_BUG_ON!(!unsafe { i915_vma_is_pinned(vma) });
    unsafe { crate::linux::memory::atomic_dec(&mut (*vma).flags) };
}

/// `i915_vma_unpin()` (`i915_vma.h:320-324`).
///
/// # Safety
/// `vma` must point to a live VMA whose DRM-MM node is allocated and pinned.
pub unsafe fn i915_vma_unpin(vma: *mut I915Vma) {
    GEM_BUG_ON!(!unsafe { crate::linux::gem_memory::drm_mm_node_allocated(&(*vma).node) });
    unsafe { __i915_vma_unpin(vma) };
}

/// `i915_vma_is_bound()` (`i915_vma.h:326-330`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_bound(vma: *const I915Vma, where_: u32) -> bool {
    unsafe { (crate::linux::memory::atomic_read(&(*vma).flags) as u32 & where_) != 0 }
}

/// `i915_vma_get_iomap()` (`i915_vma.h:338-341`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_get_iomap(vma: *mut I915Vma) -> *mut c_void {
    // SAFETY: READ_ONCE for a raw pointer load, matching the C helper.
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*vma).iomap)) }
}

unsafe extern "C" {
    pub fn i915_vma_pin_iomap(vma: *mut I915Vma) -> *mut c_void;
    pub fn i915_vma_unpin_iomap(vma: *mut I915Vma);
    #[must_use]
    pub fn i915_vma_pin_fence(vma: *mut I915Vma) -> i32;
    pub fn i915_vma_revoke_fence(vma: *mut I915Vma);
    pub fn __i915_vma_pin_fence(vma: *mut I915Vma) -> i32;
    pub fn i915_vma_parked(gt: *mut IntelGt);
}

// `__i915_vma_unpin_fence()` / `i915_vma_unpin_fence()` (i915_vma.h:386-405)
// require `i915_fence_reg.pin_count`; `I915FenceReg` remains pointer-only in
// its current owning binding. No incomplete inline fence decrement is exposed.

/// `i915_vma_is_scanout()` (`i915_vma.h:409-412`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_is_scanout(vma: *const I915Vma) -> bool {
    unsafe { crate::linux::bits::test_bit(I915_VMA_SCANOUT_BIT, &(*vma).flags.counter) }
}

/// `i915_vma_mark_scanout()` (`i915_vma.h:414-417`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_mark_scanout(vma: *mut I915Vma) {
    unsafe { crate::linux::bits::set_bit(I915_VMA_SCANOUT_BIT, &mut (*vma).flags.counter) }
}

/// `i915_vma_clear_scanout()` (`i915_vma.h:419-422`).
///
/// # Safety
/// `vma` must point to a live `i915_vma`.
pub unsafe fn i915_vma_clear_scanout(vma: *mut I915Vma) {
    unsafe { crate::linux::bits::clear_bit(I915_VMA_SCANOUT_BIT, &mut (*vma).flags.counter) }
}

unsafe extern "C" {
    pub fn i915_ggtt_clear_scanout(obj: *mut DrmI915GemObject);
}

/// `for_each_until(cond)` (`i915_vma.h:426`).
#[macro_export]
macro_rules! for_each_until {
    ($condition:expr) => {
        if $condition { break } else { () }
    };
}

/// Iterate the contiguous GGTT prefix of an object's VMA list, matching
/// `for_each_ggtt_vma(V, OBJ)` (`i915_vma.h:428-439`).
#[macro_export]
macro_rules! for_each_ggtt_vma {
    ($vma:ident, $obj:expr, $body:block) => {{
        // SAFETY: like the C list macro, the caller supplies a live object and
        // the loop body must not unlink/free the current list node.
        unsafe {
            let __head = core::ptr::addr_of!((*($obj)).vma.list)
                as *mut $crate::intel_engine_cs_upstream::ListHead;
            let mut __entry = (*__head).next;
            while __entry != __head {
                let $vma = (__entry as *mut u8)
                    .sub(core::mem::offset_of!(
                        $crate::i915_vma_types_upstream::I915Vma,
                        obj_link
                    ))
                    .cast::<$crate::i915_vma_types_upstream::I915Vma>();
                if !$crate::i915_vma_api_upstream::i915_vma_is_ggtt($vma) {
                    break;
                }
                $body;
                __entry = (*__entry).next;
            }
        }
    }};
}

unsafe extern "C" {
    pub fn i915_vma_make_unshrinkable(vma: *mut I915Vma) -> *mut I915Vma;
    pub fn i915_vma_make_shrinkable(vma: *mut I915Vma);
    pub fn i915_vma_make_purgeable(vma: *mut I915Vma);
    pub fn i915_vma_wait_for_bind(vma: *mut I915Vma) -> i32;
    pub fn i915_vma_module_exit();
    pub fn i915_vma_module_init() -> i32;
}

unsafe extern "C" {
    fn __i915_active_wait(
        active: *mut crate::intel_context_types_upstream::I915Active,
        state: i32,
    ) -> i32;
}

/// `i915_vma_sync()` (`i915_vma.h:447-451`).
///
/// # Safety
/// `vma` must point to a live VMA; the caller must obey the active object's
/// synchronization contract.
pub unsafe fn i915_vma_sync(vma: *mut I915Vma) -> i32 {
    // TASK_INTERRUPTIBLE is Linux's source state passed by i915_active_wait().
    unsafe {
        __i915_active_wait(
            core::ptr::addr_of_mut!((*vma).active),
            crate::linux::wait::TASK_INTERRUPTIBLE as i32,
        )
    }
}

/// `i915_vma_get_current_resource()` (`i915_vma.h:453-466`).
///
/// # Safety
/// `vma` must be live and bound with a non-null resource pointer.
pub unsafe fn i915_vma_get_current_resource(vma: *mut I915Vma) -> *mut I915VmaResource {
    let resource = unsafe { (*vma).resource };
    // The source inline takes a dma-fence reference and returns the resource.
    crate::linux::requests::dma_fence_get(unsafe {
        core::ptr::addr_of_mut!((*resource).unbind_fence)
    });
    resource
}

#[cfg(CONFIG_DRM_I915_SELFTEST)]
unsafe extern "C" {
    pub fn i915_vma_resource_init_from_vma(vma_res: *mut I915VmaResource, vma: *mut I915Vma);
    pub fn i915_vma_get_pages(vma: *mut I915Vma) -> i32;
    pub fn i915_vma_put_pages(vma: *mut I915Vma);
}
