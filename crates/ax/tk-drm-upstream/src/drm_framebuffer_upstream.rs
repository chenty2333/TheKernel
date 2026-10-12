// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_framebuffer.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3: the ADDFB/ADDFB2, RMFB,
//! CLOSEFB, GETFB/GETFB2 and DIRTYFB ioctl handlers, framebuffer init,
//! lookup, refcounting and removal.
//!
//! Deviations:
//! - `drm_mode_rmfb()` and `drm_fb_release()` run the deferred removal
//!   (`drm_mode_rmfb_work_fn`) inline. The Linux `schedule_work()` +
//!   `flush_work()` pair waits for that same function before returning, so
//!   the observable ordering is unchanged.
//! - The debugfs `framebuffer` info file is not ported (CONFIG_DEBUG_FS is off
//!   for this target).
//! - ioctl `data` pointers are kernel-resident (the uAPI copy has already been
//!   made), except `DIRTYFB` clips, which are copied from the user pointer.

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};

use alloc::format;
use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{
    EDEADLK, EFAULT, EINVAL, ENODEV, ENOENT, ENOMEM, ENOSPC, EOPNOTSUPP, ERANGE, ERR_PTR, IS_ERR,
    PTR_ERR,
};

// upstream: include/uapi/asm-generic/errno-base.h (not exported by tk-linux-kapi config)
const ENOSYS: i32 = 38;
use tk_linux_kapi::list::{list_add, list_add_tail, list_del, list_del_init, list_empty};
use tk_linux_kapi::memory::{kfree, kref_put, kzalloc_objs};
use tk_linux_kapi::mutex::{mutex_lock, mutex_unlock};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::types::{Kref, ListHead};

use crate::drm_atomic_types::DrmAtomicCommit;
use crate::drm_damage_helper_upstream::drm_atomic_commit_put;
use crate::drm_connector_types::DrmConnectorState;
use crate::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_fourcc_upstream::{__drm_format_info, drm_driver_legacy_fb_format, drm_format_info_bpp, drm_format_info_min_pitch, drm_get_format_info};
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_framebuffer_types::{DrmFramebuffer, DrmFramebufferFuncs};
use crate::drm_modes_types::DrmDisplayMode;
use crate::drm_mode_object_types::DrmModeObject;
use crate::drm_modeset_lock_types::DrmModesetAcquireCtx;
use crate::drm_plane_types::{DrmPlane, DrmPlaneState};
use crate::drm_print_types::DrmPrinter;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::{DrmModeClosefb, DrmModeFbCmd, DrmModeFbCmd2, DrmModeFbDirtyCmd};
use crate::uapi_drm_types::DrmClipRect;

use crate::drm_modeset_lock_upstream::{
    drm_modeset_acquire_fini, drm_modeset_acquire_init, drm_modeset_backoff, drm_modeset_drop_locks,
    drm_modeset_lock_all, drm_modeset_lock_all_ctx, drm_modeset_unlock_all,
};

// upstream: include/drm/drm_device.h / drm_drv.h
const DRIVER_MODESET: u32 = 1 << 1;
const DRIVER_ATOMIC: u32 = 1 << 2;
// upstream: include/drm/drm_capabilities CAP_SYS_ADMIN
const CAP_SYS_ADMIN: i32 = 21;

// upstream: drm_internal.h DRM_FRAMEBUFFER_HAS_HANDLE_REF(_idx)
#[inline]
const fn drm_framebuffer_has_handle_ref(idx: u32) -> u32 {
    1 << idx
}

// upstream: include/drm/drm_fourcc.h drm_format_info_plane_width()
#[inline]
fn drm_format_info_plane_width(info: *const DrmFormatInfo, width: i32, plane: i32) -> i32 {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    if info.is_null() || plane >= unsafe { (*info).num_planes } as i32 {
        return 0;
    }
    if plane == 0 {
        return width;
    }
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    let hsub = unsafe { (*info).hsub } as i32;
    (width + hsub - 1) / hsub
}

// upstream: include/drm/drm_fourcc.h drm_format_info_plane_height()
#[inline]
fn drm_format_info_plane_height(info: *const DrmFormatInfo, height: i32, plane: i32) -> i32 {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    if info.is_null() || plane >= unsafe { (*info).num_planes } as i32 {
        return 0;
    }
    if plane == 0 {
        return height;
    }
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    let vsub = unsafe { (*info).vsub } as i32;
    (height + vsub - 1) / vsub
}

// upstream: include/drm/drm_device.h drm_core_check_feature()
#[inline]
fn drm_core_check_feature(dev: *const DrmDevice, feature: u32) -> bool {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    (unsafe { (*dev).driver_features }) & feature != 0
}

// upstream: include/drm/drm_drv.h drm_drv_uses_atomic_modeset()
#[inline]
fn drm_drv_uses_atomic_modeset(dev: *const DrmDevice) -> bool {
    drm_core_check_feature(dev, DRIVER_ATOMIC)
}

// upstream: include/drm/drm_framebuffer.h drm_framebuffer_read_refcount()
#[inline]
unsafe fn drm_framebuffer_read_refcount(fb: *const DrmFramebuffer) -> u32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { tk_linux_kapi::memory::kref_read(&(*fb).base.refcount) }
}

// upstream: include/drm/drm_framebuffer.h drm_framebuffer_put()
#[inline]
unsafe fn drm_framebuffer_put(fb: *mut DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { kref_put(core::ptr::addr_of_mut!((*fb).base.refcount), drm_framebuffer_free) };
}

