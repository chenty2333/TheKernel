// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_plane.c` (batch B1c).
//!
//! Universal plane init and managed allocation, IN_FORMATS/format-modifier
//! blobs, the plane state helpers, damage clip accessors, the legacy
//! GETPLANERESOURCES/GETPLANE/SETPLANE/CURSOR/PAGE_FLIP ioctls and the
//! property helpers. Functions are named and ordered as in the C file.
//!
//! Deviations (documented in the batch report):
//! - `name` arguments of the init functions are the already formatted plane
//!   name (C variadic formatting is not expressible in this ABI); NULL selects
//!   "plane-%d".

// The Linux ports state the invariant of each C-ABI call once in the module
// header, as the GT ports do; per-block SAFETY prose is not repeated.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of, zeroed};

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{
    EBUSY, EDEADLK, EFAULT, EINVAL, ENOENT, ENOMEM, ENXIO, EOPNOTSUPP, ERANGE, GFP_KERNEL,
    IS_ERR, PTR_ERR,
};

/// `EACCES` (errno 13; not present in the kapi errno table).
const EACCES: i32 = 13;
use tk_linux_kapi::list::{list_add_tail, list_del};
use tk_linux_kapi::types::ListHead;

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_color_mgmt_upstream::{drm_core_check_feature, drm_drv_uses_atomic_modeset};
use crate::drm_crtc_types::DrmCrtc;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_framebuffer_types::DrmFramebuffer;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_plane_types::{
    DRM_PLANE_TYPE_CURSOR, DRM_PLANE_TYPE_OVERLAY, DRM_SCALING_FILTER_DEFAULT,
    DRM_SCALING_FILTER_NEAREST_NEIGHBOR,
    DrmPlane, DrmPlaneFuncs, DrmPlaneState,
};
use crate::drm_file_types::DrmPendingEvent;
use crate::drm_vblank_types::DrmPendingVblankEvent;
use crate::drm_property_types::DrmPropEnumList;
use crate::uapi_drm_mode_types::{
    DrmModeCrtcPageFlipTarget, DrmModeCursor, DrmModeCursor2, DrmModeFbCmd2, DrmModeGetPlane,
    DrmModeGetPlaneRes, DrmModeRect, DrmModeSetPlane, DrmPlaneSizeHint,
};
use crate::uapi_drm_types::{DrmEvent, DrmEventVblank};
use crate::drm_property_types::{DrmProperty, DrmPropertyBlob};
use crate::uapi_drm_consts::{
    DRM_EVENT_FLIP_COMPLETE, DRM_FORMAT_ARGB8888, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR,
    DRM_MODE_CURSOR_BO, DRM_MODE_CURSOR_FLAGS, DRM_MODE_CURSOR_MOVE, DRM_MODE_OBJECT_PLANE,
    DRM_MODE_PAGE_FLIP_ASYNC, DRM_MODE_PAGE_FLIP_EVENT, DRM_MODE_PAGE_FLIP_FLAGS,
    DRM_MODE_PAGE_FLIP_TARGET, DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE,
    DRM_MODE_PAGE_FLIP_TARGET_RELATIVE, DRM_MODE_PROP_ATOMIC, DRM_MODE_PROP_ENUM,
};
use crate::uapi_drm_mode_types::{DrmFormatModifier, DrmFormatModifierBlob};



