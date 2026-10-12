// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_damage_helper.c` (batch B1c).
//!
//! Plane damage-clip handling: the FB_DIRTY ioctl helper, the damage
//! iterator used by drivers, and the merged-damage rectangle. Functions are
//! named and ordered as in the C file.

// The Linux ports state the invariant of each C-ABI call once in the module
// header, as the GT ports do; per-block SAFETY prose is not repeated.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::c_void;
use core::mem::{offset_of, size_of, zeroed};

use tk_linux_kapi::config::{EDEADLK, ENOMEM, IS_ERR, PTR_ERR, GFP_KERNEL};
use tk_linux_kapi::types::ListHead;

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_crtc_types::DrmCrtcState;
use crate::drm_damage_helper_types::DrmAtomicHelperDamageIter;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};
use crate::drm_property_types::DrmPropertyBlob;
use crate::drm_rect_types::DrmRect;
use crate::uapi_drm_consts::DRM_MODE_FB_DIRTY_ANNOTATE_COPY;
use crate::uapi_drm_mode_types::DrmModeRect;
use crate::uapi_drm_types::DrmClipRect;

/// `DRM_MODESET_ACQUIRE_INTERRUPTIBLE` (include/drm/drm_modeset_lock.h).
const DRM_MODESET_ACQUIRE_INTERRUPTIBLE: u32 = 1 << 0;

