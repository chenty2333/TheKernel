// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_blend.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. Property creation and
//! atomic state lookup are owned by other batches; they are declared
//! `extern "C"` here and resolved through `crate::pending_stubs` until
//! those files land.

use core::ffi::c_char;

use alloc::format;
use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{ENOMEM, EINVAL, IS_ERR, PTR_ERR};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_device_types::DrmDevice;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};
use crate::drm_property_types::{DrmProperty, DrmPropEnumList};
use crate::uapi_drm_consts::*;

// upstream: include/drm/drm_blend.h
pub const DRM_BLEND_ALPHA_OPAQUE: u32 = 0xffff;
pub const DRM_MODE_BLEND_PREMULTI: u32 = 0;
pub const DRM_MODE_BLEND_COVERAGE: u32 = 1;
pub const DRM_MODE_BLEND_PIXEL_NONE: u32 = 2;

// Property and atomic helpers owned by other batches (see pending_stubs/).
unsafe extern "C" {
    fn drm_property_create_range(
        dev: *mut DrmDevice,
        flags: u32,
        name: *const c_char,
        min: u64,
        max: u64,
    ) -> *mut DrmProperty;
    fn drm_property_create(dev: *mut DrmDevice, flags: u32, name: *const c_char, num_values: i32) -> *mut DrmProperty;
    fn drm_property_create_bitmask(
        dev: *mut DrmDevice,
        flags: u32,
        name: *const c_char,
        props: *const DrmPropEnumList,
        num_props: i32,
        supported_bits: u64,
    ) -> *mut DrmProperty;
    fn drm_property_add_enum(property: *mut DrmProperty, value: u64, name: *const c_char) -> i32;
    fn drm_property_destroy(dev: *mut DrmDevice, property: *mut DrmProperty);
    fn drm_object_attach_property(obj: *mut DrmModeObject, property: *mut DrmProperty, init_val: u64);
    fn drm_atomic_get_plane_state(state: *mut DrmAtomicCommit, plane: *mut DrmPlane) -> *mut DrmPlaneState;
}

// `drm_dbg_atomic()` routed to the TheKernel log.
fn drm_dbg_atomic(dev: *mut DrmDevice, msg: &str) {
    let _ = dev;
    drm_log_at(DrmLogLevel::Debug, "drm_atomic", file!(), line!(), msg);
}

// upstream: include/drm/drm_blend.h BIT(), __builtin_ffs()
#[inline]
fn ffs(x: u32) -> u32 {
    if x == 0 { 0 } else { x.trailing_zeros() + 1 }
}

// Name of a `char *name` field for log output.
fn cstr_name<'a>(name: *const c_char) -> &'a str {
    if name.is_null() {
        return "";
    }
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    unsafe { core::ffi::CStr::from_ptr(name) }.to_str().unwrap_or("")
}

// upstream: include/linux/log2.h is_power_of_2()
#[inline]
fn is_power_of_2(x: u32) -> bool {
    x != 0 && (x & (x - 1)) == 0
}

// upstream: drm_crtc/drm_plane zpos_changed bit-field (bit 4 of the first byte)
#[inline]
fn crtc_state_zpos_changed_set(s: *mut DrmCrtcState, v: bool) {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    unsafe {
        let b = &mut (*s)._bitfields_10[0];
        if v { *b |= 1 << 4 } else { *b &= !(1 << 4) }
    }
}

// upstream: drm_blend.c drm_plane_create_alpha_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_alpha_property(plane: *mut DrmPlane) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let plane = unsafe { &mut *plane };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe { drm_property_create_range(plane.dev, 0, c"alpha".as_ptr(), 0, DRM_BLEND_ALPHA_OPAQUE as u64) };
    if prop.is_null() {
        return -ENOMEM;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut plane.base, prop, DRM_BLEND_ALPHA_OPAQUE as u64) };
    plane.alpha_property = prop;
    if !plane.state.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*plane.state).alpha = DRM_BLEND_ALPHA_OPAQUE as u16 };
    }
    0
}