unsafe extern "C" {
    // upstream: drm_mode_object.c
    fn drm_mode_object_get(obj: *mut DrmModeObject);
    fn drm_mode_object_put(obj: *mut DrmModeObject);
    // upstream: drm_framebuffer.c
    fn drm_framebuffer_lookup(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32) -> *mut DrmFramebuffer;
    fn drm_framebuffer_check_src_coords(src_x: u32, src_y: u32, src_w: u32, src_h: u32, fb: *const DrmFramebuffer) -> i32;
    fn drm_internal_framebuffer_create(dev: *mut DrmDevice, mode_cmd: *const DrmModeFbCmd2, file_priv: *mut DrmFile) -> *mut DrmFramebuffer;
    // upstream: drm_modeset_lock.c
    fn drm_modeset_acquire_init(ctx: *mut DrmModesetAcquireCtx, flags: u32);
    fn drm_modeset_acquire_fini(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_drop_locks(ctx: *mut DrmModesetAcquireCtx);
    fn drm_modeset_backoff(ctx: *mut DrmModesetAcquireCtx) -> i32;
    // upstream: drm_lease.c
    fn drm_lease_held(file_priv: *mut DrmFile, id: i32) -> bool;
    fn drm_lease_filter_crtcs(file_priv: *mut DrmFile, crtcs: u32) -> u32;
    // upstream: drm_file.c
    fn drm_event_reserve_init(dev: *mut DrmDevice, file_priv: *mut DrmFile, p: *mut DrmPendingEvent, e: *mut DrmEvent) -> i32;
    fn drm_event_cancel_free(dev: *mut DrmDevice, p: *mut DrmPendingEvent);
    // upstream: drm_property.c
    fn drm_property_create(dev: *mut DrmDevice, flags: u32, name: *const c_char, num_values: i32) -> *mut DrmProperty;
    fn drm_property_add_enum(property: *mut DrmProperty, value: u64, name: *const c_char) -> i32;
}

/// `DRIVER_ATOMIC` / `DRIVER_CURSOR_HOTSPOT` (include/drm/drm_drv.h).
const DRIVER_ATOMIC: u32 = 1 << 4;
const DRIVER_CURSOR_HOTSPOT: u32 = 1 << 6;
/// `DRIVER_MODESET` (include/drm/drm_drv.h).
const DRIVER_MODESET: u32 = 1 << 1;

// upstream: include/uapi/drm/drm.h event-union accessors (ManuallyDrop union fields)
#[inline]
unsafe fn ev_base(e: *mut DrmPendingVblankEvent) -> *mut DrmEvent {
    unsafe { core::ptr::addr_of_mut!((*e).event.base) as *mut DrmEvent }
}

#[inline]
unsafe fn ev_vbl(e: *mut DrmPendingVblankEvent) -> *mut DrmEventVblank {
    unsafe { core::ptr::addr_of_mut!((*e).event.vbl) as *mut DrmEventVblank }
}
/// `FORMAT_BLOB_CURRENT` (include/drm/drm_plane.h).
const FORMAT_BLOB_CURRENT: u32 = 1;

unsafe extern "C" {
    // upstream: drm_mode_object.c
    fn drm_mode_object_add(dev: *mut DrmDevice, obj: *mut DrmModeObject, obj_type: u32) -> i32;
    fn drm_mode_object_unregister(dev: *mut DrmDevice, obj: *mut DrmModeObject);
    fn drm_mode_object_find(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32, obj_type: u32) -> *mut DrmModeObject;
    fn drm_object_attach_property(obj: *mut DrmModeObject, property: *mut DrmProperty, init_val: u64);
    fn drm_object_property_set_value(obj: *mut DrmModeObject, property: *mut DrmProperty, val: u64) -> i32;
    fn drm_mode_obj_find_prop_id(obj: *mut DrmModeObject, prop_id: u32) -> *mut DrmProperty;
    // upstream: drm_property.c
    fn drm_property_create_blob(dev: *mut DrmDevice, length: usize, data: *const c_void) -> *mut DrmPropertyBlob;
    fn drm_property_create_signed_range(dev: *mut DrmDevice, flags: u32, name: *const c_char, min: i64, max: i64) -> *mut DrmProperty;
    fn drm_property_create_range(dev: *mut DrmDevice, flags: u32, name: *const c_char, min: u64, max: u64) -> *mut DrmProperty;
    fn drm_property_create_enum(dev: *mut DrmDevice, flags: u32, name: *const c_char, props: *const crate::drm_property_types::DrmPropEnumList, num_values: i32) -> *mut DrmProperty;
    fn drm_property_destroy(dev: *mut DrmDevice, property: *mut DrmProperty);
    // upstream: drm_modeset_lock.c
    fn drm_modeset_lock_init(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock);
    fn drm_modeset_lock(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock, ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_modeset_unlock(lock: *mut crate::drm_modeset_lock_types::DrmModesetLock);
    // upstream: drm_atomic.c
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
}

/// Copies a NUL-terminated plane name into a fresh kernel allocation.
// upstream: drm_plane.c kvasprintf()/kstrdup() result for the plane name
unsafe fn kstrdup_plane_name(src: *const c_char) -> *mut c_char {
    unsafe {
        let mut len = 0usize;
        while *src.add(len) != 0 {
            len += 1;
        }
        let dst = tk_linux_kapi::memory::kzalloc(len + 1, GFP_KERNEL) as *mut c_char;
        if !dst.is_null() {
            core::ptr::copy_nonoverlapping(src, dst, len);
        }
        dst
    }
}

/// Formats "plane-%d" (the default name) into a fresh kernel allocation.
// upstream: drm_plane.c kasprintf(GFP_KERNEL, "plane-%d", drm_num_planes(dev))
unsafe fn kasprintf_plane_default(num: u32) -> *mut c_char {
    unsafe {
        let mut buf = [0u8; 24];
        let prefix = b"plane-";
        buf[..prefix.len()].copy_from_slice(prefix);
        let mut len = prefix.len();
        let mut digits = [0u8; 10];
        let mut n = num;
        let mut nd = 0usize;
        loop {
            digits[nd] = b'0' + (n % 10) as u8;
            nd += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        while nd > 0 {
            nd -= 1;
            buf[len] = digits[nd];
            len += 1;
        }
        let dst = tk_linux_kapi::memory::kzalloc(len + 1, GFP_KERNEL) as *mut c_char;
        if !dst.is_null() {
            for i in 0..len {
                *dst.add(i) = buf[i] as c_char;
            }
        }
        dst
    }
}

/// `drm_num_planes()`: number of planes registered on `dev`.
// upstream: drm_plane.c drm_num_planes()
unsafe fn drm_num_planes(dev: *mut DrmDevice) -> u32 {
    unsafe {
        let mut num: u32 = 0;
        drm_for_each_plane(dev, |_| {
            num += 1;
            true
        });
        num
    }
}

/// Iterates `dev->mode_config.plane_list` (drm_for_each_plane()).
// upstream: include/drm/drm_plane.h drm_for_each_plane()
pub(crate) unsafe fn drm_for_each_plane(dev: *mut DrmDevice, mut f: impl FnMut(*mut DrmPlane) -> bool) {
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

// upstream: drm_plane.c formats_ptr()
unsafe fn formats_ptr(blob: *mut DrmFormatModifierBlob) -> *mut u32 {
    unsafe { (blob as *mut u8).add((*blob).formats_offset as usize) as *mut u32 }
}

// upstream: drm_plane.c modifiers_ptr()
unsafe fn modifiers_ptr(blob: *mut DrmFormatModifierBlob) -> *mut DrmFormatModifier {
    unsafe { (blob as *mut u8).add((*blob).modifiers_offset as usize) as *mut DrmFormatModifier }
}

// upstream: include/uapi/drm/drm_mode.h ALIGN(x, a)
#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Builds the IN_FORMATS blob: the plane's formats plus, per format-modifier,
/// the bitmask of formats it supports.
// upstream: drm_plane.c create_in_format_blob()
unsafe fn create_in_format_blob(
    dev: *mut DrmDevice,
    plane: *mut DrmPlane,
    format_mod_supported: Option<unsafe extern "C" fn(*mut DrmPlane, u32, u64) -> bool>,
) -> *mut DrmPropertyBlob {
    unsafe {
        let formats_size = size_of::<u32>() * (*plane).format_count as usize;
        if warn_on(formats_size == 0) {
            // 0 formats are never expected
            return tk_linux_kapi::config::ERR_PTR::<DrmPropertyBlob>(-EINVAL);
        }

        let modifiers_size = size_of::<DrmFormatModifier>() * (*plane).modifier_count as usize;
        let mut blob_size = size_of::<DrmFormatModifierBlob>();

        // Modifiers offset is a pointer to a struct with a 64 bit field so it
        // should be naturally aligned to 8B.
        blob_size += align_up(formats_size, 8);
        blob_size += modifiers_size;

        let blob = drm_property_create_blob(dev, blob_size, core::ptr::null());
        if IS_ERR(blob) {
            return blob;
        }

        let blob_data = (*blob).data as *mut DrmFormatModifierBlob;
        (*blob_data).version = FORMAT_BLOB_CURRENT;
        (*blob_data).count_formats = (*plane).format_count;
        (*blob_data).formats_offset = size_of::<DrmFormatModifierBlob>() as u32;
        (*blob_data).count_modifiers = (*plane).modifier_count;
        (*blob_data).modifiers_offset = align_up((*blob_data).formats_offset as usize + formats_size, 8) as u32;

        core::ptr::copy_nonoverlapping((*plane).format_types as *const u8, formats_ptr(blob_data) as *mut u8, formats_size);

        let mut md = modifiers_ptr(blob_data);
        for i in 0..(*plane).modifier_count as usize {
            for j in 0..(*plane).format_count as usize {
                let supported = match format_mod_supported {
                    None => true,
                    Some(f) => f(plane, *(*plane).format_types.add(j), *(*plane).modifiers.add(i)),
                };
                if supported {
                    (*md).formats |= 1u64 << j;
                }
            }
            (*md).modifier = *(*plane).modifiers.add(i);
            (*md).offset = 0;
            (*md).pad = 0;
            md = md.add(1);
        }

        blob
    }
}

/// Attaches the signed HOTSPOT_X/HOTSPOT_Y properties of a cursor plane.
// upstream: drm_plane.c drm_plane_create_hotspot_properties()
unsafe fn drm_plane_create_hotspot_properties(plane: *mut DrmPlane) -> i32 {
    unsafe {
        let dev = (*plane).dev;
        warn_on(!drm_core_check_feature(dev, DRIVER_CURSOR_HOTSPOT));

        let prop_x = drm_property_create_signed_range(dev, 0, b"HOTSPOT_X\0".as_ptr() as *const c_char, i32::MIN as i64, i32::MAX as i64);
        if prop_x.is_null() {
            return -ENOMEM;
        }

        let prop_y = drm_property_create_signed_range(dev, 0, b"HOTSPOT_Y\0".as_ptr() as *const c_char, i32::MIN as i64, i32::MAX as i64);
        if prop_y.is_null() {
            drm_property_destroy(dev, prop_x);
            return -ENOMEM;
        }

        drm_object_attach_property(core::ptr::addr_of_mut!((*plane).base), prop_x, 0);
        drm_object_attach_property(core::ptr::addr_of_mut!((*plane).base), prop_y, 0);
        (*plane).hotspot_x_property = prop_x;
        (*plane).hotspot_y_property = prop_y;
        0
    }
}

/// Shared body of the universal plane init functions.
// upstream: drm_plane.c __drm_universal_plane_init()
unsafe fn __drm_universal_plane_init(
    dev: *mut DrmDevice,
    plane: *mut DrmPlane,
    possible_crtcs: u32,
    funcs: *const DrmPlaneFuncs,
    formats: *const u32,
    format_count: u32,
    format_modifiers: *const u64,
    plane_type: u32,
    name: *const c_char,
) -> i32 {
    unsafe {
        use tk_linux_kapi::memory::kzalloc;

        let config = core::ptr::addr_of_mut!((*dev).mode_config);
        let default_modifiers: [u64; 1] = [DRM_FORMAT_MOD_LINEAR];
        let mut format_modifiers = format_modifiers;
        let mut format_modifier_count: u32 = 0;

        // plane index is used with 32bit bitmasks
        if warn_on((*config).num_total_plane >= 32) {
            return -EINVAL;
        }

        // First driver to need more than 64 formats needs to fix this. Each
        // format is encoded as a bit and the current code only supports a u64.
        if warn_on(format_count > 64) {
            return -EINVAL;
        }

        warn_on(
            drm_drv_uses_atomic_modeset(dev)
                && ((*funcs).atomic_destroy_state.is_none() || (*funcs).atomic_duplicate_state.is_none()),
        );

        let ret = drm_mode_object_add(dev, core::ptr::addr_of_mut!((*plane).base), DRM_MODE_OBJECT_PLANE);
        if ret != 0 {
            return ret;
        }

        drm_modeset_lock_init(core::ptr::addr_of_mut!((*plane).mutex));
        (*plane).base.properties = core::ptr::addr_of_mut!((*plane).properties);

        (*plane).dev = dev;
        (*plane).funcs = funcs as *mut DrmPlaneFuncs;

        // kmalloc_array(format_count, sizeof(uint32_t), GFP_KERNEL)
        (*plane).format_types = kzalloc(format_count as usize * size_of::<u32>(), GFP_KERNEL) as *mut u32;
        if (*plane).format_types.is_null() {
            // DRM_DEBUG_KMS("out of memory when allocating plane\n")
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*plane).base));
            return -ENOMEM;
        }

        if !format_modifiers.is_null() {
            let mut temp = format_modifiers;
            while *temp != DRM_FORMAT_MOD_INVALID {
                format_modifier_count += 1;
                temp = temp.add(1);
            }
        } else if !(*config).fb_modifiers_not_supported {
            format_modifiers = default_modifiers.as_ptr();
            format_modifier_count = default_modifiers.len() as u32;
        }

        // autoset the cap and check for consistency across all planes
        warn_on((*config).fb_modifiers_not_supported && format_modifier_count != 0);

        (*plane).modifier_count = format_modifier_count;
        (*plane).modifiers = kzalloc(format_modifier_count as usize * size_of::<u64>(), GFP_KERNEL) as *mut u64;
        if format_modifier_count != 0 && (*plane).modifiers.is_null() {
            // DRM_DEBUG_KMS("out of memory when allocating plane\n")
            tk_linux_kapi::memory::kfree((*plane).format_types);
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*plane).base));
            return -ENOMEM;
        }

        (*plane).name = if !name.is_null() {
            kstrdup_plane_name(name)
        } else {
            kasprintf_plane_default(drm_num_planes(dev))
        };

        if (*plane).name.is_null() {
            tk_linux_kapi::memory::kfree((*plane).format_types);
            tk_linux_kapi::memory::kfree((*plane).modifiers);
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*plane).base));
            return -ENOMEM;
        }

        core::ptr::copy_nonoverlapping(formats, (*plane).format_types, format_count as usize);
        (*plane).format_count = format_count;
        core::ptr::copy_nonoverlapping(format_modifiers, (*plane).modifiers, format_modifier_count as usize);

        (*plane).possible_crtcs = possible_crtcs;
        (*plane).r#type = plane_type;
        list_add_tail(
            core::ptr::addr_of_mut!((*plane).head),
            core::ptr::addr_of_mut!((*config).plane_list),
        );
        (*plane).index = (*config).num_total_plane as u32;
        (*config).num_total_plane += 1;

        let base = core::ptr::addr_of_mut!((*plane).base);
        drm_object_attach_property(base, (*config).plane_type_property, (*plane).r#type as u64);

        if drm_core_check_feature(dev, DRIVER_ATOMIC) {
            drm_object_attach_property(base, (*config).prop_fb_id, 0);
            drm_object_attach_property(base, (*config).prop_in_fence_fd, (-1i64) as u64);
            drm_object_attach_property(base, (*config).prop_crtc_id, 0);
            drm_object_attach_property(base, (*config).prop_crtc_x, 0);
            drm_object_attach_property(base, (*config).prop_crtc_y, 0);
            drm_object_attach_property(base, (*config).prop_crtc_w, 0);
            drm_object_attach_property(base, (*config).prop_crtc_h, 0);
            drm_object_attach_property(base, (*config).prop_src_x, 0);
            drm_object_attach_property(base, (*config).prop_src_y, 0);
            drm_object_attach_property(base, (*config).prop_src_w, 0);
            drm_object_attach_property(base, (*config).prop_src_h, 0);
        }

        if drm_core_check_feature(dev, DRIVER_CURSOR_HOTSPOT) && plane_type == DRM_PLANE_TYPE_CURSOR {
            drm_plane_create_hotspot_properties(plane);
        }

        if format_modifier_count != 0 {
            let blob = create_in_format_blob(dev, plane, (*funcs).format_mod_supported);
            if !IS_ERR(blob) {
                drm_object_attach_property(base, (*config).modifiers_property, (*blob).base.id as u64);
            }
        }

        if (*funcs).format_mod_supported_async.is_some() {
            let blob = create_in_format_blob(dev, plane, (*funcs).format_mod_supported_async);
            if !IS_ERR(blob) {
                drm_object_attach_property(base, (*config).async_modifiers_property, (*blob).base.id as u64);
            }
        }

        0
    }
}

