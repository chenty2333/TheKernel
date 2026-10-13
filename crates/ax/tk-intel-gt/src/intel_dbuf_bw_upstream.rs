// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Translation of Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dbuf_bw.c.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::c_void,
    mem::{offset_of, size_of},
    ptr,
};

use tk_drm_upstream::{
    drm_crtc_types::DrmCrtcState, drm_device_types::DrmDevice,
    drm_modeset_lock_types::DrmModesetAcquireCtx,
};
use tk_linux_kapi::{
    config::GFP_KERNEL,
    list,
    memory::{kfree, kzalloc_obj},
    types::ListHead,
};

use crate::{
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelCrtcState},
    intel_global_state_types_upstream::{IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs},
    intel_wm_types_upstream::SklDdbEntry,
};

const I915_MAX_PIPES: usize = 4;
const I915_MAX_PLANES: usize = 8;
const I915_MAX_DBUF_SLICES: usize = 4;
const PLANE_CURSOR: usize = 7;
const ENOMEM: i32 = 12;

#[repr(C)]
#[derive(Clone, Copy)]
struct IntelDbufBw {
    max_bw: [u32; I915_MAX_DBUF_SLICES],
    active_planes: [u8; I915_MAX_DBUF_SLICES],
}

#[repr(C)]
pub struct IntelDbufBwState {
    base: IntelGlobalState,
    dbuf_bw: [IntelDbufBw; I915_MAX_PIPES],
}

unsafe extern "C" {
    fn intel_cdclk_update_dbuf_bw_min_cdclk(
        state: *mut IntelAtomicState,
        old_min_cdclk: i32,
        new_min_cdclk: i32,
        need_cdclk_calc: *mut bool,
    ) -> i32;
}

#[inline]
fn is_err<T>(ptr: *const T) -> bool {
    (ptr as usize) >= (!4095usize)
}

#[inline]
unsafe fn display_from_atomic(state: *mut IntelAtomicState) -> *mut IntelDisplay {
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*state).base.dev) }
}

#[inline]
unsafe fn display_from_crtc(crtc: *mut IntelCrtc) -> *mut IntelDisplay {
    if crtc.is_null() {
        return ptr::null_mut();
    }
    unsafe { __drm_to_display((*crtc).base.dev) }
}

#[inline]
unsafe fn display_from_crtc_state(state: *const IntelCrtcState) -> *mut IntelDisplay {
    if state.is_null() || unsafe { (*state).uapi.crtc.is_null() } {
        return ptr::null_mut();
    }
    let drm = unsafe { (*(*state).uapi.crtc).dev };
    unsafe { __drm_to_display(drm) }
}

#[inline]
unsafe fn display_ver(display: *const IntelDisplay) -> u16 {
    unsafe { (*display).info.__runtime_info.ip.ver }
}

#[inline]
unsafe fn active_pipe_mask(display: *const IntelDisplay) -> u8 {
    unsafe { (*display).info.__runtime_info.pipe_mask }
}

#[inline]
unsafe fn active_dbuf_slice_mask(display: *const IntelDisplay) -> u8 {
    let info = unsafe { (*display).info.__device_info };
    if info.is_null() {
        0
    } else {
        unsafe { (*info).dbuf.slice_mask }
    }
}

#[inline]
unsafe fn ddb_dbuf_slice_mask(display: *const IntelDisplay, ddb: *const SklDdbEntry) -> u8 {
    if display.is_null() || ddb.is_null() || unsafe { (*ddb).start >= (*ddb).end } {
        return 0;
    }
    let info = unsafe { (*display).info.__device_info };
    if info.is_null() {
        return 0;
    }
    let slice_mask = unsafe { (*info).dbuf.slice_mask };
    let dbuf_size = unsafe { (*info).dbuf.size };
    let slice_count = (slice_mask & 0x0f).count_ones();
    if slice_count == 0 || dbuf_size == 0 {
        return 0;
    }
    let slice_size = dbuf_size as u32 / slice_count;
    if slice_size == 0 {
        return 0;
    }
    let start_slice = (*ddb).start as u32 / slice_size;
    let end_slice = ((*ddb).end as u32 - 1) / slice_size;
    let mut mask = 0u8;
    let mut slice = start_slice;
    while slice <= end_slice && slice < I915_MAX_DBUF_SLICES as u32 {
        mask |= 1 << slice;
        slice += 1;
    }
    mask
}