unsafe extern "C" {
    // upstream: drm_modeset_lock.c drm_modeset_acquire_init() / drm_modeset_acquire_fini()
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
    // upstream: drm_modeset_lock.c drm_modeset_drop_locks() / drm_modeset_backoff()
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    // upstream: drm_modeset_lock.c drm_modeset_lock() / drm_modeset_unlock()
    fn drm_modeset_lock(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock, ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_modeset_unlock(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock);
    // upstream: drm_atomic.c drm_atomic_commit_alloc() / drm_atomic_commit() / drm_atomic_commit_clear()
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32;
    fn drm_atomic_commit_clear(state: *mut DrmAtomicCommit);
    // upstream: drm_atomic.c __drm_atomic_commit_free()
    fn __drm_atomic_commit_free(kref: *mut tk_linux_kapi::types::Kref);
    // upstream: drm_atomic.c drm_atomic_get_plane_state()
    fn drm_atomic_get_plane_state(state: *mut DrmAtomicCommit, plane: *mut DrmPlane) -> *mut DrmPlaneState;
    // upstream: drm_property.c drm_property_create_blob() / drm_property_replace_blob() / drm_property_blob_put()
    fn drm_property_create_blob(dev: *mut DrmDevice, length: usize, data: *const c_void) -> *mut DrmPropertyBlob;
    fn drm_property_replace_blob(blob: *mut *mut DrmPropertyBlob, new_blob: *mut DrmPropertyBlob) -> bool;
    fn drm_property_blob_put(blob: *mut DrmPropertyBlob);
    // upstream: drm_rect.c drm_rect_intersect()
    fn drm_rect_intersect(r: *mut DrmRect, clip: *const DrmRect) -> bool;
}

// upstream: include/drm/drm_atomic.h drm_atomic_commit_put()
#[inline]
pub(crate) unsafe fn drm_atomic_commit_put(state: *mut DrmAtomicCommit) {
    unsafe {
        tk_linux_kapi::primitives::kref_put(core::ptr::addr_of_mut!((*state).r#ref), Some(__drm_atomic_commit_free));
    }
}

// upstream: include/drm/drm_atomic.h drm_atomic_get_new_crtc_state()
#[inline]
unsafe fn drm_atomic_get_new_crtc_state(state: *mut DrmAtomicCommit, crtc: *mut crate::drm_crtc_types::DrmCrtc) -> *mut DrmCrtcState {
    unsafe { (*(*state).crtcs.add((*crtc).index as usize)).new_state }
}

// upstream: include/drm/drm_atomic.h drm_atomic_crtc_needs_modeset()
#[inline]
unsafe fn drm_atomic_crtc_needs_modeset(state: *const DrmCrtcState) -> bool {
    // bit-fields: mode_changed:1 (bit 1), active_changed:1 (bit 2), connectors_changed:1 (bit 3)
    unsafe { ((*state)._bitfields_10[0] & 0b1110) != 0 }
}

// upstream: include/drm/drm_plane.h drm_plane_state_src()
#[inline]
unsafe fn drm_plane_state_src(state: *const DrmPlaneState) -> DrmRect {
    unsafe {
        DrmRect {
            x1: (*state).src_x as i32,
            y1: (*state).src_y as i32,
            x2: ((*state).src_x + (*state).src_w) as i32,
            y2: ((*state).src_y + (*state).src_h) as i32,
        }
    }
}

// upstream: include/drm/drm_rect.h drm_rect_equals()
#[inline]
fn drm_rect_equals(r1: &DrmRect, r2: &DrmRect) -> bool {
    r1.x1 == r2.x1 && r1.y1 == r2.y1 && r1.x2 == r2.x2 && r1.y2 == r2.y2
}

// upstream: drm_damage_helper.c convert_clip_rect_to_rect()
unsafe fn convert_clip_rect_to_rect(src: *const DrmClipRect, dest: *mut DrmModeRect, mut num_clips: u32, src_inc: u32) {
    unsafe {
        let mut src = src;
        let mut dest = dest;
        while num_clips > 0 {
            (*dest).x1 = (*src).x1 as i32;
            (*dest).y1 = (*src).y1 as i32;
            (*dest).x2 = (*src).x2 as i32;
            (*dest).y2 = (*src).y2 as i32;
            src = src.add(src_inc as usize);
            dest = dest.add(1);
            num_clips -= 1;
        }
    }
}

/// Drops the damage clips of planes whose CRTC is doing a full modeset.
// upstream: drm_damage_helper.c drm_atomic_helper_check_plane_damage()
pub unsafe fn drm_atomic_helper_check_plane_damage(state: *mut DrmAtomicCommit, plane_state: *mut DrmPlaneState) {
    unsafe {
        if !(*plane_state).crtc.is_null() {
            let crtc_state = drm_atomic_get_new_crtc_state(state, (*plane_state).crtc);
            if tk_linux_kapi::assert::warn_on(crtc_state.is_null()) {
                return;
            }

            if drm_atomic_crtc_needs_modeset(crtc_state) {
                drm_property_blob_put((*plane_state).fb_damage_clips);
                (*plane_state).fb_damage_clips = core::ptr::null_mut();
            }
        }
    }
}

/// Iterates the planes of `dev` in `mode_config.plane_list` order.
// upstream: include/drm/drm_plane.h drm_for_each_plane()
unsafe fn drm_for_each_plane(dev: *mut DrmDevice, mut f: impl FnMut(*mut DrmPlane) -> bool) {
    unsafe {
        let head: *mut ListHead = core::ptr::addr_of_mut!((*dev).mode_config.plane_list);
        let mut cur = (*head).next;
        while cur != head {
            let plane = (cur as *mut u8).sub(offset_of!(DrmPlane, head)) as *mut DrmPlane;
            if !f(plane) {
                break;
            }
            cur = (*cur).next;
        }
    }
}

/// Handles the DIRTYFB ioctl and internal defio flushes: attaches the given
/// clip rectangles as FB_DAMAGE_CLIPS to every plane showing `fb`.
// upstream: drm_damage_helper.c drm_atomic_helper_dirtyfb()
pub unsafe fn drm_atomic_helper_dirtyfb(
    fb: *mut DrmFramebuffer,
    file_priv: *mut DrmFile,
    flags: u32,
    _color: u32,
    clips: *mut DrmClipRect,
    mut num_clips: u32,
) -> i32 {
    unsafe {
        let mut ctx: DrmModesetAcquireCtx = zeroed();
        let ctx_ptr: *mut DrmModesetAcquireCtx = &mut ctx;
        let mut damage: *mut DrmPropertyBlob = core::ptr::null_mut();
        let mut rects: *mut DrmModeRect = core::ptr::null_mut();
        let mut ret: i32 = 0;

        // When called from ioctl, we are interruptible, but not when called
        // internally (ie. defio worker).
        drm_modeset_acquire_init(
            ctx_ptr,
            if !file_priv.is_null() { DRM_MODESET_ACQUIRE_INTERRUPTIBLE } else { 0 },
        );

        let state = drm_atomic_commit_alloc((*fb).dev);
        if state.is_null() {
            ret = -ENOMEM;
            // goto out_drop_locks
            drm_modeset_drop_locks(ctx_ptr);
            drm_modeset_acquire_fini(ctx_ptr);
            return ret;
        }

        (*state).acquire_ctx = ctx_ptr;

        // Preparation before the retry loop; a failure here is `goto out`.
        let mut prep_ok = true;
        if !clips.is_null() {
            let mut inc: u32 = 1;
            if (flags & DRM_MODE_FB_DIRTY_ANNOTATE_COPY) != 0 {
                inc = 2;
                num_clips /= 2;
            }

            // kzalloc_objs(*rects, num_clips) == kcalloc(num_clips, sizeof(*rects), GFP_KERNEL)
            rects = match (num_clips as usize).checked_mul(size_of::<DrmModeRect>()) {
                Some(bytes) => tk_linux_kapi::memory::kzalloc(bytes, GFP_KERNEL) as *mut DrmModeRect,
                None => core::ptr::null_mut(),
            };
            if rects.is_null() {
                ret = -ENOMEM;
                prep_ok = false;
            } else {
                convert_clip_rect_to_rect(clips, rects, num_clips, inc);
                damage = drm_property_create_blob(
                    (*fb).dev,
                    num_clips as usize * size_of::<DrmModeRect>(),
                    rects as *const c_void,
                );
                if IS_ERR(damage) {
                    ret = PTR_ERR(damage);
                    damage = core::ptr::null_mut();
                    prep_ok = false;
                }
            }
        }

        if prep_ok {
            // retry:
            loop {
                let mut loop_ret: i32 = 0;
                let mut lock_failed = false;
                drm_for_each_plane((*fb).dev, |plane| {
                    loop_ret = drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), (*state).acquire_ctx);
                    if loop_ret != 0 {
                        lock_failed = true;
                        return false; // goto out
                    }

                    if (*(*plane).state).fb != fb {
                        drm_modeset_unlock(core::ptr::addr_of_mut!((*plane).mutex));
                        return true; // continue
                    }

                    let plane_state = drm_atomic_get_plane_state(state, plane);
                    if IS_ERR(plane_state) {
                        loop_ret = PTR_ERR(plane_state);
                        lock_failed = true;
                        return false; // goto out
                    }

                    drm_property_replace_blob(core::ptr::addr_of_mut!((*plane_state).fb_damage_clips), damage);
                    true
                });

                ret = if lock_failed {
                    loop_ret
                } else {
                    drm_atomic_commit(state)
                };

                // out:
                if ret == -EDEADLK {
                    drm_atomic_commit_clear(state);
                    ret = drm_modeset_backoff(ctx_ptr);
                    if ret == 0 {
                        continue;
                    }
                }
                break;
            }
        }

        // out:
        drm_property_blob_put(damage);
        tk_linux_kapi::memory::kfree(rects);
        drm_atomic_commit_put(state);

        // out_drop_locks:
        drm_modeset_drop_locks(ctx_ptr);
        drm_modeset_acquire_fini(ctx_ptr);
        ret
    }
}

/// Initializes the damage iterator for a plane update.
// upstream: drm_damage_helper.c drm_atomic_helper_damage_iter_init()
pub unsafe fn drm_atomic_helper_damage_iter_init(
    iter: *mut DrmAtomicHelperDamageIter,
    old_state: *const DrmPlaneState,
    state: *const DrmPlaneState,
) {
    unsafe {
        core::ptr::write_bytes(iter, 0, 1);

        if state.is_null() || (*state).crtc.is_null() || (*state).fb.is_null() || !(*state).visible {
            return;
        }

        (*iter).clips = crate::drm_plane_upstream::drm_plane_get_damage_clips(state) as *mut DrmRect;
        (*iter).num_clips = crate::drm_plane_upstream::drm_plane_get_damage_clips_count(state);

        // Round down for x1/y1 and round up for x2/y2 to catch all pixels.
        let src = drm_plane_state_src(state);
        (*iter).plane_src.x1 = src.x1 >> 16;
        (*iter).plane_src.y1 = src.y1 >> 16;
        (*iter).plane_src.x2 = (src.x2 >> 16) + ((src.x2 & 0xFFFF) != 0) as i32;
        (*iter).plane_src.y2 = (src.y2 >> 16) + ((src.y2 & 0xFFFF) != 0) as i32;

        // Short-circuits like the C condition: old_state is only read when needed.
        if (*iter).clips.is_null()
            || (*state).ignore_damage_clips
            || !drm_rect_equals(&(*state).src, &(*old_state).src)
        {
            (*iter).clips = core::ptr::null_mut();
            (*iter).num_clips = 0;
            (*iter).full_update = true;
        }
    }
}

/// Returns the next damage rectangle intersecting the plane, or the whole
/// plane once for a full update.
// upstream: drm_damage_helper.c drm_atomic_helper_damage_iter_next()
pub unsafe fn drm_atomic_helper_damage_iter_next(iter: *mut DrmAtomicHelperDamageIter, rect: *mut DrmRect) -> bool {
    unsafe {
        let mut ret = false;

        if (*iter).full_update {
            *rect = core::ptr::read(core::ptr::addr_of!((*iter).plane_src));
            (*iter).full_update = false;
            return true;
        }

        while (*iter).curr_clip < (*iter).num_clips {
            *rect = core::ptr::read((*iter).clips.add((*iter).curr_clip as usize));
            (*iter).curr_clip += 1;
            if drm_rect_intersect(rect, core::ptr::addr_of!((*iter).plane_src)) {
                ret = true;
                break;
            }
        }
        ret
    }
}

/// Bounding box of all damage clips of a plane; false when there is none.
// upstream: drm_damage_helper.c drm_atomic_helper_damage_merged()
pub unsafe fn drm_atomic_helper_damage_merged(
    old_state: *const DrmPlaneState,
    state: *const DrmPlaneState,
    rect: *mut DrmRect,
) -> bool {
    unsafe {
        let mut iter: DrmAtomicHelperDamageIter = zeroed();
        let mut clip: DrmRect = zeroed();
        let mut valid = false;

        (*rect).x1 = i32::MAX;
        (*rect).y1 = i32::MAX;
        (*rect).x2 = 0;
        (*rect).y2 = 0;

        drm_atomic_helper_damage_iter_init(&mut iter, old_state, state);
        while drm_atomic_helper_damage_iter_next(&mut iter, &mut clip) {
            (*rect).x1 = (*rect).x1.min(clip.x1);
            (*rect).y1 = (*rect).y1.min(clip.y1);
            (*rect).x2 = (*rect).x2.max(clip.x2);
            (*rect).y2 = (*rect).y2.max(clip.y2);
            valid = true;
        }
        valid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_crtc_types::DrmCrtc;
    use crate::drm_plane_types::DrmPlane;
    use crate::drm_property_types::{DrmProperty, DrmPropertyBlob};
    use crate::test_support::TestDrm;
    use crate::uapi_drm_mode_types::DrmModeRect;
    use core::mem::zeroed;
    use std::boxed::Box;
    use std::vec::Vec;

    /// A plane on a device with a `prop_fb_damage_clips` property, plus a
    /// plane state that is visible on a CRTC and a framebuffer.
    struct Fixture {
        _drm: TestDrm,
        plane: *mut DrmPlane,
        crtc: *mut DrmCrtc,
        fb: *mut DrmFramebuffer,
    }

    impl Fixture {
        fn new() -> Self {
            let mut drm = TestDrm::new(0);
            let dev = drm.ptr();
            unsafe {
                let prop: &'static mut DrmProperty = Box::leak(Box::new(zeroed()));
                (*dev).mode_config.prop_fb_damage_clips = prop;
                let plane: *mut DrmPlane = Box::into_raw(Box::new(zeroed()));
                (*plane).dev = dev;
                // No properties attached: drm_mode_obj_find_prop_id() finds nothing.
                let props: &'static mut crate::drm_mode_object_types::DrmObjectProperties =
                    Box::leak(Box::new(zeroed()));
                (*plane).base.properties = props;
                Self {
                    _drm: drm,
                    plane,
                    crtc: 0x1000 as *mut DrmCrtc,
                    fb: 0x2000 as *mut DrmFramebuffer,
                }
            }
        }

        /// A visible state covering a 64x32 source rectangle.
        fn state(&self) -> *mut DrmPlaneState {
            unsafe {
                let s: *mut DrmPlaneState = Box::into_raw(Box::new(zeroed()));
                (*s).plane = self.plane;
                (*s).crtc = self.crtc;
                (*s).fb = self.fb;
                (*s).visible = true;
                (*s).src_w = 64 << 16;
                (*s).src_h = 32 << 16;
                (*s).src = DrmRect { x1: 0, y1: 0, x2: 64, y2: 32 };
                s
            }
        }

        /// Attaches `clips` as the state's FB_DAMAGE_CLIPS blob.
        fn set_clips(&self, s: *mut DrmPlaneState, clips: Vec<DrmModeRect>) {
            unsafe {
                let data: &'static mut [DrmModeRect] = Box::leak(clips.into_boxed_slice());
                let blob: &'static mut DrmPropertyBlob = Box::leak(Box::new(zeroed()));
                blob.length = core::mem::size_of_val(data);
                blob.data = data.as_mut_ptr() as *mut core::ffi::c_void;
                (*s).fb_damage_clips = blob;
            }
        }
    }

    fn rect(x1: i32, y1: i32, x2: i32, y2: i32) -> DrmModeRect {
        DrmModeRect { x1, y1, x2, y2 }
    }

    #[test]
    fn full_update_yields_the_plane_source_once() {
        let fx = Fixture::new();
        unsafe {
            let new = fx.state();
            let old = fx.state();
            let mut iter: DrmAtomicHelperDamageIter = zeroed();
            drm_atomic_helper_damage_iter_init(&mut iter, old, new);
            assert!(iter.full_update);
            assert_eq!(iter.num_clips, 0);

            let mut r: DrmRect = zeroed();
            assert!(drm_atomic_helper_damage_iter_next(&mut iter, &mut r));
            assert_eq!((r.x1, r.y1, r.x2, r.y2), (0, 0, 64, 32));
            assert!(!drm_atomic_helper_damage_iter_next(&mut iter, &mut r));
        }
    }

    #[test]
    fn clip_iteration_skips_clips_outside_the_plane() {
        let fx = Fixture::new();
        unsafe {
            let new = fx.state();
            fx.set_clips(new, std::vec![rect(0, 0, 10, 10), rect(100, 100, 200, 200), rect(4, 4, 6, 6)]);
            let old = fx.state();

            let mut iter: DrmAtomicHelperDamageIter = zeroed();
            drm_atomic_helper_damage_iter_init(&mut iter, old, new);
            assert!(!iter.full_update);
            assert_eq!(iter.num_clips, 3);

            let mut got = Vec::new();
            let mut r: DrmRect = zeroed();
            while drm_atomic_helper_damage_iter_next(&mut iter, &mut r) {
                got.push((r.x1, r.y1, r.x2, r.y2));
            }
            // The middle clip lies outside the 64x32 plane and is dropped.
            assert_eq!(got, std::vec![(0, 0, 10, 10), (4, 4, 6, 6)]);
        }
    }

    #[test]
    fn merged_damage_is_the_bounding_box_of_the_clips() {
        let fx = Fixture::new();
        unsafe {
            let new = fx.state();
            fx.set_clips(new, std::vec![rect(2, 3, 10, 12), rect(8, 1, 20, 5)]);
            let old = fx.state();
            let mut merged: DrmRect = zeroed();
            assert!(drm_atomic_helper_damage_merged(old, new, &mut merged));
            assert_eq!((merged.x1, merged.y1, merged.x2, merged.y2), (2, 1, 20, 12));
        }
    }

    #[test]
    fn clip_count_reports_blob_entries() {
        let fx = Fixture::new();
        unsafe {
            let s = fx.state();
            assert_eq!(crate::drm_plane_upstream::drm_plane_get_damage_clips_count(s), 0);
            fx.set_clips(s, std::vec![rect(0, 0, 1, 1), rect(1, 1, 2, 2)]);
            assert_eq!(crate::drm_plane_upstream::drm_plane_get_damage_clips_count(s), 2);
        }
    }
}