/// Initializes a plane of the given type on `dev`.
/// `name` is the already formatted plane name (NULL selects "plane-%d").
// upstream: drm_plane.c drm_universal_plane_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_universal_plane_init(
    dev: *mut DrmDevice,
    plane: *mut DrmPlane,
    possible_crtcs: u32,
    funcs: *const DrmPlaneFuncs,
    formats: *const u32,
    format_count: u32,
    format_modifiers: *const u64,
    plane_type: u32,
    name: *const c_char,
) -> i32 {
    unsafe {
        warn_on((*funcs).destroy.is_none());
        __drm_universal_plane_init(
            dev,
            plane,
            possible_crtcs,
            funcs,
            formats,
            format_count,
            format_modifiers,
            plane_type,
            name,
        )
    }
}

/// Managed-release action: tears down a plane allocated by
/// [`__drmm_universal_plane_alloc`].
// upstream: drm_plane.c drmm_universal_plane_alloc_release()
unsafe extern "C" fn drmm_universal_plane_alloc_release(_dev: *mut c_void, ptr: *mut c_void) {
    unsafe {
        let plane = ptr as *mut DrmPlane;
        if warn_on((*plane).dev.is_null()) {
            return;
        }
        drm_plane_cleanup(plane);
    }
}

/// Allocates a managed container with the plane at `offset` and initializes it.
// upstream: drm_plane.c __drmm_universal_plane_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_universal_plane_alloc(
    dev: *mut DrmDevice,
    size: usize,
    offset: usize,
    possible_crtcs: u32,
    funcs: *const DrmPlaneFuncs,
    formats: *const u32,
    format_count: u32,
    format_modifiers: *const u64,
    plane_type: u32,
    name: *const c_char,
) -> *mut c_void {
    unsafe {
        if warn_on(funcs.is_null() || (*funcs).destroy.is_some()) {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(-EINVAL);
        }

        let container = tk_linux_kapi::memory::drmm_kzalloc_c(dev as *mut c_void, size, GFP_KERNEL);
        if container.is_null() {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(-ENOMEM);
        }

        let plane = (container as *mut u8).add(offset) as *mut DrmPlane;
        let ret = __drm_universal_plane_init(
            dev,
            plane,
            possible_crtcs,
            funcs,
            formats,
            format_count,
            format_modifiers,
            plane_type,
            name,
        );
        if ret != 0 {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(ret);
        }

        let ret = tk_linux_kapi::kernel_core::drmm_add_action_or_reset(
            dev as *mut c_void,
            drmm_universal_plane_alloc_release,
            plane as *mut c_void,
        );
        if ret != 0 {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(ret);
        }

        container
    }
}

/// Allocates a plain (unmanaged) container with the plane at `offset`.
/// Returns an error pointer on failure.
// upstream: drm_plane.c __drm_universal_plane_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drm_universal_plane_alloc(
    dev: *mut DrmDevice,
    size: usize,
    offset: usize,
    possible_crtcs: u32,
    funcs: *const DrmPlaneFuncs,
    formats: *const u32,
    format_count: u32,
    format_modifiers: *const u64,
    plane_type: u32,
    name: *const c_char,
) -> *mut c_void {
    unsafe {
        if warn_on(funcs.is_null()) {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(-EINVAL);
        }

        let container = tk_linux_kapi::memory::kzalloc(size, GFP_KERNEL);
        if container.is_null() {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(-ENOMEM);
        }

        let plane = (container as *mut u8).add(offset) as *mut DrmPlane;
        let ret = __drm_universal_plane_init(
            dev,
            plane,
            possible_crtcs,
            funcs,
            formats,
            format_count,
            format_modifiers,
            plane_type,
            name,
        );
        if ret != 0 {
            tk_linux_kapi::memory::kfree(container);
            return tk_linux_kapi::config::ERR_PTR::<c_void>(ret);
        }
        container
    }
}

/// Runs every plane's late_register hook and checks zpos consistency.
// upstream: drm_plane.c drm_plane_register_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_register_all(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mut num_planes: u32 = 0;
        let mut num_zpos: u32 = 0;
        let mut ret: i32 = 0;
        let mut err: Option<i32> = None;

        drm_for_each_plane(dev, |plane| {
            if let Some(late_register) = (*(*plane).funcs).late_register {
                ret = late_register(plane);
            }
            if ret != 0 {
                err = Some(ret);
                return false;
            }
            if !(*plane).zpos_property.is_null() {
                num_zpos += 1;
            }
            num_planes += 1;
            true
        });

        if let Some(e) = err {
            return e;
        }

        // drm_WARN(dev, num_zpos && num_planes != num_zpos, "Mixing planes ...")
        warn_on(num_zpos != 0 && num_planes != num_zpos);
        0
    }
}