// upstream: include/drm/drm_framebuffer.h drm_framebuffer_get()
#[inline]
unsafe fn drm_framebuffer_get(fb: *mut DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { tk_linux_kapi::memory::kref_get(core::ptr::addr_of_mut!((*fb).base.refcount)) };
}

// upstream: include/drm/drm_mode_object.h obj_to_fb()
#[inline]
unsafe fn obj_to_fb(obj: *mut DrmModeObject) -> *mut DrmFramebuffer {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (obj as *mut u8).sub(offset_of!(DrmFramebuffer, base)) as *mut DrmFramebuffer }
}

// Cross-batch callees (mode objects, GEM handles, atomic, CRTC/plane
// force-disable, DRM auth). Resolved through `crate::pending_stubs`.
unsafe extern "C" {
    fn __drm_mode_object_add(
        dev: *mut DrmDevice,
        obj: *mut DrmModeObject,
        obj_type: u32,
        register: bool,
        free_cb: Option<unsafe extern "C" fn(*mut Kref)>,
    ) -> i32;
    fn drm_mode_object_register(dev: *mut DrmDevice, obj: *mut DrmModeObject);
    fn drm_mode_object_unregister(dev: *mut DrmDevice, obj: *mut DrmModeObject);
    fn __drm_mode_object_find(
        dev: *mut DrmDevice,
        file_priv: *mut DrmFile,
        id: u32,
        obj_type: u32,
    ) -> *mut DrmModeObject;
    fn drm_gem_handle_create(file_priv: *mut DrmFile, obj: *mut c_void, handlep: *mut u32) -> i32;
    fn drm_gem_handle_delete(file_priv: *mut DrmFile, handle: u32) -> i32;
    fn drm_gem_object_handle_get_if_exists_unlocked(obj: *mut c_void) -> bool;
    fn drm_gem_object_handle_put_unlocked(obj: *mut c_void);
    fn drm_gem_print_info(p: *mut DrmPrinter, indent: u32, obj: *const c_void);
    fn drm_is_current_master(file_priv: *mut DrmFile) -> bool;
    fn drm_atomic_commit_alloc(dev: *mut DrmDevice) -> *mut DrmAtomicCommit;
    fn drm_atomic_commit_clear(state: *mut DrmAtomicCommit);
    fn drm_atomic_add_affected_connectors(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> i32;
    fn drm_atomic_set_mode_for_crtc(crtc_state: *mut DrmCrtcState, mode: *const DrmDisplayMode) -> i32;
    fn drm_atomic_set_fb_for_plane(plane_state: *mut DrmPlaneState, fb: *mut DrmFramebuffer);
    fn drm_atomic_set_crtc_for_plane(plane_state: *mut DrmPlaneState, crtc: *mut DrmCrtc) -> i32;
    fn drm_atomic_set_crtc_for_connector(conn_state: *mut DrmConnectorState, crtc: *mut DrmCrtc) -> i32;
    fn drm_atomic_commit(state: *mut DrmAtomicCommit) -> i32;
    fn drm_atomic_get_plane_state(state: *mut DrmAtomicCommit, plane: *mut DrmPlane) -> *mut DrmPlaneState;
    fn drm_crtc_force_disable(crtc: *mut DrmCrtc) -> i32;
    fn drm_plane_force_disable(plane: *mut DrmPlane);
}

// upstream: drm_framebuffer.c drm_framebuffer_check_src_coords()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_check_src_coords(
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    fb: *const DrmFramebuffer,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { &*fb };
    let fb_width: u32 = fb.width << 16;
    let fb_height: u32 = fb.height << 16;

    if src_w > fb_width
        || src_x > fb_width - src_w
        || src_h > fb_height
        || src_y > fb_height - src_h
    {
        drm_log_at(
            DrmLogLevel::Debug,
            "drm_framebuffer",
            file!(),
            line!(),
            &format!(
                "Invalid source coordinates {}.{:06}x{}.{:06}+{}.{:06}+{}.{:06} (fb {}x{})",
                src_w >> 16,
                ((src_w & 0xffff) * 15625) >> 10,
                src_h >> 16,
                ((src_h & 0xffff) * 15625) >> 10,
                src_x >> 16,
                ((src_x & 0xffff) * 15625) >> 10,
                src_y >> 16,
                ((src_y & 0xffff) * 15625) >> 10,
                fb.width,
                fb.height
            ),
        );
        return -ENOSPC;
    }
    0
}

// upstream: drm_framebuffer.c drm_mode_addfb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_addfb(
    dev: *mut DrmDevice,
    or_: *mut DrmModeFbCmd,
    file_priv: *mut DrmFile,
) -> i32 {
    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let or_ = unsafe { &mut *or_ };

    let mut r = DrmModeFbCmd2 {
        fb_id: 0,
        width: 0,
        height: 0,
        pixel_format: 0,
        flags: 0,
        handles: [0; 4],
        pitches: [0; 4],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    r.pixel_format = unsafe { drm_driver_legacy_fb_format(dev, or_.bpp, or_.depth) };
    if r.pixel_format == DRM_FORMAT_INVALID {
        drm_log_at(
            DrmLogLevel::Debug,
            "drm_framebuffer",
            file!(),
            line!(),
            &format!("bad {{bpp:{}, depth:{}}}", or_.bpp, or_.depth),
        );
        return -EINVAL;
    }

    r.fb_id = or_.fb_id;
    r.width = or_.width;
    r.height = or_.height;
    r.pitches[0] = or_.pitch;
    r.handles[0] = or_.handle;

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { drm_mode_addfb2(dev, &mut r as *mut DrmModeFbCmd2 as *mut c_void, file_priv) };
    if ret != 0 {
        return ret;
    }

    or_.fb_id = r.fb_id;
    0
}

// upstream: drm_framebuffer.c drm_mode_addfb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_addfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_mode_addfb(dev, data as *mut DrmModeFbCmd, file_priv) }
}

