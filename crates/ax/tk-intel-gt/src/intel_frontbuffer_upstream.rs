// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
// Source: drivers/gpu/drm/i915/display/intel_frontbuffer.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::sync::atomic::{AtomicI32, Ordering};

use tk_drm_upstream::drm_device_types::DrmDevice;

use crate::{
    intel_display_core_types_upstream::{IntelDisplay, IntelFrontbufferTracking},
    intel_frontbuffer_types_upstream::IntelFrontbuffer,
    intel_parent_upstream::{
        intel_parent_frontbuffer_flush_for_display, intel_parent_frontbuffer_put,
        intel_parent_frontbuffer_ref,
    },
    linux::{
        locks::{spin_lock, spin_unlock},
        workqueue::{INIT_WORK_C, system_dfl_wq, try_queue_work},
    },
};

pub const ORIGIN_CPU: i32 = 0;
pub const ORIGIN_CS: i32 = 1;
pub const ORIGIN_FLIP: i32 = 2;
pub const ORIGIN_DIRTYFB: i32 = 3;
pub const ORIGIN_CURSOR_UPDATE: i32 = 4;

pub const INTEL_FRONTBUFFER_BITS_PER_PIPE: u32 = 8;

#[inline]
pub const fn intel_frontbuffer_bit(pipe: u32, plane_id: u32) -> u32 {
    1u32.wrapping_shl(plane_id + INTEL_FRONTBUFFER_BITS_PER_PIPE * pipe)
}

#[inline]
pub const fn intel_frontbuffer_overlay_bit(pipe: u32) -> u32 {
    1u32.wrapping_shl(INTEL_FRONTBUFFER_BITS_PER_PIPE - 1 + INTEL_FRONTBUFFER_BITS_PER_PIPE * pipe)
}

#[inline]
pub const fn intel_frontbuffer_all_mask(pipe: u32) -> u32 {
    ((1u32.wrapping_shl(INTEL_FRONTBUFFER_BITS_PER_PIPE * (pipe + 1))) - 1)
        & !((1u32.wrapping_shl(INTEL_FRONTBUFFER_BITS_PER_PIPE * pipe)) - 1)
}

#[inline]
fn filter_busy_flush(frontbuffer_bits: u32, busy_bits: u32) -> u32 {
    frontbuffer_bits & !busy_bits
}

#[inline]
fn complete_flush(origin: i32, frontbuffer_bits: u32, busy_bits: u32) -> (u32, u32) {
    if origin == ORIGIN_CS {
        let bits = frontbuffer_bits & busy_bits;
        (bits, busy_bits & !bits)
    } else {
        (frontbuffer_bits, busy_bits)
    }
}

#[inline]
fn atomic_bits(bits: &tk_linux_kapi::types::AtomicT) -> &AtomicI32 {
    // `atomic_t` and `AtomicI32` are both a single aligned 32-bit counter.
    unsafe { &*(core::ptr::addr_of!(bits.counter).cast::<AtomicI32>()) }
}

#[inline]
fn atomic_bits_mut(bits: &mut tk_linux_kapi::types::AtomicT) -> &AtomicI32 {
    unsafe { &*(core::ptr::addr_of_mut!(bits.counter).cast::<AtomicI32>()) }
}

unsafe fn flush_tracked(display: *mut IntelDisplay, frontbuffer_bits: u32, origin: i32) {
    if display.is_null() || frontbuffer_bits == 0 {
        return;
    }

    let tracking = unsafe { core::ptr::addr_of_mut!((*display).fb_tracking) };
    unsafe { spin_lock(&mut (*tracking).lock) };
    let busy_bits = unsafe { (*tracking).busy_bits };
    unsafe { spin_unlock(&mut (*tracking).lock) };
    let frontbuffer_bits = filter_busy_flush(frontbuffer_bits, busy_bits);
    if frontbuffer_bits == 0 {
        return;
    }

    // Linux dispatches these notifications to FBC/PSR/DRRS/trace consumers
    // here. Those optional modules are not part of this batch; the shared
    // busy-bit/frontbuffer state and parent flush handoff below are real.
    let _ = origin;
}