/// Runs every plane's early_unregister hook.
// upstream: drm_plane.c drm_plane_unregister_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_unregister_all(dev: *mut DrmDevice) {
    unsafe {
        drm_for_each_plane(dev, |plane| {
            if let Some(early_unregister) = (*(*plane).funcs).early_unregister {
                early_unregister(plane);
            }
            true
        });
    }
}

/// Tears down a plane initialized by one of the init functions above.
// upstream: drm_plane.c drm_plane_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_cleanup(plane: *mut DrmPlane) {
    unsafe {
        let dev = (*plane).dev;

        // drm_modeset_lock_fini(&plane->mutex): WARN_ON(!list_empty(&lock->head))
        warn_on(!tk_linux_kapi::list::list_empty(&(*plane).mutex.head));

        tk_linux_kapi::memory::kfree((*plane).format_types);
        tk_linux_kapi::memory::kfree((*plane).modifiers);

        drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*plane).base));

        tk_linux_kapi::BUG_ON!(tk_linux_kapi::list::list_empty(&(*plane).head));

        // Note that the plane_list is considered to be static; should we remove
        // the drm_plane at runtime we would have to decrement all the indices on
        // the drm_plane after us in the plane_list.
        list_del(core::ptr::addr_of_mut!((*plane).head));
        (*dev).mode_config.num_total_plane -= 1;

        warn_on(!(*plane).state.is_null() && (*(*plane).funcs).atomic_destroy_state.is_none());
        if !(*plane).state.is_null() {
            if let Some(atomic_destroy_state) = (*(*plane).funcs).atomic_destroy_state {
                atomic_destroy_state(plane, (*plane).state);
            }
        }

        tk_linux_kapi::memory::kfree((*plane).name);

        // memset(plane, 0, sizeof(*plane))
        core::ptr::write_bytes(plane, 0, 1);
    }
}

/// Returns the plane with the given index, or null.
// upstream: drm_plane.c drm_plane_from_index()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_from_index(dev: *mut DrmDevice, idx: i32) -> *mut DrmPlane {
    unsafe {
        let mut found: *mut DrmPlane = core::ptr::null_mut();
        drm_for_each_plane(dev, |plane| {
            if idx == (*plane).index as i32 {
                found = plane;
                return false;
            }
            true
        });
        found
    }
}

/// Disables a plane whose framebuffer must be released (used on teardown).
// upstream: drm_plane.c drm_plane_force_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_force_disable(plane: *mut DrmPlane) {
    unsafe {
        if (*plane).fb.is_null() {
            return;
        }

        warn_on(drm_drv_uses_atomic_modeset((*plane).dev));

        (*plane).old_fb = (*plane).fb;
        let ret = (*(*plane).funcs).disable_plane.expect("disable_plane")(plane, core::ptr::null_mut());
        if ret != 0 {
            // DRM_ERROR("failed to disable plane with busy fb\n")
            (*plane).old_fb = core::ptr::null_mut();
            return;
        }

        // disconnect the plane from the fb and crtc:
        drm_mode_object_put(core::ptr::addr_of_mut!((*(*plane).old_fb).base));
        (*plane).old_fb = core::ptr::null_mut();
        (*plane).fb = core::ptr::null_mut();
        (*plane).crtc = core::ptr::null_mut();
    }
}

/// Sets a plane object property: the plane's `set_property` hook first, then
/// the stored value.
// upstream: drm_plane.c drm_mode_plane_set_obj_prop()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_plane_set_obj_prop(plane: *mut DrmPlane, property: *mut DrmProperty, value: u64) -> i32 {
    unsafe {
        let mut ret = -EINVAL;
        let obj = core::ptr::addr_of_mut!((*plane).base);
        if let Some(set_property) = (*(*plane).funcs).set_property {
            ret = set_property(plane, property, value);
        }
        if ret == 0 {
            drm_object_property_set_value(obj, property, value);
        }
        ret
    }
}

/// Looks up a plane from its userspace object ID.
// upstream: include/drm/drm_plane.h drm_plane_find()
pub(crate) unsafe fn drm_plane_find(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32) -> *mut DrmPlane {
    unsafe {
        let mo = drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_PLANE);
        if mo.is_null() {
            core::ptr::null_mut()
        } else {
            (mo as *mut u8).sub(offset_of!(DrmPlane, base)) as *mut DrmPlane
        }
    }
}

/// Legacy GETPLANERESOURCES ioctl: lists the plane IDs visible to the file.
// upstream: drm_plane.c drm_mode_getplane_res()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getplane_res(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let plane_resp = data as *mut DrmModeGetPlaneRes;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let plane_ptr = (*plane_resp).plane_id_ptr as usize as *mut u32;
        let mut count: u32 = 0;
        let mut err: Option<i32> = None;

        // This ioctl is called twice, once to determine how much space is needed,
        // and the 2nd time to fill it.
        drm_for_each_plane(dev, |plane| {
            // Unless userspace set the 'universal planes' capability bit, only
            // advertise overlays.
            if (*plane).r#type != DRM_PLANE_TYPE_OVERLAY && !(*file_priv).universal_planes {
                return true;
            }

            // If we're running on a virtualized driver then, unless userspace
            // advertizes support for the virtualized cursor plane, disable cursor
            // planes because they'll be broken due to missing cursor hotspot info.
            if (*plane).r#type == DRM_PLANE_TYPE_CURSOR
                && drm_core_check_feature(dev, DRIVER_CURSOR_HOTSPOT)
                && (*file_priv).atomic
                && !(*file_priv).supports_virtualized_cursor_plane
            {
                return true;
            }

            if drm_lease_held(file_priv, (*plane).base.id as i32) {
                if count < (*plane_resp).count_planes {
                    let id = (*plane).base.id;
                    if tk_linux_kapi::mm_native::copy_to_user(
                        plane_ptr.add(count as usize) as *mut c_void,
                        core::ptr::addr_of!(id) as *const c_void,
                        size_of::<u32>(),
                    ) != 0
                    {
                        err = Some(-EFAULT);
                        return false;
                    }
                }
                count += 1;
            }
            true
        });

        if let Some(e) = err {
            return e;
        }

        (*plane_resp).count_planes = count;
        0
    }
}

/// Legacy GETPLANE ioctl: reports a plane's CRTC, framebuffer, possible CRTCs
/// and supported formats.
// upstream: drm_plane.c drm_mode_getplane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getplane(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let plane_resp = data as *mut DrmModeGetPlane;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let plane = drm_plane_find(dev, file_priv, (*plane_resp).plane_id);
        if plane.is_null() {
            return -ENOENT;
        }

        drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), core::ptr::null_mut());
        if !(*plane).state.is_null() && !(*(*plane).state).crtc.is_null() && drm_lease_held(file_priv, (*(*(*plane).state).crtc).base.id as i32) {
            (*plane_resp).crtc_id = (*(*(*plane).state).crtc).base.id;
        } else if (*plane).state.is_null() && !(*plane).crtc.is_null() && drm_lease_held(file_priv, (*(*plane).crtc).base.id as i32) {
            (*plane_resp).crtc_id = (*(*plane).crtc).base.id;
        } else {
            (*plane_resp).crtc_id = 0;
        }

        if !(*plane).state.is_null() && !(*(*plane).state).fb.is_null() {
            (*plane_resp).fb_id = (*(*(*plane).state).fb).base.id;
        } else if (*plane).state.is_null() && !(*plane).fb.is_null() {
            (*plane_resp).fb_id = (*(*plane).fb).base.id;
        } else {
            (*plane_resp).fb_id = 0;
        }
        drm_modeset_unlock(core::ptr::addr_of_mut!((*plane).mutex));

        (*plane_resp).plane_id = (*plane).base.id;
        (*plane_resp).possible_crtcs = drm_lease_filter_crtcs(file_priv, (*plane).possible_crtcs);
        (*plane_resp).gamma_size = 0;

        // This ioctl is called twice, once to determine how much space is needed,
        // and the 2nd time to fill it.
        if (*plane).format_count != 0 && (*plane_resp).count_format_types >= (*plane).format_count {
            let format_ptr = (*plane_resp).format_type_ptr as usize as *mut c_void;
            if tk_linux_kapi::mm_native::copy_to_user(
                format_ptr,
                (*plane).format_types as *const c_void,
                size_of::<u32>() * (*plane).format_count as usize,
            ) != 0
            {
                return -EFAULT;
            }
        }

        (*plane_resp).count_format_types = (*plane).format_count;
        0
    }
}