// upstream: drm_framebuffer.c framebuffer_check()
unsafe fn framebuffer_check(dev: *mut DrmDevice, info: *const DrmFormatInfo, r: *const DrmModeFbCmd2) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let r = unsafe { &*r };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let info_ref = unsafe { &*info };

    if r.width == 0 {
        dbg_kms(dev, &format!("bad framebuffer width {}", r.width));
        return -EINVAL;
    }

    if r.height == 0 {
        dbg_kms(dev, &format!("bad framebuffer height {}", r.height));
        return -EINVAL;
    }

    for i in 0..info_ref.num_planes as usize {
        let width = drm_format_info_plane_width(info, r.width as i32, i as i32) as u32;
        let height = drm_format_info_plane_height(info, r.height as i32, i as i32) as u32;
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let block_size = unsafe { info_ref._anon_6.char_per_block[i] };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let min_pitch = unsafe { drm_format_info_min_pitch(info, i as i32, width) };

        if block_size == 0 && r.modifier[i] == DRM_FORMAT_MOD_LINEAR {
            dbg_kms(dev, &format!("Format requires non-linear modifier for plane {}", i));
            return -EINVAL;
        }

        if r.handles[i] == 0 {
            dbg_kms(dev, &format!("no buffer object handle for plane {}", i));
            return -EINVAL;
        }

        if min_pitch > u32::MAX as u64 {
            return -ERANGE;
        }

        if (height as u64) * (r.pitches[i] as u64) + (r.offsets[i] as u64) > u32::MAX as u64 {
            return -ERANGE;
        }

        if block_size != 0 && (r.pitches[i] as u64) < min_pitch {
            dbg_kms(dev, &format!("bad pitch {} for plane {}", r.pitches[i], i));
            return -EINVAL;
        }

        if r.modifier[i] != 0 && r.flags & DRM_MODE_FB_MODIFIERS == 0 {
            dbg_kms(dev, &format!("bad fb modifier {} for plane {}", r.modifier[i], i));
            return -EINVAL;
        }

        if r.flags & DRM_MODE_FB_MODIFIERS != 0 && r.modifier[i] != r.modifier[0] {
            dbg_kms(dev, &format!("bad fb modifier {} for plane {}", r.modifier[i], i));
            return -EINVAL;
        }

        match r.modifier[i] {
            DRM_FORMAT_MOD_SAMSUNG_64_32_TILE => {
                if r.pixel_format != DRM_FORMAT_NV12
                    || width % 128 != 0
                    || height % 32 != 0
                    || r.pitches[i] % 128 != 0
                {
                    dbg_kms(dev, &format!("bad modifier data for plane {}", i));
                    return -EINVAL;
                }
            }
            _ => {}
        }
    }

    for i in info_ref.num_planes as usize..4 {
        if r.modifier[i] != 0 {
            dbg_kms(dev, &format!("non-zero modifier for unused plane {}", i));
            return -EINVAL;
        }

        if r.flags & DRM_MODE_FB_MODIFIERS == 0 {
            continue;
        }

        if r.handles[i] != 0 {
            dbg_kms(dev, &format!("buffer object handle for unused plane {}", i));
            return -EINVAL;
        }

        if r.pitches[i] != 0 {
            dbg_kms(dev, &format!("non-zero pitch for unused plane {}", i));
            return -EINVAL;
        }

        if r.offsets[i] != 0 {
            dbg_kms(dev, &format!("non-zero offset for unused plane {}", i));
            return -EINVAL;
        }
    }

    0
}

// drm_dbg_kms(dev, ...) routed to the TheKernel log.
fn dbg_kms(dev: *const DrmDevice, msg: &str) {
    let _ = dev;
    drm_log_at(DrmLogLevel::Debug, "drm_kms", file!(), line!(), msg);
}