// upstream: drm_blend.c drm_plane_create_rotation_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_rotation_property(
    plane: *mut DrmPlane,
    rotation: u32,
    supported_rotations: u32,
) -> i32 {
    // __builtin_ffs(X) - 1
    let props: [DrmPropEnumList; 6] = [
        DrmPropEnumList { r#type: (ffs(DRM_MODE_ROTATE_0) - 1) as i32, name: c"rotate-0".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: (ffs(DRM_MODE_ROTATE_90) - 1) as i32, name: c"rotate-90".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: (ffs(DRM_MODE_ROTATE_180) - 1) as i32, name: c"rotate-180".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: (ffs(DRM_MODE_ROTATE_270) - 1) as i32, name: c"rotate-270".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: (ffs(DRM_MODE_REFLECT_X) - 1) as i32, name: c"reflect-x".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: (ffs(DRM_MODE_REFLECT_Y) - 1) as i32, name: c"reflect-y".as_ptr() as *mut c_char },
    ];
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let plane_ref = unsafe { &mut *plane };

    warn_on((supported_rotations & DRM_MODE_ROTATE_MASK) == 0);
    warn_on(!is_power_of_2(rotation & DRM_MODE_ROTATE_MASK));
    warn_on(rotation & !supported_rotations != 0);

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe {
        drm_property_create_bitmask(
            plane_ref.dev,
            0,
            c"rotation".as_ptr(),
            props.as_ptr(),
            props.len() as i32,
            supported_rotations as u64,
        )
    };
    if prop.is_null() {
        return -ENOMEM;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut plane_ref.base, prop, rotation as u64) };
    if !plane_ref.state.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*plane_ref.state).rotation = rotation };
    }
    plane_ref.rotation_property = prop;
    0
}

// upstream: drm_blend.c drm_rotation_simplify()
#[unsafe(no_mangle)]
pub extern "C" fn drm_rotation_simplify(mut rotation: u32, supported_rotations: u32) -> u32 {
    if rotation & !supported_rotations != 0 {
        rotation ^= DRM_MODE_REFLECT_X | DRM_MODE_REFLECT_Y;
        rotation = (rotation & DRM_MODE_REFLECT_MASK)
            | (1u32 << ((ffs(rotation & DRM_MODE_ROTATE_MASK) + 1) % 4));
    }
    rotation
}

// upstream: drm_blend.c drm_plane_create_zpos_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_zpos_property(
    plane: *mut DrmPlane,
    zpos: u32,
    min: u32,
    max: u32,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let plane = unsafe { &mut *plane };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe { drm_property_create_range(plane.dev, 0, c"zpos".as_ptr(), min as u64, max as u64) };
    if prop.is_null() {
        return -ENOMEM;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut plane.base, prop, zpos as u64) };
    plane.zpos_property = prop;
    if !plane.state.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*plane.state).zpos = zpos;
            (*plane.state).normalized_zpos = zpos;
        }
    }
    0
}

// upstream: drm_blend.c drm_plane_create_zpos_immutable_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_zpos_immutable_property(plane: *mut DrmPlane, zpos: u32) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let plane = unsafe { &mut *plane };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe {
        drm_property_create_range(
            plane.dev,
            DRM_MODE_PROP_IMMUTABLE,
            c"zpos".as_ptr(),
            zpos as u64,
            zpos as u64,
        )
    };
    if prop.is_null() {
        return -ENOMEM;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut plane.base, prop, zpos as u64) };
    plane.zpos_property = prop;
    if !plane.state.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*plane.state).zpos = zpos;
            (*plane.state).normalized_zpos = zpos;
        }
    }
    0
}