/// True when `format` (and `modifier`) is supported by the plane.
// upstream: drm_plane.c drm_plane_has_format()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_has_format(plane: *mut DrmPlane, format: u32, modifier: u64) -> bool {
    unsafe {
        let mut i: u32 = 0;
        while i < (*plane).format_count {
            if format == *(*plane).format_types.add(i as usize) {
                break;
            }
            i += 1;
        }
        if i == (*plane).format_count {
            return false;
        }

        if let Some(format_mod_supported) = (*(*plane).funcs).format_mod_supported {
            if !format_mod_supported(plane, format, modifier) {
                return false;
            }
        } else {
            if (*plane).modifier_count == 0 {
                return true;
            }
            let mut j: u32 = 0;
            while j < (*plane).modifier_count {
                if modifier == *(*plane).modifiers.add(j as usize) {
                    break;
                }
                j += 1;
            }
            if j == (*plane).modifier_count {
                return false;
            }
        }
        true
    }
}

/// Checks a legacy plane update against the plane's CRTC mask, its format
/// list and the CRTC/source coordinates.
// upstream: drm_plane.c __setplane_check()
unsafe fn __setplane_check(
    plane: *mut DrmPlane,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
) -> i32 {
    unsafe {
        // Check whether this plane is usable on this CRTC
        if ((*plane).possible_crtcs & crate::drm_crtc_upstream::drm_crtc_mask(crtc)) == 0 {
            // DRM_DEBUG_KMS("Invalid crtc for plane\n")
            return -EINVAL;
        }

        // Check whether this plane supports the fb pixel format.
        if !drm_plane_has_format(plane, (*(*fb).format).format, (*fb).modifier) {
            // DRM_DEBUG_KMS("Invalid pixel format ...")
            return -EINVAL;
        }

        // Give drivers some help against integer overflows
        if crtc_w > i32::MAX as u32
            || crtc_x > i32::MAX - crtc_w as i32
            || crtc_h > i32::MAX as u32
            || crtc_y > i32::MAX - crtc_h as i32
        {
            // DRM_DEBUG_KMS("Invalid CRTC coordinates ...")
            return -ERANGE;
        }

        drm_framebuffer_check_src_coords(src_x, src_y, src_w, src_h, fb)
    }
}

/// True when any plane on `dev` supports the format and modifier.
// upstream: drm_plane.c drm_any_plane_has_format()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_any_plane_has_format(dev: *mut DrmDevice, format: u32, modifier: u64) -> bool {
    unsafe {
        let mut found = false;
        drm_for_each_plane(dev, |plane| {
            if drm_plane_has_format(plane, format, modifier) {
                found = true;
                return false;
            }
            true
        });
        found
    }
}

/// Legacy (non-atomic) plane update or disable.
// upstream: drm_plane.c __setplane_internal()
unsafe fn __setplane_internal(
    plane: *mut DrmPlane,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    unsafe {
        let mut ret: i32;

        warn_on(drm_drv_uses_atomic_modeset((*plane).dev));

        // No fb means shut it down
        if fb.is_null() {
            (*plane).old_fb = (*plane).fb;
            ret = (*(*plane).funcs).disable_plane.expect("disable_plane")(plane, ctx);
            if ret == 0 {
                (*plane).crtc = core::ptr::null_mut();
                (*plane).fb = core::ptr::null_mut();
            } else {
                (*plane).old_fb = core::ptr::null_mut();
            }
        } else {
            ret = __setplane_check(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h);
            if ret == 0 {
                (*plane).old_fb = (*plane).fb;
                ret = (*(*plane).funcs).update_plane.expect("update_plane")(
                    plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h, ctx,
                );
                if ret == 0 {
                    (*plane).crtc = crtc;
                    (*plane).fb = fb;
                    drm_mode_object_get(core::ptr::addr_of_mut!((*(*plane).fb).base));
                } else {
                    (*plane).old_fb = core::ptr::null_mut();
                }
            }
        }

        // out:
        if !(*plane).old_fb.is_null() {
            drm_mode_object_put(core::ptr::addr_of_mut!((*(*plane).old_fb).base));
        }
        (*plane).old_fb = core::ptr::null_mut();
        ret
    }
}

/// Atomic-driver plane update or disable through `update_plane`/`disable_plane`.
// upstream: drm_plane.c __setplane_atomic()
unsafe fn __setplane_atomic(
    plane: *mut DrmPlane,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    unsafe {
        warn_on(!drm_drv_uses_atomic_modeset((*plane).dev));

        // No fb means shut it down
        if fb.is_null() {
            return (*(*plane).funcs).disable_plane.expect("disable_plane")(plane, ctx);
        }

        // FIXME: This is redundant with drm_atomic_plane_check(), but the legacy
        // cursor/"async" .update_plane() tricks don't call that so we still need
        // this here.
        let ret = __setplane_check(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h);
        if ret != 0 {
            return ret;
        }

        (*(*plane).funcs).update_plane.expect("update_plane")(
            plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h, ctx,
        )
    }
}

/// Runs the right plane update under the modeset lock-all protocol.
// upstream: drm_plane.c setplane_internal()
unsafe fn setplane_internal(
    plane: *mut DrmPlane,
    crtc: *mut DrmCrtc,
    fb: *mut DrmFramebuffer,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
) -> i32 {
    unsafe {
        crate::drm_color_mgmt_upstream::with_modeset_lock_all((*plane).dev, 1 /* INTERRUPTIBLE */, |ctx| {
            if drm_drv_uses_atomic_modeset((*plane).dev) {
                __setplane_atomic(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h, ctx)
            } else {
                __setplane_internal(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, src_x, src_y, src_w, src_h, ctx)
            }
        })
    }
}

/// Legacy SETPLANE ioctl: binds (or disables, with fb_id 0) a plane.
// upstream: drm_plane.c drm_mode_setplane()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_setplane(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let plane_req = data as *mut DrmModeSetPlane;
        let mut crtc: *mut DrmCrtc = core::ptr::null_mut();
        let mut fb: *mut DrmFramebuffer = core::ptr::null_mut();

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        // First, find the plane, crtc, and fb objects. If not available, we don't
        // bother to call the driver.
        let plane = drm_plane_find(dev, file_priv, (*plane_req).plane_id);
        if plane.is_null() {
            // DRM_DEBUG_KMS("Unknown plane ID %d\n")
            return -ENOENT;
        }

        if (*plane_req).fb_id != 0 {
            fb = drm_framebuffer_lookup(dev, file_priv, (*plane_req).fb_id);
            if fb.is_null() {
                // DRM_DEBUG_KMS("Unknown framebuffer ID %d\n")
                return -ENOENT;
            }

            crtc = crate::drm_crtc_upstream::drm_crtc_find(dev, file_priv, (*plane_req).crtc_id);
            if crtc.is_null() {
                drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base));
                // DRM_DEBUG_KMS("Unknown crtc ID %d\n")
                return -ENOENT;
            }
        }

        let ret = setplane_internal(
            plane,
            crtc,
            fb,
            (*plane_req).crtc_x as i32,
            (*plane_req).crtc_y as i32,
            (*plane_req).crtc_w,
            (*plane_req).crtc_h,
            (*plane_req).src_x,
            (*plane_req).src_y,
            (*plane_req).src_w,
            (*plane_req).src_h,
        );

        if !fb.is_null() {
            drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base));
        }
        ret
    }
}