// upstream: drm_framebuffer.c drm_internal_framebuffer_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_internal_framebuffer_create(
    dev: *mut DrmDevice,
    r: *const DrmModeFbCmd2,
    file_priv: *mut DrmFile,
) -> *mut DrmFramebuffer {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let config = unsafe { core::ptr::addr_of_mut!((*dev).mode_config) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let rr = unsafe { &*r };

    if rr.flags & !(DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS) != 0 {
        dbg_kms(dev, &format!("bad framebuffer flags 0x{:08x}", rr.flags));
        return ERR_PTR(-EINVAL);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        if (*config).min_width > rr.width || rr.width > (*config).max_width {
            dbg_kms(
                dev,
                &format!(
                    "bad framebuffer width {}, should be >= {} && <= {}",
                    rr.width,
                    (*config).min_width,
                    (*config).max_width
                ),
            );
            return ERR_PTR(-EINVAL);
        }
        if (*config).min_height > rr.height || rr.height > (*config).max_height {
            dbg_kms(
                dev,
                &format!(
                    "bad framebuffer height {}, should be >= {} && <= {}",
                    rr.height,
                    (*config).min_height,
                    (*config).max_height
                ),
            );
            return ERR_PTR(-EINVAL);
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if rr.flags & DRM_MODE_FB_MODIFIERS != 0 && unsafe { (*config).fb_modifiers_not_supported } {
        dbg_kms(dev, "driver does not support fb modifiers");
        return ERR_PTR(-EINVAL);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { __drm_format_info(rr.pixel_format) }.is_null() {
        dbg_kms(dev, &format!("bad framebuffer format {:#010x}", rr.pixel_format));
        return ERR_PTR(-EINVAL);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let info = unsafe { drm_get_format_info(dev, rr.pixel_format, rr.modifier[0]) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { framebuffer_check(dev, info, r) };
    if ret != 0 {
        return ERR_PTR(ret);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb_create = unsafe { (*(*config).funcs).fb_create }.expect("fb_create");
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { fb_create(dev, file_priv, info as *mut DrmFormatInfo, r as *mut DrmModeFbCmd2) };
    if IS_ERR(fb) {
        dbg_kms(dev, "could not create framebuffer");
        return fb;
    }

    fb
}

// upstream: drm_framebuffer.c drm_mode_addfb2()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_addfb2(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }

    let r = data as *mut DrmModeFbCmd2;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_internal_framebuffer_create(dev, r, file_priv) };
    if IS_ERR(fb) {
        return PTR_ERR(fb);
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let id = unsafe { (*fb).base.id };
    dbg_kms(dev, &format!("[FB:{}]", id));
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*r).fb_id = id };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fbs_lock = unsafe { core::ptr::addr_of_mut!((*file_priv).fbs_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_lock(fbs_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_add(core::ptr::addr_of_mut!((*fb).filp_head), core::ptr::addr_of_mut!((*file_priv).fbs)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_unlock(fbs_lock) };

    0
}

// upstream: drm_framebuffer.c drm_mode_addfb2_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_addfb2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    // `__BIG_ENDIAN` is not set on x86_64.
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_mode_addfb2(dev, data, file_priv) }
}

// upstream: drm_framebuffer.c struct drm_mode_rmfb_work
#[repr(C)]
pub struct DrmModeRmfbWork {
    fbs: ListHead,
}

// upstream: drm_framebuffer.c drm_mode_rmfb_work_fn()
unsafe fn drm_mode_rmfb_work_fn(arg: *mut DrmModeRmfbWork) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*arg).fbs) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    while !unsafe { list_empty(&*head) } {
        // list_first_entry(&arg->fbs, struct drm_framebuffer, filp_head)
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let fb = unsafe { (*head).next.cast::<u8>().sub(offset_of!(DrmFramebuffer, filp_head)) } as *mut DrmFramebuffer;
        dbg_kms(
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*fb).dev },
            &format!(
                "Removing [FB:{}] from all active usage due to RMFB ioctl",
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { (*fb).base.id }
            ),
        );
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { list_del_init(core::ptr::addr_of_mut!((*fb).filp_head)) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_framebuffer_remove(fb) };
    }
}

// upstream: drm_framebuffer.c drm_mode_closefb()
unsafe fn drm_mode_closefb(fb: *mut DrmFramebuffer, file_priv: *mut DrmFile) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fbs_lock = unsafe { core::ptr::addr_of_mut!((*file_priv).fbs_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_lock(fbs_lock) };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*file_priv).fbs) };
    let mut found = false;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let fbl = unsafe { link.cast::<u8>().sub(offset_of!(DrmFramebuffer, filp_head)) as *mut DrmFramebuffer };
        if fb == fbl {
            found = true;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
    }

    if !found {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { mutex_unlock(fbs_lock) };
        return -ENOENT;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { list_del_init(core::ptr::addr_of_mut!((*fb).filp_head)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_unlock(fbs_lock) };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
    0
}

// upstream: drm_framebuffer.c drm_mode_rmfb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_rmfb(dev: *mut DrmDevice, fb_id: u32, file_priv: *mut DrmFile) -> i32 {
    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, fb_id) };
    if fb.is_null() {
        return -ENOENT;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { drm_mode_closefb(fb, file_priv) };
    if ret != 0 {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_framebuffer_put(fb) };
        return ret;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { drm_framebuffer_read_refcount(fb) } > 1 {
        let mut arg = DrmModeRmfbWork {
            fbs: ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        };
        let arg_head = core::ptr::addr_of_mut!(arg.fbs);
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*arg_head).next = arg_head;
            (*arg_head).prev = arg_head;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        warn_on(!unsafe { list_empty(&(*fb).filp_head) });
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { list_add_tail(core::ptr::addr_of_mut!((*fb).filp_head), arg_head) };
        // schedule_work() + flush_work(): see the module note.
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_mode_rmfb_work_fn(&mut arg as *mut DrmModeRmfbWork) };
    } else {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_framebuffer_put(fb) };
    }

    0
}

// upstream: drm_framebuffer.c drm_mode_rmfb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_rmfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb_id = unsafe { *(data as *mut u32) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_mode_rmfb(dev, fb_id, file_priv) }
}