// upstream: drm_blend.c drm_atomic_commit_zpos_cmp()
// The C comparator returns `sa->zpos - sb->zpos` as int; zpos fits in the
// positive range for all valid planes, so the wrapping difference matches.
fn drm_atomic_commit_zpos_cmp(sa: *const DrmPlaneState, sb: *const DrmPlaneState) -> core::cmp::Ordering {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    let (sa, sb) = unsafe { (&*sa, &*sb) };
    if sa.zpos != sb.zpos {
        let d = (sa.zpos as i32).wrapping_sub(sb.zpos as i32);
        return d.cmp(&0);
    }
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    let (ia, ib) = unsafe { ((*sa.plane).base.id, (*sb.plane).base.id) };
    (ia as i32).wrapping_sub(ib as i32).cmp(&0)
}

// upstream: drm_blend.c drm_atomic_helper_crtc_normalize_zpos()
unsafe fn drm_atomic_helper_crtc_normalize_zpos(crtc: *mut DrmCrtc, crtc_state: *mut DrmCrtcState) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let state = unsafe { (*crtc_state).state };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*crtc).dev };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let total_planes = unsafe { (*dev).mode_config.num_total_plane };
    let mut ret = 0;

    drm_dbg_atomic(
        dev,
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        &format!("[CRTC:{}:{}] calculating normalized zpos values", unsafe { (*crtc).base.id }, cstr_name(unsafe { (*crtc).name })),
    );

    // kmalloc_objs(*states, total_planes)
    let states = tk_linux_kapi::memory::kmalloc(
        core::mem::size_of::<*mut DrmPlaneState>() * total_planes.max(0) as usize,
        tk_linux_kapi::config::GFP_KERNEL,
    ) as *mut *mut DrmPlaneState;
    if states.is_null() {
        return -ENOMEM;
    }

    let mut n: usize = 0;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let plane_mask = unsafe { (*crtc_state).plane_mask };
    // drm_for_each_plane_mask(plane, dev, mask)
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    'planes: while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let plane = unsafe { link.cast::<u8>().sub(core::mem::offset_of!(DrmPlane, head)) as *mut DrmPlane };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if plane_mask & (1u32 << unsafe { (*plane).index }) == 0 {
            continue;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let plane_state = unsafe { drm_atomic_get_plane_state(state, plane) };
        if IS_ERR(plane_state) {
            ret = PTR_ERR(plane_state);
            break 'planes;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { *states.add(n) = plane_state };
        n += 1;
        drm_dbg_atomic(
            dev,
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            &format!("[PLANE:{}:{}] processing zpos value {}", unsafe { (*plane).base.id }, cstr_name(unsafe { (*plane).name }), unsafe { (*plane_state).zpos }),
        );
    }

    if ret == 0 {
        // sort(states, n, sizeof(*states), drm_atomic_commit_zpos_cmp, NULL)
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let slice = unsafe { core::slice::from_raw_parts_mut(states, n) };
        slice.sort_by(|a, b| drm_atomic_commit_zpos_cmp(*a, *b));

        for i in 0..n {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let st = unsafe { *states.add(i) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let plane = unsafe { (*st).plane };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*st).normalized_zpos = i as u32 };
            drm_dbg_atomic(
                dev,
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                &format!("[PLANE:{}:{}] normalized zpos value {}", unsafe { (*plane).base.id }, cstr_name(unsafe { (*plane).name }), i),
            );
        }
        crtc_state_zpos_changed_set(crtc_state, true);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { tk_linux_kapi::memory::kfree(states) };
    ret
}