/// Universal cursor-plane update for a CRTC that owns a cursor plane.
// upstream: drm_plane.c drm_mode_cursor_universal()
unsafe fn drm_mode_cursor_universal(
    crtc: *mut DrmCrtc,
    req: *mut DrmModeCursor2,
    file_priv: *mut DrmFile,
    ctx: *mut DrmModesetAcquireCtx,
) -> i32 {
    unsafe {
        let _ = file_priv;
        let dev = (*crtc).dev;
        let plane = (*crtc).cursor;
        let fb: *mut DrmFramebuffer;
        let mut fbreq: DrmModeFbCmd2 = zeroed();
        fbreq.width = (*req).width;
        fbreq.height = (*req).height;
        fbreq.pixel_format = DRM_FORMAT_ARGB8888;
        fbreq.pitches[0] = (*req).width * 4;
        fbreq.handles[0] = (*req).handle;

        let mut crtc_w: u32 = 0;
        let mut crtc_h: u32 = 0;
        let mut src_w: u32 = 0;
        let mut src_h: u32 = 0;
        let ret: i32;

        tk_linux_kapi::BUG_ON!(plane.is_null());
        warn_on(!(*plane).crtc.is_null() && (*plane).crtc != crtc);

        // Obtain fb we'll be using (either new or existing) and take an extra
        // reference to it if fb != null. setplane will take care of dropping the
        // reference if the plane update fails.
        if (*req).flags & DRM_MODE_CURSOR_BO != 0 {
            if (*req).handle != 0 {
                fb = drm_internal_framebuffer_create(dev, &mut fbreq, file_priv);
                if IS_ERR(fb) {
                    // DRM_DEBUG_KMS("failed to wrap cursor buffer in drm framebuffer\n")
                    return PTR_ERR(fb);
                }

                if !(*plane).hotspot_x_property.is_null() && !(*plane).state.is_null() {
                    (*(*plane).state).hotspot_x = (*req).hot_x as i32;
                }
                if !(*plane).hotspot_y_property.is_null() && !(*plane).state.is_null() {
                    (*(*plane).state).hotspot_y = (*req).hot_y as i32;
                }
            } else {
                fb = core::ptr::null_mut();
            }
        } else {
            fb = if !(*plane).state.is_null() {
                (*(*plane).state).fb
            } else {
                (*plane).fb
            };
            if !fb.is_null() {
                drm_mode_object_get(core::ptr::addr_of_mut!((*fb).base));
            }
        }

        let (crtc_x, crtc_y) = if (*req).flags & DRM_MODE_CURSOR_MOVE != 0 {
            ((*req).x as i32, (*req).y as i32)
        } else {
            ((*crtc).cursor_x, (*crtc).cursor_y)
        };

        if !fb.is_null() {
            crtc_w = (*fb).width;
            crtc_h = (*fb).height;
            src_w = (*fb).width << 16;
            src_h = (*fb).height << 16;
        }

        if drm_drv_uses_atomic_modeset(dev) {
            ret = __setplane_atomic(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, 0, 0, src_w, src_h, ctx);
        } else {
            ret = __setplane_internal(plane, crtc, fb, crtc_x, crtc_y, crtc_w, crtc_h, 0, 0, src_w, src_h, ctx);
        }

        if !fb.is_null() {
            drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base));
        }

        // Update successful; save new cursor position, if necessary
        if ret == 0 && (*req).flags & DRM_MODE_CURSOR_MOVE != 0 {
            (*crtc).cursor_x = (*req).x as i32;
            (*crtc).cursor_y = (*req).y as i32;
        }
        ret
    }
}

/// Common body of the CURSOR and CURSOR2 ioctls.
// upstream: drm_plane.c drm_mode_cursor_common()
unsafe fn drm_mode_cursor_common(dev: *mut DrmDevice, req: *mut DrmModeCursor2, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        if (*req).flags == 0 || (!DRM_MODE_CURSOR_FLAGS & (*req).flags) != 0 {
            return -EINVAL;
        }

        let crtc = crate::drm_crtc_upstream::drm_crtc_find(dev, file_priv, (*req).crtc_id);
        if crtc.is_null() {
            // DRM_DEBUG_KMS("Unknown CRTC ID %d\n")
            return -ENOENT;
        }

        let mut ctx: DrmModesetAcquireCtx = zeroed();
        let ctx_ptr: *mut DrmModesetAcquireCtx = &mut ctx;
        drm_modeset_acquire_init(ctx_ptr, 1 /* INTERRUPTIBLE */);

        let mut ret: i32;
        loop {
            ret = drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).mutex), ctx_ptr);
            if ret == 0 {
                'out: {
                    // If this crtc has a universal cursor plane, call that plane's
                    // update handler rather than using legacy cursor handlers.
                    if !(*crtc).cursor.is_null() {
                        ret = drm_modeset_lock(core::ptr::addr_of_mut!((*(*crtc).cursor).mutex), ctx_ptr);
                        if ret != 0 {
                            break 'out;
                        }
                        if !drm_lease_held(file_priv, (*(*crtc).cursor).base.id as i32) {
                            ret = -EACCES;
                            break 'out;
                        }
                        ret = drm_mode_cursor_universal(crtc, req, file_priv, ctx_ptr);
                        break 'out;
                    }

                    if (*req).flags & DRM_MODE_CURSOR_BO != 0 {
                        let funcs = (*crtc).funcs;
                        if (*funcs).cursor_set.is_none() && (*funcs).cursor_set2.is_none() {
                            ret = -ENXIO;
                            break 'out;
                        }

                        // Turns off the cursor if handle is 0
                        if let Some(cursor_set2) = (*funcs).cursor_set2 {
                            ret = cursor_set2(crtc, file_priv, (*req).handle, (*req).width, (*req).height, (*req).hot_x as i32, (*req).hot_y as i32);
                        } else {
                            ret = (*funcs).cursor_set.expect("cursor_set")(crtc, file_priv, (*req).handle, (*req).width, (*req).height);
                        }
                    }

                    if (*req).flags & DRM_MODE_CURSOR_MOVE != 0 {
                        if let Some(cursor_move) = (*(*crtc).funcs).cursor_move {
                            ret = cursor_move(crtc, (*req).x as i32, (*req).y as i32);
                        } else {
                            ret = -EFAULT;
                            break 'out;
                        }
                    }
                }
            }

            // out:
            if ret == -EDEADLK {
                ret = drm_modeset_backoff(ctx_ptr);
                if ret == 0 {
                    continue;
                }
            }
            break;
        }

        drm_modeset_drop_locks(ctx_ptr);
        drm_modeset_acquire_fini(ctx_ptr);
        ret
    }
}

/// Legacy CURSOR ioctl (no hotspot).
// upstream: drm_plane.c drm_mode_cursor_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_cursor_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let req = data as *mut DrmModeCursor;
        let mut new_req: DrmModeCursor2 = zeroed();
        // memcpy(&new_req, req, sizeof(struct drm_mode_cursor))
        core::ptr::copy_nonoverlapping(req as *const u8, &mut new_req as *mut DrmModeCursor2 as *mut u8, size_of::<DrmModeCursor>());
        new_req.hot_x = 0;
        new_req.hot_y = 0;
        drm_mode_cursor_common(dev, &mut new_req, file_priv)
    }
}

/// CURSOR2 ioctl (with hotspot).
// upstream: drm_plane.c drm_mode_cursor2_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_cursor2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe { drm_mode_cursor_common(dev, data as *mut DrmModeCursor2, file_priv) }
}