// upstream: drm_framebuffer.c drm_mode_closefb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_closefb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    let r = data as *mut DrmModeClosefb;

    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { (*r).pad } != 0 {
        return -EINVAL;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, (*r).fb_id) };
    if fb.is_null() {
        return -ENOENT;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe { drm_mode_closefb(fb, file_priv) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
    ret
}

// upstream: drm_framebuffer.c drm_mode_getfb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getfb(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    let r = data as *mut DrmModeFbCmd;

    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, (*r).fb_id) };
    if fb.is_null() {
        return -ENOENT;
    }

    let ret: i32;
    'out: {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let format = unsafe { (*fb).format };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*format).num_planes } > 1 {
            ret = -EINVAL;
            break 'out;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*(*fb).funcs).create_handle }.is_none() {
            ret = -ENODEV;
            break 'out;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*r).height = (*fb).height;
            (*r).width = (*fb).width;
            (*r).depth = (*format).depth as u32;
            (*r).bpp = drm_format_info_bpp(format, 0);
            (*r).pitch = (*fb).pitches[0];
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if !unsafe { drm_is_current_master(file_priv) } && !unsafe { tk_linux_kapi::kernel_services::capable(CAP_SYS_ADMIN) } {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*r).handle = 0 };
            ret = 0;
            break 'out;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let create_handle = unsafe { (*(*fb).funcs).create_handle }.unwrap();
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        ret = unsafe { create_handle(fb, file_priv, &mut (*r).handle) };
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
    ret
}

// upstream: drm_framebuffer.c drm_mode_getfb2_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getfb2_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    let r = data as *mut DrmModeFbCmd2;
    let mut ret: i32 = 0;

    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EINVAL;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, (*r).fb_id) };
    if fb.is_null() {
        return -ENOENT;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mods_unsupported = unsafe { (*dev).mode_config.fb_modifiers_not_supported };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let format = unsafe { (*fb).format };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_planes = unsafe { (*format).num_planes } as usize;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let have_create_handle = unsafe { (*(*fb).funcs).create_handle }.is_some();

    'out: {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*fb).obj[0] }.is_null() && (num_planes > 1 || !have_create_handle) {
            ret = -ENODEV;
            break 'out;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe {
            (*r).height = (*fb).height;
            (*r).width = (*fb).width;
            (*r).pixel_format = (*format).format;
            (*r).flags = 0;
        }
        if !mods_unsupported {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*r).flags |= DRM_MODE_FB_MODIFIERS };
        }

        for i in 0..4 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe {
                (*r).handles[i] = 0;
                (*r).pitches[i] = 0;
                (*r).offsets[i] = 0;
                (*r).modifier[i] = 0;
            }
        }

        for i in 0..num_planes {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe {
                (*r).pitches[i] = (*fb).pitches[i];
                (*r).offsets[i] = (*fb).offsets[i];
                if !mods_unsupported {
                    (*r).modifier[i] = (*fb).modifier;
                }
            }
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if !unsafe { drm_is_current_master(file_priv) } && !unsafe { tk_linux_kapi::kernel_services::capable(CAP_SYS_ADMIN) } {
            ret = 0;
            break 'out;
        }

        for i in 0..num_planes {
            for j in 0..i {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                if unsafe { (*fb).obj[i] } == unsafe { (*fb).obj[j] } {
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    unsafe { (*r).handles[i] = (*r).handles[j] };
                    break;
                }
            }
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            if unsafe { (*r).handles[i] } != 0 {
                continue;
            }

            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let obj = unsafe { (*fb).obj[i] };
            if !obj.is_null() {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                ret = unsafe { drm_gem_handle_create(file_priv, obj as *mut c_void, &mut (*r).handles[i]) };
            } else {
                warn_on(i > 0);
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                let create_handle = unsafe { (*(*fb).funcs).create_handle }.unwrap();
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                ret = unsafe { create_handle(fb, file_priv, &mut (*r).handles[i]) };
            }
            if ret != 0 {
                break 'out;
            }
        }
    }

    // out:
    if ret != 0 {
        for i in 0..4 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            if unsafe { (*r).handles[i] } != 0 {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { drm_gem_handle_delete(file_priv, (*r).handles[i]) };
            }
            for j in i + 1..4 {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                if unsafe { (*r).handles[j] == (*r).handles[i] } {
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    unsafe { (*r).handles[j] = 0 };
                }
            }
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
    ret
}