fn dbuf_bw_changed(old: &IntelDbufBw, new: &IntelDbufBw, slices: u8) -> bool {
    (0..I915_MAX_DBUF_SLICES).any(|slice| {
        slices & (1 << slice) != 0
            && (old.max_bw[slice] != new.max_bw[slice]
                || old.active_planes[slice] != new.active_planes[slice])
    })
}

unsafe fn dbuf_state_changed(
    display: *mut IntelDisplay,
    old_state: *const IntelDbufBwState,
    new_state: *const IntelDbufBwState,
) -> bool {
    if display.is_null() || old_state.is_null() || new_state.is_null() {
        return false;
    }
    let pipes = unsafe { active_pipe_mask(display) };
    let slices = unsafe { active_dbuf_slice_mask(display) };
    (0..I915_MAX_PIPES).any(|pipe| {
        pipes & (1 << pipe) != 0
            && dbuf_bw_changed(
                unsafe { &(*old_state).dbuf_bw[pipe] },
                unsafe { &(*new_state).dbuf_bw[pipe] },
                slices,
            )
    })
}

unsafe fn calc_plane_dbuf_bw(
    display: *mut IntelDisplay,
    dbuf_bw: &mut IntelDbufBw,
    plane_id: usize,
    ddb: *const SklDdbEntry,
    data_rate: u32,
) {
    let dbuf_mask = unsafe { ddb_dbuf_slice_mask(display, ddb) };
    let slices = unsafe { active_dbuf_slice_mask(display) };
    for slice in 0..I915_MAX_DBUF_SLICES {
        let bit = 1u8 << slice;
        if slices & dbuf_mask & bit == 0 {
            continue;
        }
        // DDB guarantees a plane can span a contiguous slice range. The
        // arbiter's guarantee is an equal share per active plane.
        dbuf_bw.max_bw[slice] = dbuf_bw.max_bw[slice].max(data_rate);
        dbuf_bw.active_planes[slice] |= 1 << plane_id;
    }
}

unsafe fn calc_crtc_dbuf_bw(dbuf_bw: *mut IntelDbufBw, crtc_state: *const IntelCrtcState) {
    if dbuf_bw.is_null() {
        return;
    }
    unsafe { ptr::write_bytes(dbuf_bw.cast::<u8>(), 0, size_of::<IntelDbufBw>()) };
    if crtc_state.is_null() || !unsafe { (*crtc_state).hw.active } {
        return;
    }
    let display = unsafe { display_from_crtc_state(crtc_state) };
    if display.is_null() {
        return;
    }
    let crtc_ptr = unsafe { (*crtc_state).uapi.crtc };
    if crtc_ptr.is_null() {
        return;
    }
    // IntelCrtc embeds drm_crtc at offset zero, matching to_intel_crtc().
    let crtc = crtc_ptr.cast::<IntelCrtc>();
    let plane_mask = unsafe { (*crtc).plane_ids_mask };
    for plane in 0..I915_MAX_PLANES {
        if plane == PLANE_CURSOR || plane_mask & (1 << plane) == 0 {
            continue;
        }
        let wm = unsafe { &(*crtc_state).wm._anon_0_0.skl };
        unsafe {
            calc_plane_dbuf_bw(
                display,
                &mut *dbuf_bw,
                plane,
                ptr::addr_of!(wm.plane_ddb[plane]),
                (*crtc_state).data_rate[plane],
            );
            if display_ver(display) < 11 {
                calc_plane_dbuf_bw(
                    display,
                    &mut *dbuf_bw,
                    plane,
                    ptr::addr_of!(wm.plane_ddb_y[plane]),
                    (*crtc_state).data_rate_y[plane],
                );
            }
        }
    }
}