/// Legacy PAGE_FLIP ioctl, including the target-vblank variants.
// upstream: drm_plane.c drm_mode_page_flip_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_page_flip_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    unsafe {
        let page_flip = data as *mut DrmModeCrtcPageFlipTarget;
        let target_vblank_in = (*page_flip).sequence;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        if (*page_flip).flags & !DRM_MODE_PAGE_FLIP_FLAGS != 0 {
            return -EINVAL;
        }

        if (*page_flip).sequence != 0 && (*page_flip).flags & DRM_MODE_PAGE_FLIP_TARGET == 0 {
            return -EINVAL;
        }

        // Only one of the DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE/RELATIVE flags can be specified
        if (*page_flip).flags & DRM_MODE_PAGE_FLIP_TARGET == DRM_MODE_PAGE_FLIP_TARGET {
            return -EINVAL;
        }

        if (*page_flip).flags & DRM_MODE_PAGE_FLIP_ASYNC != 0 && !(*dev).mode_config.async_page_flip {
            return -EINVAL;
        }

        let crtc = crate::drm_crtc_upstream::drm_crtc_find(dev, file_priv, (*page_flip).crtc_id);
        if crtc.is_null() {
            return -ENOENT;
        }

        let plane = (*crtc).primary;
        if !drm_lease_held(file_priv, (*plane).base.id as i32) {
            return -EACCES;
        }

        let mut target_vblank: u32 = target_vblank_in;
        if let Some(_) = (*(*crtc).funcs).page_flip_target {
            let r = crate::drm_vblank_upstream::drm_crtc_vblank_get(crtc);
            if r != 0 {
                return r;
            }

            let current_vblank = crate::drm_vblank_upstream::drm_crtc_vblank_count(crtc) as u32;
            match (*page_flip).flags & DRM_MODE_PAGE_FLIP_TARGET {
                DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE => {
                    if (target_vblank.wrapping_sub(current_vblank) as i32) > 1 {
                        // DRM_DEBUG("Invalid absolute flip target ...")
                        crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
                        return -EINVAL;
                    }
                }
                DRM_MODE_PAGE_FLIP_TARGET_RELATIVE => {
                    if target_vblank != 0 && target_vblank != 1 {
                        // DRM_DEBUG("Invalid relative flip target ...")
                        crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
                        return -EINVAL;
                    }
                    target_vblank = target_vblank.wrapping_add(current_vblank);
                }
                _ => {
                    target_vblank = current_vblank
                        .wrapping_add(((*page_flip).flags & DRM_MODE_PAGE_FLIP_ASYNC == 0) as u32);
                }
            }
        } else if (*(*crtc).funcs).page_flip.is_none() || (*page_flip).flags & DRM_MODE_PAGE_FLIP_TARGET != 0 {
            return -EINVAL;
        }

        let mut ctx: DrmModesetAcquireCtx = zeroed();
        let ctx_ptr: *mut DrmModesetAcquireCtx = &mut ctx;
        drm_modeset_acquire_init(ctx_ptr, 1 /* INTERRUPTIBLE */);

        let mut ret: i32;
        loop {
            // retry:
            let mut fb: *mut DrmFramebuffer;
            fb = core::ptr::null_mut();
            ret = drm_modeset_lock(core::ptr::addr_of_mut!((*crtc).mutex), ctx_ptr);
            if ret == 0 {
                ret = drm_modeset_lock(core::ptr::addr_of_mut!((*plane).mutex), ctx_ptr);
            }
            if ret == 0 {
                'out: {
                    let old_fb = if !(*plane).state.is_null() {
                        (*(*plane).state).fb
                    } else {
                        (*plane).fb
                    };
                    if old_fb.is_null() {
                        // The framebuffer is currently unbound, presumably due to a
                        // hotplug event, that userspace has not yet discovered.
                        ret = -EBUSY;
                        break 'out;
                    }

                    fb = drm_framebuffer_lookup(dev, file_priv, (*page_flip).fb_id);
                    if fb.is_null() {
                        ret = -ENOENT;
                        break 'out;
                    }

                    if !(*plane).state.is_null() {
                        let state = (*plane).state;
                        ret = drm_framebuffer_check_src_coords(
                            (*state).src_x,
                            (*state).src_y,
                            (*state).src_w,
                            (*state).src_h,
                            fb,
                        );
                    } else {
                        ret = crate::drm_crtc_upstream::drm_crtc_check_viewport(
                            crtc,
                            (*crtc).x,
                            (*crtc).y,
                            core::ptr::addr_of!((*crtc).mode),
                            fb,
                        );
                    }
                    if ret != 0 {
                        break 'out;
                    }

                    // Only check the FOURCC format code, excluding modifiers. This is
                    // enough for all legacy drivers. Atomic drivers have their own
                    // checks in their ->atomic_check implementation.
                    if (*(*old_fb).format).format != (*(*fb).format).format {
                        // DRM_DEBUG_KMS("Page flip is not allowed to change frame buffer format.\n")
                        ret = -EINVAL;
                        break 'out;
                    }

                    let mut e: *mut DrmPendingVblankEvent = core::ptr::null_mut();
                    if (*page_flip).flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
                        // kzalloc_obj(*e)
                        e = tk_linux_kapi::memory::kzalloc(size_of::<DrmPendingVblankEvent>(), GFP_KERNEL)
                            as *mut DrmPendingVblankEvent;
                        if e.is_null() {
                            ret = -ENOMEM;
                            break 'out;
                        }

                        (*ev_base(e)).r#type = DRM_EVENT_FLIP_COMPLETE;
                        (*ev_base(e)).length = size_of::<DrmEventVblank>() as u32;
                        (*ev_vbl(e)).user_data = (*page_flip).user_data;
                        (*ev_vbl(e)).crtc_id = (*crtc).base.id;

                        ret = drm_event_reserve_init(
                            dev,
                            file_priv,
                            core::ptr::addr_of_mut!((*e).base),
                            core::ptr::addr_of_mut!((*e).event.base) as *mut DrmEvent,
                        );
                        if ret != 0 {
                            tk_linux_kapi::memory::kfree(e);
                            e = core::ptr::null_mut();
                            break 'out;
                        }
                    }

                    (*plane).old_fb = (*plane).fb;
                    if let Some(page_flip_target) = (*(*crtc).funcs).page_flip_target {
                        ret = page_flip_target(crtc, fb, e, (*page_flip).flags, target_vblank, ctx_ptr);
                    } else {
                        ret = (*(*crtc).funcs).page_flip.expect("page_flip")(crtc, fb, e, (*page_flip).flags, ctx_ptr);
                    }

                    if ret != 0 {
                        if (*page_flip).flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
                            drm_event_cancel_free(dev, core::ptr::addr_of_mut!((*e).base));
                        }
                        // Keep the old fb, don't unref it.
                        (*plane).old_fb = core::ptr::null_mut();
                    } else if (*plane).state.is_null() {
                        (*plane).fb = fb;
                        drm_mode_object_get(core::ptr::addr_of_mut!((*fb).base));
                    }
                }
            }

            // out:
            if !fb.is_null() {
                drm_mode_object_put(core::ptr::addr_of_mut!((*fb).base));
            }
            if !(*plane).old_fb.is_null() {
                drm_mode_object_put(core::ptr::addr_of_mut!((*(*plane).old_fb).base));
            }
            (*plane).old_fb = core::ptr::null_mut();

            if ret == -EDEADLK {
                ret = drm_modeset_backoff(ctx_ptr);
                if ret == 0 {
                    continue;
                }
            }
            break;
        }

        drm_modeset_drop_locks(ctx_ptr);
        drm_modeset_acquire_fini(ctx_ptr);

        if ret != 0 && (*(*crtc).funcs).page_flip_target.is_some() {
            crate::drm_vblank_upstream::drm_crtc_vblank_put(crtc);
        }
        ret
    }
}

/// Marks the plane as accepting FB_DAMAGE_CLIPS.
// upstream: drm_plane.c drm_plane_enable_fb_damage_clips()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_enable_fb_damage_clips(plane: *mut DrmPlane) {
    unsafe {
        let dev = (*plane).dev;
        let config = core::ptr::addr_of!((*dev).mode_config);
        drm_object_attach_property(core::ptr::addr_of_mut!((*plane).base), (*config).prop_fb_damage_clips, 0);
    }
}

/// Number of damage clips set by userspace on the plane state.
// upstream: drm_plane.c drm_plane_get_damage_clips_count()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_get_damage_clips_count(state: *const DrmPlaneState) -> u32 {
    unsafe {
        if !state.is_null() && !(*state).fb_damage_clips.is_null() {
            ((*(*state).fb_damage_clips).length / size_of::<DrmModeRect>()) as u32
        } else {
            0
        }
    }
}

/// Raw damage clip array of the plane state (NULL when unset).
// upstream: drm_plane.c __drm_plane_get_damage_clips()
pub unsafe fn __drm_plane_get_damage_clips(state: *const DrmPlaneState) -> *mut DrmModeRect {
    unsafe {
        if !state.is_null() && !(*state).fb_damage_clips.is_null() {
            (*(*state).fb_damage_clips).data as *mut DrmModeRect
        } else {
            core::ptr::null_mut()
        }
    }
}

/// Damage clip array, warning when FB_DAMAGE_CLIPS was not enabled.
// upstream: drm_plane.c drm_plane_get_damage_clips()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_get_damage_clips(state: *const DrmPlaneState) -> *mut DrmModeRect {
    unsafe {
        let dev = (*(*state).plane).dev;
        let config = core::ptr::addr_of!((*dev).mode_config);

        // check that drm_plane_enable_fb_damage_clips() was called
        if drm_mode_obj_find_prop_id(core::ptr::addr_of_mut!((*(*state).plane).base), (*(*config).prop_fb_damage_clips).base.id).is_null() {
            // drm_warn_once(dev, "drm_plane_enable_fb_damage_clips() not called\n")
            warn_on(true);
        }

        __drm_plane_get_damage_clips(state)
    }
}