// upstream: drm_framebuffer.c drm_mode_dirtyfb_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_dirtyfb_ioctl(dev: *mut DrmDevice, data: *mut c_void, file_priv: *mut DrmFile) -> i32 {
    let r = data as *mut DrmModeFbDirtyCmd;

    if !drm_core_check_feature(dev, DRIVER_MODESET) {
        return -EOPNOTSUPP;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { drm_framebuffer_lookup(dev, file_priv, (*r).fb_id) };
    if fb.is_null() {
        return -ENOENT;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_clips = unsafe { (*r).num_clips } as i32;
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let clips_ptr = unsafe { (*r).clips_ptr } as usize as *mut DrmClipRect;

    let mut clips: *mut DrmClipRect = core::ptr::null_mut();
    let ret: i32;

    'out: {
        if (num_clips == 0) != clips_ptr.is_null() {
            ret = -EINVAL;
            break 'out;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let flags = DRM_MODE_FB_DIRTY_FLAGS & unsafe { (*r).flags };
        if flags & DRM_MODE_FB_DIRTY_ANNOTATE_COPY != 0 && (num_clips % 2) != 0 {
            ret = -EINVAL;
            break 'out;
        }

        if num_clips != 0 && !clips_ptr.is_null() {
            if num_clips < 0 || num_clips as u32 > DRM_MODE_FB_DIRTY_MAX_CLIPS {
                ret = -EINVAL;
                break 'out;
            }

            clips = kzalloc_objs::<DrmClipRect, i32>(num_clips);
            if clips.is_null() {
                ret = -ENOMEM;
                break 'out;
            }

            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let left = unsafe {
                tk_linux_kapi::mm_native::copy_from_user(
                    clips as *mut c_void,
                    clips_ptr as *const c_void,
                    num_clips as usize * size_of::<DrmClipRect>(),
                )
            };
            if left != 0 {
                ret = -EFAULT;
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { kfree(clips) };
                break 'out;
            }
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let dirty = unsafe { (*(*fb).funcs).dirty };
        ret = match dirty {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            Some(dirty) => unsafe {
                dirty(fb, file_priv, flags, (*r).color, clips, num_clips as u32)
            },
            None => -ENOSYS,
        };
        if !clips.is_null() {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { kfree(clips) };
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_framebuffer_put(fb) };
        return ret;
    }

    // out_err1:
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
    ret
}

// upstream: drm_framebuffer.c drm_fb_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_release(priv_: *mut DrmFile) {
    let mut arg = DrmModeRmfbWork {
        fbs: ListHead {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        },
    };
    let arg_head = core::ptr::addr_of_mut!(arg.fbs);
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*arg_head).next = arg_head;
        (*arg_head).prev = arg_head;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*priv_).fbs) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let next = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let fb = unsafe { link.cast::<u8>().sub(offset_of!(DrmFramebuffer, filp_head)) as *mut DrmFramebuffer };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { drm_framebuffer_read_refcount(fb) } > 1 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { list_move_tail_raw(core::ptr::addr_of_mut!((*fb).filp_head), arg_head) };
        } else {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { list_del_init(core::ptr::addr_of_mut!((*fb).filp_head)) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_framebuffer_put(fb) };
        }
        link = next;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if !unsafe { list_empty(&*arg_head) } {
        // schedule_work() + flush_work(): see the module note.
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_mode_rmfb_work_fn(&mut arg as *mut DrmModeRmfbWork) };
    }
}

// upstream: include/linux/list.h list_move_tail()
#[inline]
unsafe fn list_move_tail_raw(entry: *mut ListHead, head: *mut ListHead) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { tk_linux_kapi::list::list_move_tail(entry, head) };
}

// upstream: drm_framebuffer.c drm_framebuffer_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_free(kref: *mut Kref) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb = unsafe { (kref as *mut u8).sub(offset_of!(DrmFramebuffer, base) + offset_of!(DrmModeObject, refcount)) as *mut DrmFramebuffer };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { list_empty(&(*fb).filp_head) });
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*fb).base)) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let destroy = unsafe { (*(*fb).funcs).destroy }.expect("destroy");
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { destroy(fb) };
}

// upstream: drm_framebuffer.c drm_framebuffer_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_init(
    dev: *mut DrmDevice,
    fb: *mut DrmFramebuffer,
    funcs: *const DrmFramebufferFuncs,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if warn_on(unsafe { (*fb).dev } != dev || unsafe { (*fb).format }.is_null()) {
        return -EINVAL;
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let format = unsafe { (*fb).format };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_planes = unsafe { (*format).num_planes } as u32;
    for i in 0..num_planes {
        let flag = drm_framebuffer_has_handle_ref(i);
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if warn_on(unsafe { (*fb).internal_flags } & flag != 0) {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { (*fb).internal_flags &= !flag };
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let obj = unsafe { (*fb).obj[i as usize] };
        if !obj.is_null() {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let exists = unsafe { drm_gem_object_handle_get_if_exists_unlocked(obj as *mut c_void) };
            if exists {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { (*fb).internal_flags |= flag };
            }
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        list_init(core::ptr::addr_of_mut!((*fb).filp_head));
        (*fb).funcs = funcs as *mut DrmFramebufferFuncs;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { copy_comm(fb) };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let ret = unsafe {
        __drm_mode_object_add(dev, core::ptr::addr_of_mut!((*fb).base), DRM_MODE_OBJECT_FB, false, Some(drm_framebuffer_free))
    };
    if ret != 0 {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        return unsafe { drm_framebuffer_init_err(fb, num_planes, ret) };
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb_lock = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.fb_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_lock(fb_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*dev).mode_config.num_fb += 1;
        list_add(core::ptr::addr_of_mut!((*fb).head), core::ptr::addr_of_mut!((*dev).mode_config.fb_list));
        mutex_unlock(fb_lock);
        drm_mode_object_register(dev, core::ptr::addr_of_mut!((*fb).base));
    }
    0
}

// `err:` label of drm_framebuffer_init().
unsafe fn drm_framebuffer_init_err(fb: *mut DrmFramebuffer, num_planes: u32, ret: i32) -> i32 {
    for i in 0..num_planes {
        let flag = drm_framebuffer_has_handle_ref(i);
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*fb).internal_flags } & flag != 0 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe {
                drm_gem_object_handle_put_unlocked((*fb).obj[i as usize] as *mut c_void);
                (*fb).internal_flags &= !flag;
            }
        }
    }
    ret
}

// INIT_LIST_HEAD()
#[inline]
unsafe fn list_init(head: *mut ListHead) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        (*head).next = head;
        (*head).prev = head;
    }
}