// Header-inline intel_frontbuffer_invalidate().
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_invalidate(
    front: *mut IntelFrontbuffer,
    origin: i32,
) -> bool {
    if front.is_null() {
        return false;
    }
    let bits = atomic_bits(unsafe { &(*front).bits }).load(Ordering::Relaxed) as u32;
    if bits == 0 {
        return false;
    }
    unsafe { __intel_frontbuffer_invalidate(front, origin, bits) };
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_frontbuffer_invalidate(
    front: *mut IntelFrontbuffer,
    origin: i32,
    frontbuffer_bits: u32,
) {
    if front.is_null() || frontbuffer_bits == 0 {
        return;
    }
    let display = unsafe { (*front).display };
    if display.is_null() {
        return;
    }
    if origin == ORIGIN_CS {
        let tracking = unsafe { core::ptr::addr_of_mut!((*display).fb_tracking) };
        unsafe { spin_lock(&mut (*tracking).lock) };
        unsafe { (*tracking).busy_bits |= frontbuffer_bits };
        unsafe { spin_unlock(&mut (*tracking).lock) };
    }
    // Optional FBC/PSR/DRRS invalidation consumers are compiled out until
    // their feature batches land; invalidation bookkeeping remains active.
}

// Header-inline intel_frontbuffer_flush().
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_flush(front: *mut IntelFrontbuffer, origin: i32) {
    if front.is_null() {
        return;
    }
    let bits = atomic_bits(unsafe { &(*front).bits }).load(Ordering::Relaxed) as u32;
    if bits != 0 {
        unsafe { __intel_frontbuffer_flush(front, origin, bits) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_frontbuffer_flush(
    front: *mut IntelFrontbuffer,
    origin: i32,
    mut frontbuffer_bits: u32,
) {
    if front.is_null() || frontbuffer_bits == 0 {
        return;
    }
    let display = unsafe { (*front).display };
    if display.is_null() {
        return;
    }

    if origin == ORIGIN_DIRTYFB {
        unsafe { intel_parent_frontbuffer_flush_for_display(display, front.cast()) };
    }

    if origin == ORIGIN_CS {
        let tracking = unsafe { core::ptr::addr_of_mut!((*display).fb_tracking) };
        unsafe { spin_lock(&mut (*tracking).lock) };
        let (bits, busy) =
            complete_flush(origin, frontbuffer_bits, unsafe { (*tracking).busy_bits });
        frontbuffer_bits = bits;
        unsafe { (*tracking).busy_bits = busy };
        unsafe { spin_unlock(&mut (*tracking).lock) };
    }

    if frontbuffer_bits != 0 {
        unsafe { flush_tracked(display, frontbuffer_bits, origin) };
    }
}

unsafe extern "C" fn frontbuffer_flush_work(work: *mut tk_linux_kapi::types::WorkStruct) {
    if work.is_null() {
        return;
    }
    let front = container_of!(work, IntelFrontbuffer, flush_work);
    unsafe {
        intel_frontbuffer_flush(front, ORIGIN_DIRTYFB);
        intel_parent_frontbuffer_put((*front).display, front.cast());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_flip(display: *mut IntelDisplay, frontbuffer_bits: u32) {
    if display.is_null() || frontbuffer_bits == 0 {
        return;
    }
    let tracking = unsafe { core::ptr::addr_of_mut!((*display).fb_tracking) };
    unsafe { spin_lock(&mut (*tracking).lock) };
    unsafe { (*tracking).busy_bits &= !frontbuffer_bits };
    unsafe { spin_unlock(&mut (*tracking).lock) };
    unsafe { flush_tracked(display, frontbuffer_bits, ORIGIN_FLIP) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_queue_flush(front: *mut IntelFrontbuffer) {
    if front.is_null() {
        return;
    }
    let display = unsafe { (*front).display };
    if display.is_null() {
        return;
    }
    unsafe { intel_parent_frontbuffer_ref(display, front.cast()) };
    let queued =
        unsafe { try_queue_work(system_dfl_wq, core::ptr::addr_of_mut!((*front).flush_work)) }
            .unwrap_or(false);
    if !queued {
        unsafe { intel_parent_frontbuffer_put(display, front.cast()) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_init(front: *mut IntelFrontbuffer, drm: *mut DrmDevice) {
    if front.is_null() {
        return;
    }
    unsafe {
        (*front).display = crate::intel_display_conversion_upstream::__drm_to_display(drm);
        atomic_bits_mut(&mut (*front).bits).store(0, Ordering::Relaxed);
        INIT_WORK_C(&mut (*front).flush_work, frontbuffer_flush_work);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_fini(front: *mut IntelFrontbuffer) {
    if front.is_null() {
        return;
    }
    let bits = atomic_bits(unsafe { &(*front).bits }).load(Ordering::Relaxed);
    let _ = tk_linux_kapi::assert::warn_on(bits != 0);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_frontbuffer_track(
    old: *mut IntelFrontbuffer,
    new: *mut IntelFrontbuffer,
    frontbuffer_bits: u32,
) {
    if frontbuffer_bits == 0 {
        return;
    }
    if !old.is_null() {
        let old_bits = atomic_bits(unsafe { &(*old).bits });
        let previous = old_bits.fetch_and(!(frontbuffer_bits as i32), Ordering::Relaxed) as u32;
        let _ = tk_linux_kapi::assert::warn_on(previous & frontbuffer_bits != frontbuffer_bits);
    }
    if !new.is_null() {
        let new_bits = atomic_bits(unsafe { &(*new).bits });
        let previous = new_bits.fetch_or(frontbuffer_bits as i32, Ordering::Relaxed) as u32;
        let _ = tk_linux_kapi::assert::warn_on(previous & frontbuffer_bits != 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_filters_busy_planes_and_cs_retirement_only_clears_its_bits() {
        assert_eq!(filter_busy_flush(0b1111, 0b0101), 0b1010);
        assert_eq!(complete_flush(ORIGIN_CS, 0b1111, 0b0101), (0b0101, 0));
        assert_eq!(complete_flush(ORIGIN_CPU, 0b1111, 0b0101), (0b1111, 0b0101));
    }

    #[test]
    fn frontbuffer_masks_follow_eight_slots_per_pipe() {
        assert_eq!(intel_frontbuffer_bit(1, 3), 1 << 11);
        assert_eq!(intel_frontbuffer_overlay_bit(2), 1 << 23);
        assert_eq!(intel_frontbuffer_all_mask(1), 0x0000_ff00);
    }
}