// upstream: intel_dbuf_bw.c to_intel_dbuf_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn to_intel_dbuf_bw_state(
    obj_state: *mut IntelGlobalState,
) -> *mut IntelDbufBwState {
    obj_state.cast()
}

// upstream: intel_dbuf_bw.c intel_atomic_get_old_dbuf_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_old_dbuf_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelDbufBwState {
    if state.is_null() {
        return ptr::null_mut();
    }
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return ptr::null_mut();
    }
    let base = unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_old_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).dbuf_bw.obj),
        )
    };
    unsafe { to_intel_dbuf_bw_state(base) }
}

// upstream: intel_dbuf_bw.c intel_atomic_get_new_dbuf_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_new_dbuf_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelDbufBwState {
    if state.is_null() {
        return ptr::null_mut();
    }
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return ptr::null_mut();
    }
    let base = unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_new_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).dbuf_bw.obj),
        )
    };
    unsafe { to_intel_dbuf_bw_state(base) }
}

// upstream: intel_dbuf_bw.c intel_atomic_get_dbuf_bw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_get_dbuf_bw_state(
    state: *mut IntelAtomicState,
) -> *mut IntelDbufBwState {
    if state.is_null() {
        return ptr::null_mut();
    }
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return ptr::null_mut();
    }
    let base = unsafe {
        crate::intel_global_state_upstream::intel_atomic_get_global_obj_state(
            state,
            ptr::addr_of_mut!((*display).dbuf_bw.obj),
        )
    };
    if is_err(base) {
        return base.cast();
    }
    unsafe { to_intel_dbuf_bw_state(base) }
}

// upstream: intel_dbuf_bw.c intel_dbuf_bw_min_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_min_cdclk(
    display: *mut IntelDisplay,
    state: *const IntelDbufBwState,
) -> i32 {
    if display.is_null() || state.is_null() {
        return 0;
    }
    let pipes = unsafe { active_pipe_mask(display) };
    let slices = unsafe { active_dbuf_slice_mask(display) };
    let mut total_max_bw = 0u32;
    for slice in 0..I915_MAX_DBUF_SLICES {
        if slices & (1 << slice) == 0 {
            continue;
        }
        let mut num_active_planes = 0u32;
        let mut max_bw = 0u32;
        for pipe in 0..I915_MAX_PIPES {
            if pipes & (1 << pipe) == 0 {
                continue;
            }
            let dbuf_bw = unsafe { &(*state).dbuf_bw[pipe] };
            max_bw = max_bw.max(dbuf_bw.max_bw[slice]);
            num_active_planes += (dbuf_bw.active_planes[slice]).count_ones();
        }
        total_max_bw = total_max_bw.max(max_bw.saturating_mul(num_active_planes));
    }
    ((total_max_bw + 63) / 64) as i32
}