// strscpy(fb->comm, current->comm): TASK_COMM_LEN is 16 with the NUL.
unsafe fn copy_comm(fb: *mut DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let src = unsafe { tk_linux_kapi::kernel_services::current_comm() };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dst = unsafe { core::ptr::addr_of_mut!((*fb).comm) as *mut u8 };
    let mut i = 0usize;
    if !src.is_null() {
        while i < 15 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let c = unsafe { *(src as *const u8).add(i) };
            if c == 0 {
                break;
            }
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { *dst.add(i) = c };
            i += 1;
        }
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { *dst.add(i) = 0 };
}

// upstream: drm_framebuffer.c drm_framebuffer_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_lookup(dev: *mut DrmDevice, file_priv: *mut DrmFile, id: u32) -> *mut DrmFramebuffer {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let obj = unsafe { __drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_FB) };
    if !obj.is_null() {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { obj_to_fb(obj) }
    } else {
        core::ptr::null_mut()
    }
}

// upstream: drm_framebuffer.c drm_framebuffer_unregister_private()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_unregister_private(fb: *mut DrmFramebuffer) {
    if fb.is_null() {
        return;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*fb).base)) };
}

// upstream: drm_framebuffer.c drm_framebuffer_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_cleanup(fb: *mut DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let num_planes = unsafe { (*(*fb).format).num_planes } as u32;
    for i in 0..num_planes {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*fb).internal_flags } & drm_framebuffer_has_handle_ref(i) != 0 {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_gem_object_handle_put_unlocked((*fb).obj[i as usize] as *mut c_void) };
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb_lock = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.fb_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { mutex_lock(fb_lock) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe {
        list_del(core::ptr::addr_of_mut!((*fb).head));
        (*dev).mode_config.num_fb -= 1;
        mutex_unlock(fb_lock);
    }
}

// upstream: drm_framebuffer.c atomic_remove_fb()
unsafe fn atomic_remove_fb(fb: *mut DrmFramebuffer) -> i32 {
    let mut ctx = core::mem::MaybeUninit::<DrmModesetAcquireCtx>::zeroed();
    let ctx = ctx.as_mut_ptr();
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };
    let mut ret: i32;
    let mut disable_crtcs = false;

    loop {
        // retry_disable:
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_modeset_acquire_init(ctx, 0) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let state = unsafe { drm_atomic_commit_alloc(dev) };
        if state.is_null() {
            ret = -ENOMEM;
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_modeset_drop_locks(ctx) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_modeset_acquire_fini(ctx) };
            return ret;
        }
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { (*state).acquire_ctx = ctx };

        'retry: loop {
            let mut plane_mask: u32 = 0;
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            ret = unsafe { drm_modeset_lock_all_ctx(dev, ctx) };
            if ret != 0 {
                break 'retry;
            }

            // drm_for_each_plane()
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let mut link = unsafe { (*head).next };
            let mut bail = false;
            while link != head {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                let plane = unsafe { link.cast::<u8>().sub(offset_of!(DrmPlane, head)) as *mut DrmPlane };
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                link = unsafe { (*link).next };
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                if unsafe { (*(*plane).state).fb } != fb {
                    continue;
                }

                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                dbg_kms(dev, &format!("Disabling [PLANE:{}] because [FB:{}] is removed", unsafe { (*plane).base.id }, unsafe { (*fb).base.id }));
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                let plane_state = unsafe { drm_atomic_get_plane_state(state, plane) };
                if IS_ERR(plane_state) {
                    ret = PTR_ERR(plane_state);
                    bail = true;
                    break;
                }

                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                let pcrtc = unsafe { (*plane_state).crtc };
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                if disable_crtcs && unsafe { (*pcrtc).primary } == plane {
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    dbg_kms(dev, &format!("Disabling [CRTC:{}] because [FB:{}] is removed", unsafe { (*pcrtc).base.id }, unsafe { (*fb).base.id }));
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    let crtc_state = unsafe { drm_atomic_get_new_crtc_state_inline(state, pcrtc) };
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    ret = unsafe { drm_atomic_add_affected_connectors(state, pcrtc) };
                    if ret != 0 {
                        bail = true;
                        break;
                    }
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    unsafe { (*crtc_state).active = false };
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    ret = unsafe { drm_atomic_set_mode_for_crtc(crtc_state, core::ptr::null()) };
                    if ret != 0 {
                        bail = true;
                        break;
                    }
                }

                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { drm_atomic_set_fb_for_plane(plane_state, core::ptr::null_mut()) };
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                ret = unsafe { drm_atomic_set_crtc_for_plane(plane_state, core::ptr::null_mut()) };
                if ret != 0 {
                    bail = true;
                    break;
                }
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                plane_mask |= 1u32 << unsafe { (*plane).index };
            }
            if bail {
                // goto unlock
            } else {
                // for_each_new_connector_in_state(): connectors are walked by the
                // atomic state's connector array.
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                let nconn = unsafe { (*state).num_connector } as usize;
                for i in 0..nconn {
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    let entry = unsafe { &*(*state).connectors.add(i) };
                    if entry.ptr.is_null() {
                        continue;
                    }
                    let conn_state = entry.new_state;
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    ret = unsafe { drm_atomic_set_crtc_for_connector(conn_state, core::ptr::null_mut()) };
                    if ret != 0 {
                        break;
                    }
                }
                if ret == 0 && plane_mask != 0 {
                    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                    ret = unsafe { drm_atomic_commit(state) };
                }
            }

            // unlock:
            if ret == -EDEADLK {
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { drm_atomic_commit_clear(state) };
                // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
                unsafe { drm_modeset_backoff(ctx) };
                continue 'retry;
            }
            break 'retry;
        }

        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_atomic_commit_put(state) };
        // out:
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_modeset_drop_locks(ctx) };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { drm_modeset_acquire_fini(ctx) };
        if ret == -EINVAL && !disable_crtcs {
            disable_crtcs = true;
            continue;
        }
        return ret;
    }
}