/// Builds the SCALING_FILTER enum property for the supported filters.
// upstream: drm_plane.c drm_create_scaling_filter_prop()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_create_scaling_filter_prop(dev: *mut DrmDevice, supported_filters: u32) -> *mut DrmProperty {
    unsafe {
        // static const struct drm_prop_enum_list props[]
        let props: [(u32, &[u8]); 2] = [
            (DRM_SCALING_FILTER_DEFAULT, b"Default\0"),
            (DRM_SCALING_FILTER_NEAREST_NEIGHBOR, b"Nearest Neighbor\0"),
        ];
        let valid_mode_mask: u32 = (1u32 << DRM_SCALING_FILTER_DEFAULT) | (1u32 << DRM_SCALING_FILTER_NEAREST_NEIGHBOR);

        if warn_on((supported_filters & !valid_mode_mask) != 0 || (supported_filters & (1u32 << DRM_SCALING_FILTER_DEFAULT)) == 0) {
            return tk_linux_kapi::config::ERR_PTR::<DrmProperty>(-EINVAL);
        }

        let prop = drm_property_create(
            dev,
            DRM_MODE_PROP_ENUM,
            b"SCALING_FILTER\0".as_ptr() as *const c_char,
            supported_filters.count_ones() as i32,
        );
        if prop.is_null() {
            return tk_linux_kapi::config::ERR_PTR::<DrmProperty>(-ENOMEM);
        }

        for (value, name) in props.iter() {
            if (1u32 << *value) & supported_filters == 0 {
                continue;
            }
            let ret = drm_property_add_enum(prop, *value as u64, name.as_ptr() as *const c_char);
            if ret != 0 {
                drm_property_destroy(dev, prop);
                return tk_linux_kapi::config::ERR_PTR::<DrmProperty>(ret);
            }
        }

        prop
    }
}

/// Creates and attaches the plane SCALING_FILTER property.
// upstream: drm_plane.c drm_plane_create_scaling_filter_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_scaling_filter_property(plane: *mut DrmPlane, supported_filters: u32) -> i32 {
    unsafe {
        let prop = drm_create_scaling_filter_prop((*plane).dev, supported_filters);
        if IS_ERR(prop) {
            return PTR_ERR(prop);
        }

        drm_object_attach_property(core::ptr::addr_of_mut!((*plane).base), prop, DRM_SCALING_FILTER_DEFAULT as u64);
        (*plane).scaling_filter_property = prop;
        0
    }
}

/// Attaches the SIZE_HINTS blob (cursor planes only).
// upstream: drm_plane.c drm_plane_add_size_hints_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_add_size_hints_property(
    plane: *mut DrmPlane,
    hints: *const DrmPlaneSizeHint,
    num_hints: i32,
) -> i32 {
    unsafe {
        let dev = (*plane).dev;
        let config = core::ptr::addr_of!((*dev).mode_config);

        // extending to other plane types needs actual thought
        if warn_on((*plane).r#type != DRM_PLANE_TYPE_CURSOR) {
            return -EINVAL;
        }

        // array_size(sizeof(hints[0]), num_hints): saturates to SIZE_MAX on overflow
        let size = size_of::<DrmPlaneSizeHint>().saturating_mul(num_hints as usize);
        let blob = drm_property_create_blob(dev, size, hints as *const c_void);
        if IS_ERR(blob) {
            return PTR_ERR(blob);
        }

        drm_object_attach_property(
            core::ptr::addr_of_mut!((*plane).base),
            (*config).size_hints_property,
            (*blob).base.id as u64,
        );
        0
    }
}

/// Creates the COLOR_PIPELINE enum property: a default Bypass entry followed
/// by the driver's pipelines.
// upstream: drm_plane.c drm_plane_create_color_pipeline_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_plane_create_color_pipeline_property(
    plane: *mut DrmPlane,
    pipelines: *const DrmPropEnumList,
    num_pipelines: i32,
) -> i32 {
    unsafe {
        let n = (num_pipelines + 1) as usize;
        let all_pipelines = tk_linux_kapi::memory::kzalloc(n * size_of::<DrmPropEnumList>(), GFP_KERNEL)
            as *mut DrmPropEnumList;
        if all_pipelines.is_null() {
            // drm_err(plane->dev, "failed to allocate color pipeline\n")
            return -ENOMEM;
        }

        let mut len = 0usize;
        // Create default Bypass color pipeline
        (*all_pipelines.add(len)).r#type = 0;
        (*all_pipelines.add(len)).name = b"Bypass\0".as_ptr() as *mut c_char;
        len += 1;

        // Add all other color pipelines
        for i in 0..num_pipelines as usize {
            (*all_pipelines.add(len)).r#type = (*pipelines.add(i)).r#type;
            (*all_pipelines.add(len)).name = (*pipelines.add(i)).name;
            len += 1;
        }

        let prop = drm_property_create_enum(
            (*plane).dev,
            DRM_MODE_PROP_ATOMIC,
            b"COLOR_PIPELINE\0".as_ptr() as *const c_char,
            all_pipelines,
            len as i32,
        );
        if prop.is_null() {
            tk_linux_kapi::memory::kfree(all_pipelines);
            return -ENOMEM;
        }

        drm_object_attach_property(core::ptr::addr_of_mut!((*plane).base), prop, 0);
        (*plane).color_pipeline_property = prop;
        tk_linux_kapi::memory::kfree(all_pipelines);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDrm;
    use crate::uapi_drm_consts::DRM_FORMAT_XRGB8888;
    use core::mem::zeroed;
    use std::boxed::Box;
    use tk_linux_kapi::list::INIT_LIST_HEAD;

    /// Plane funcs with no optional hooks.
    fn funcs() -> *mut DrmPlaneFuncs {
        Box::leak(Box::new(unsafe { zeroed() }))
    }

    #[test]
    fn plane_lookup_by_index_walks_the_plane_list() {
        let mut drm = TestDrm::new(DRIVER_MODESET);
        let dev = drm.ptr();
        unsafe {
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*dev).mode_config.plane_list));
            let p0: *mut DrmPlane = Box::into_raw(Box::new(zeroed()));
            let p1: *mut DrmPlane = Box::into_raw(Box::new(zeroed()));
            (*p0).index = 0;
            (*p1).index = 1;
            list_add_tail(core::ptr::addr_of_mut!((*p0).head), core::ptr::addr_of_mut!((*dev).mode_config.plane_list));
            list_add_tail(core::ptr::addr_of_mut!((*p1).head), core::ptr::addr_of_mut!((*dev).mode_config.plane_list));
            assert_eq!(drm_plane_from_index(dev, 0), p0);
            assert_eq!(drm_plane_from_index(dev, 1), p1);
            assert!(drm_plane_from_index(dev, 2).is_null());
            assert_eq!(drm_num_planes(dev), 2);
        }
    }

    #[test]
    fn format_lookup_checks_the_format_list_and_modifiers() {
        let mut plane: DrmPlane = unsafe { zeroed() };
        let formats = [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888];
        let modifiers = [DRM_FORMAT_MOD_LINEAR];
        plane.funcs = funcs();
        plane.format_types = formats.as_ptr() as *mut u32;
        plane.format_count = formats.len() as u32;
        plane.modifiers = modifiers.as_ptr() as *mut u64;
        plane.modifier_count = modifiers.len() as u32;
        unsafe {
            assert!(drm_plane_has_format(&mut plane, DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR));
            assert!(!drm_plane_has_format(&mut plane, 0x3231_3252, DRM_FORMAT_MOD_LINEAR));
            assert!(!drm_plane_has_format(&mut plane, DRM_FORMAT_XRGB8888, 1));
        }
    }

    #[test]
    fn format_lookup_without_modifier_list_accepts_any_modifier() {
        let mut plane: DrmPlane = unsafe { zeroed() };
        let formats = [DRM_FORMAT_ARGB8888];
        plane.funcs = funcs();
        plane.format_types = formats.as_ptr() as *mut u32;
        plane.format_count = 1;
        unsafe {
            assert!(drm_plane_has_format(&mut plane, DRM_FORMAT_ARGB8888, 0x1234));
        }
    }
}