// upstream: intel_dbuf_bw.c intel_dbuf_bw_calc_min_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_calc_min_cdclk(
    state: *mut IntelAtomicState,
    need_cdclk_calc: *mut bool,
) -> i32 {
    if state.is_null() || need_cdclk_calc.is_null() {
        return -ENOMEM;
    }
    let display = unsafe { display_from_atomic(state) };
    if display.is_null() {
        return -ENOMEM;
    }
    if unsafe { display_ver(display) } < 9 {
        return 0;
    }
    let drm = unsafe { (*state).base.dev };
    if drm.is_null() {
        return -ENOMEM;
    }
    let num_crtcs = unsafe { (*drm).num_crtcs as usize };
    let crtc_entries = unsafe { (*state).base.crtcs };
    let mut new_state: *mut IntelDbufBwState = ptr::null_mut();
    let mut old_state: *const IntelDbufBwState = ptr::null();
    for index in 0..num_crtcs {
        if crtc_entries.is_null() {
            break;
        }
        let entry = unsafe { &*crtc_entries.add(index) };
        if entry.ptr.is_null() {
            continue;
        }
        let crtc = entry.ptr.cast::<IntelCrtc>();
        let mut old_bw = IntelDbufBw {
            max_bw: [0; I915_MAX_DBUF_SLICES],
            active_planes: [0; I915_MAX_DBUF_SLICES],
        };
        let mut new_bw = old_bw;
        unsafe {
            calc_crtc_dbuf_bw(&mut old_bw, entry.old_state.cast::<IntelCrtcState>());
            calc_crtc_dbuf_bw(&mut new_bw, entry.new_state.cast::<IntelCrtcState>());
        }
        if !dbuf_bw_changed(&old_bw, &new_bw, unsafe { active_dbuf_slice_mask(display) }) {
            continue;
        }
        if new_state.is_null() {
            new_state = unsafe { intel_atomic_get_dbuf_bw_state(state) };
            if is_err(new_state) {
                return new_state as isize as i32;
            }
            old_state = unsafe { intel_atomic_get_old_dbuf_bw_state(state) };
        }
        let pipe = unsafe { (*crtc).pipe };
        if !(0..I915_MAX_PIPES as i32).contains(&pipe) {
            return -ENOMEM;
        }
        unsafe { (*new_state).dbuf_bw[pipe as usize] = new_bw };
    }
    if old_state.is_null() {
        return 0;
    }
    if unsafe { dbuf_state_changed(display, old_state, new_state) } {
        let ret = unsafe {
            crate::intel_global_state_upstream::intel_atomic_lock_global_state(ptr::addr_of_mut!(
                (*new_state).base
            ))
        };
        if ret != 0 {
            return ret;
        }
    }
    unsafe {
        intel_cdclk_update_dbuf_bw_min_cdclk(
            state,
            intel_dbuf_bw_min_cdclk(display, old_state),
            intel_dbuf_bw_min_cdclk(display, new_state),
            need_cdclk_calc,
        )
    }
}

// upstream: intel_dbuf_bw.c intel_dbuf_bw_update_hw_state()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_update_hw_state(display: *mut IntelDisplay) {
    if display.is_null() || unsafe { display_ver(display) } < 9 {
        return;
    }
    let state = unsafe { (*display).dbuf_bw.obj.state.cast::<IntelDbufBwState>() };
    if state.is_null() {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*display).pipe_list) };
    let mut pos = unsafe { (*head).next };
    while !ptr::eq(pos, head) {
        let next = unsafe { (*pos).next };
        let crtc = unsafe {
            pos.cast::<u8>()
                .sub(offset_of!(IntelCrtc, pipe_head))
                .cast::<IntelCrtc>()
        };
        let pipe = unsafe { (*crtc).pipe };
        if (0..I915_MAX_PIPES as i32).contains(&pipe) {
            unsafe {
                calc_crtc_dbuf_bw(
                    ptr::addr_of_mut!((*state).dbuf_bw[pipe as usize]),
                    (*crtc).base.state.cast::<IntelCrtcState>(),
                )
            };
        }
        pos = next;
    }
}

// upstream: intel_dbuf_bw.c intel_dbuf_bw_crtc_disable_noatomic()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_crtc_disable_noatomic(crtc: *mut IntelCrtc) {
    let display = unsafe { display_from_crtc(crtc) };
    if display.is_null() || unsafe { display_ver(display) } < 9 {
        return;
    }
    let state = unsafe { (*display).dbuf_bw.obj.state.cast::<IntelDbufBwState>() };
    let pipe = unsafe { (*crtc).pipe };
    if !state.is_null() && (0..I915_MAX_PIPES as i32).contains(&pipe) {
        unsafe {
            ptr::write_bytes(
                ptr::addr_of_mut!((*state).dbuf_bw[pipe as usize]).cast::<u8>(),
                0,
                size_of::<IntelDbufBw>(),
            )
        };
    }
}

unsafe extern "C" fn dbuf_bw_duplicate_state(obj: *mut IntelGlobalObj) -> *mut IntelGlobalState {
    if obj.is_null() || unsafe { (*obj).state.is_null() } {
        return ptr::null_mut();
    }
    let state = kzalloc_obj::<IntelDbufBwState>();
    if state.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping((*obj).state.cast::<IntelDbufBwState>(), state, 1);
    }
    state.cast()
}