// upstream: drm_crtc_internal / drm_atomic.h drm_atomic_get_new_crtc_state()
#[inline]
unsafe fn drm_atomic_get_new_crtc_state_inline(state: *mut DrmAtomicCommit, crtc: *mut DrmCrtc) -> *mut DrmCrtcState {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { (*(*state).crtcs.add((*crtc).index as usize)).new_state }
}

// upstream: drm_framebuffer.c legacy_remove_fb()
unsafe fn legacy_remove_fb(fb: *mut DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_lock_all(dev) };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.crtc_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let crtc = unsafe { link.cast::<u8>().sub(offset_of!(DrmCrtc, head)) as *mut DrmCrtc };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*(*crtc).primary).fb } == fb {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            dbg_kms(dev, &format!("Disabling [CRTC:{}] because [FB:{}] is removed", unsafe { (*crtc).base.id }, unsafe { (*fb).base.id }));
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            if unsafe { drm_crtc_force_disable(crtc) } != 0 {
                drm_log_at(DrmLogLevel::Error, "drm", file!(), line!(), &format!("*ERROR* failed to reset crtc {:p} when fb was deleted", crtc));
            }
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let head = unsafe { core::ptr::addr_of_mut!((*dev).mode_config.plane_list) };
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let mut link = unsafe { (*head).next };
    while link != head {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        let plane = unsafe { link.cast::<u8>().sub(offset_of!(DrmPlane, head)) as *mut DrmPlane };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        link = unsafe { (*link).next };
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        if unsafe { (*plane).fb } == fb {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            dbg_kms(dev, &format!("Disabling [PLANE:{}] because [FB:{}] is removed", unsafe { (*plane).base.id }, unsafe { (*fb).base.id }));
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_plane_force_disable(plane) };
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_modeset_unlock_all(dev) };
}

// upstream: drm_framebuffer.c drm_framebuffer_remove()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_remove(fb: *mut DrmFramebuffer) {
    if fb.is_null() {
        return;
    }
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let dev = unsafe { (*fb).dev };

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    warn_on(!unsafe { list_empty(&(*fb).filp_head) });

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    if unsafe { drm_framebuffer_read_refcount(fb) } > 1 {
        if drm_drv_uses_atomic_modeset(dev) {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            let ret = unsafe { atomic_remove_fb(fb) };
            warn_on(ret != 0);
        } else {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { legacy_remove_fb(fb) };
        }
    }

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    unsafe { drm_framebuffer_put(fb) };
}

// upstream: drm_framebuffer.c drm_framebuffer_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_framebuffer_print_info(p: *mut DrmPrinter, indent: u32, fb: *const DrmFramebuffer) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let fb_ref = unsafe { &*fb };
    let format = fb_ref.format;

    printf_indent(p, indent, &format!("allocated by = {}\n", cstr_of(fb_ref.comm.as_ptr())));
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    printf_indent(p, indent, &format!("refcount={}\n", unsafe { drm_framebuffer_read_refcount(fb) }));
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    printf_indent(p, indent, &format!("format={:#010x}\n", unsafe { (*format).format }));
    printf_indent(p, indent, &format!("modifier=0x{:x}\n", fb_ref.modifier));
    printf_indent(p, indent, &format!("size={}x{}\n", fb_ref.width, fb_ref.height));
    printf_indent(p, indent, "layers:\n");

    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    for i in 0..unsafe { (*format).num_planes } as usize {
        printf_indent(
            p,
            indent + 1,
            &format!(
                "size[{}]={}x{}\n",
                i,
                drm_format_info_plane_width(format, fb_ref.width as i32, i as i32),
                drm_format_info_plane_height(format, fb_ref.height as i32, i as i32)
            ),
        );
        printf_indent(p, indent + 1, &format!("pitch[{}]={}\n", i, fb_ref.pitches[i]));
        printf_indent(p, indent + 1, &format!("offset[{}]={}\n", i, fb_ref.offsets[i]));
        let obj = fb_ref.obj[i];
        printf_indent(p, indent + 1, &format!("obj[{}]:{}\n", i, if obj.is_null() { "(null)" } else { "" }));
        if !obj.is_null() {
            // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
            unsafe { drm_gem_print_info(p, indent + 2, obj as *const c_void) };
        }
    }
}

// drm_printf_indent(): one tab per indent level, then the formatted text.
fn printf_indent(p: *mut DrmPrinter, indent: u32, text: &str) {
    let mut line = alloc::string::String::new();
    for _ in 0..indent {
        line.push('\t');
    }
    line.push_str(text);
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    unsafe { crate::drm_print_upstream::drm_printf(p, &line) };
}

// NUL-terminated C char array to &str (for `comm`).
fn cstr_of<'a>(p: *const c_char) -> &'a str {
    // SAFETY: the pointer or reference used here was validated or constructed in the lines above.
    unsafe { core::ffi::CStr::from_ptr(p) }.to_str().unwrap_or("")
}