// upstream: drm_blend.c drm_atomic_normalize_zpos()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_atomic_normalize_zpos(_dev: *mut DrmDevice, state: *mut DrmAtomicCommit) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let state_ref = unsafe { &*state };
    let sdev = state_ref.dev;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_total_plane = unsafe { (*sdev).mode_config.num_total_plane };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_crtc = unsafe { (*sdev).mode_config.num_crtc };
    let ret = 0;

    // for_each_oldnew_plane_in_state()
    for i in 0..num_total_plane.max(0) as usize {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let entry = unsafe { &*state_ref.planes.add(i) };
        if entry.ptr.is_null() {
            continue;
        }
        let plane = entry.ptr;
        let old_plane_state = entry.old_state;
        let new_plane_state = entry.new_state;
        let _ = plane;
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let crtc = unsafe { (*new_plane_state).crtc };
        if crtc.is_null() {
            continue;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*old_plane_state).zpos != (*new_plane_state).zpos } {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let new_crtc_state = unsafe { drm_atomic_get_new_crtc_state_inline(state, crtc) };
            crtc_state_zpos_changed_set(new_crtc_state, true);
        }
    }

    // for_each_oldnew_crtc_in_state()
    for i in 0..num_crtc.max(0) as usize {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let entry = unsafe { &*state_ref.crtcs.add(i) };
        if entry.ptr.is_null() {
            continue;
        }
        let crtc = entry.ptr;
        let old_crtc_state = entry.old_state;
        let new_crtc_state = entry.new_state;
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let zpos_changed = unsafe { ((*new_crtc_state)._bitfields_10[0] >> 4) & 1 != 0 };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*old_crtc_state).plane_mask != (*new_crtc_state).plane_mask } || zpos_changed {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let ret = unsafe { drm_atomic_helper_crtc_normalize_zpos(crtc, new_crtc_state) };
            if ret != 0 {
                return ret;
            }
        }
    }

    ret
}

// upstream: drm_atomic.h drm_atomic_get_new_crtc_state() (header inline,
// indexed by crtc->index)
unsafe fn drm_atomic_get_new_crtc_state_inline(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> *mut DrmCrtcState {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let index = unsafe { (*crtc).index } as usize;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*(*state).crtcs.add(index)).new_state }
}

// upstream: drm_blend.c drm_plane_create_blend_mode_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_blend_mode_property(plane: *mut DrmPlane, supported_modes: u32) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*plane).dev };
    let props: [DrmPropEnumList; 3] = [
        DrmPropEnumList { r#type: DRM_MODE_BLEND_PIXEL_NONE as i32, name: c"None".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: DRM_MODE_BLEND_PREMULTI as i32, name: c"Pre-multiplied".as_ptr() as *mut c_char },
        DrmPropEnumList { r#type: DRM_MODE_BLEND_COVERAGE as i32, name: c"Coverage".as_ptr() as *mut c_char },
    ];
    let valid_mode_mask: u32 = (1 << DRM_MODE_BLEND_PIXEL_NONE)
        | (1 << DRM_MODE_BLEND_PREMULTI)
        | (1 << DRM_MODE_BLEND_COVERAGE);

    if warn_on((supported_modes & !valid_mode_mask) != 0
        || (supported_modes & (1 << DRM_MODE_BLEND_PREMULTI)) == 0)
    {
        return -EINVAL;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe {
        drm_property_create(dev, 0, c"pixel blend mode".as_ptr(), supported_modes.count_ones() as i32)
    };
    if prop.is_null() {
        return -ENOMEM;
    }

    for p in props.iter() {
        if (1u32 << p.r#type) & supported_modes == 0 {
            continue;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let ret = unsafe { drm_property_add_enum(prop, p.r#type as u64, p.name) };
        if ret != 0 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_property_destroy(dev, prop) };
            return ret;
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut (*plane).base, prop, DRM_MODE_BLEND_PREMULTI as u64) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*plane).blend_mode_property = prop };
    0
}

// upstream: drm_blend.c drm_crtc_attach_background_color_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_crtc_attach_background_color_property(crtc: *mut DrmCrtc) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let crtc = unsafe { &mut *crtc };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let prop = unsafe { (*crtc.dev).mode_config.background_color_property };
    // DRM_ARGB64_PREP(0xffff, 0, 0, 0): __DRM_ARGB64_PREP(alpha, 48) | red/green/blue (zero)
    let value: u64 = 0xffffu64 << 48;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_object_attach_property(&mut crtc.base as *mut DrmModeObject, prop, value) };
}