unsafe extern "C" fn dbuf_bw_destroy_state(
    _obj: *mut IntelGlobalObj,
    state: *mut IntelGlobalState,
) {
    unsafe { kfree(state) };
}

static INTEL_DBUF_BW_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
    atomic_duplicate_state: Some(dbuf_bw_duplicate_state),
    atomic_destroy_state: Some(dbuf_bw_destroy_state),
};

// upstream: intel_dbuf_bw.c intel_dbuf_bw_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() {
        return -ENOMEM;
    }
    let state = kzalloc_obj::<IntelDbufBwState>();
    if state.is_null() {
        return -ENOMEM;
    }
    unsafe {
        crate::intel_global_state_upstream::intel_atomic_global_obj_init(
            display,
            ptr::addr_of_mut!((*display).dbuf_bw.obj),
            ptr::addr_of_mut!((*state).base),
            ptr::addr_of!(INTEL_DBUF_BW_FUNCS),
        )
    };
    0
}

#[cfg(test)]
mod tests {
    use super::{
        IntelDbufBw, IntelDbufBwState, dbuf_bw_changed, ddb_dbuf_slice_mask,
        intel_dbuf_bw_min_cdclk,
    };
    use crate::{
        intel_display_core_types_upstream::IntelDisplay,
        intel_display_device_upstream::XE_LPD_DISPLAY, intel_wm_types_upstream::SklDdbEntry,
    };

    #[test]
    fn changed_slices_are_scoped_to_enabled_dbuffers() {
        let old = IntelDbufBw {
            max_bw: [1, 2, 3, 4],
            active_planes: [1, 1, 0, 0],
        };
        let mut new = old;
        new.max_bw[2] = 99;
        assert!(!dbuf_bw_changed(&old, &new, 0b0011));
        assert!(dbuf_bw_changed(&old, &new, 0b0100));
    }

    #[test]
    fn per_slice_state_records_plane_membership_independently() {
        let mut state = IntelDbufBw {
            max_bw: [0; 4],
            active_planes: [0; 4],
        };
        state.max_bw[1] = 1234;
        state.active_planes[1] |= 1 << 2;
        assert_eq!(state.max_bw, [0, 1234, 0, 0]);
        assert_eq!(state.active_planes[1].count_ones(), 1);
    }

    #[test]
    fn ddb_slice_intersections_use_half_open_boundaries() {
        let mut display: IntelDisplay = unsafe { core::mem::zeroed() };
        display.info.__device_info = (&XE_LPD_DISPLAY
            as *const crate::intel_display_device_types_upstream::IntelDisplayDeviceInfo)
            .cast_mut();
        let first = SklDdbEntry {
            start: 0,
            end: 1024,
        };
        let cross = SklDdbEntry {
            start: 1023,
            end: 1025,
        };
        let next = SklDdbEntry {
            start: 1024,
            end: 2048,
        };
        assert_eq!(unsafe { ddb_dbuf_slice_mask(&display, &first) }, 0b0001);
        assert_eq!(unsafe { ddb_dbuf_slice_mask(&display, &cross) }, 0b0011);
        assert_eq!(unsafe { ddb_dbuf_slice_mask(&display, &next) }, 0b0010);
    }

    #[test]
    fn minimum_cdclk_uses_the_maximum_equal_share_per_active_slice() {
        let mut display: IntelDisplay = unsafe { core::mem::zeroed() };
        display.info.__device_info = (&XE_LPD_DISPLAY
            as *const crate::intel_display_device_types_upstream::IntelDisplayDeviceInfo)
            .cast_mut();
        display.info.__runtime_info.pipe_mask = 0b0011;
        let mut state: IntelDbufBwState = unsafe { core::mem::zeroed() };
        state.dbuf_bw[0].max_bw[0] = 100;
        state.dbuf_bw[0].active_planes[0] = 0b0001;
        state.dbuf_bw[1].max_bw[0] = 80;
        state.dbuf_bw[1].active_planes[0] = 0b0011;
        assert_eq!(unsafe { intel_dbuf_bw_min_cdclk(&mut display, &state) }, 5);
    }
}
