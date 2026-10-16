// SPDX-License-Identifier: MIT
// Copyright © 2024 Intel Corporation
// Source: drivers/gpu/drm/i915/i915_bo.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_ulong, c_void},
    mem::MaybeUninit,
    ptr,
};

use tk_drm_upstream::{
    drm_device_types::DrmDevice, drm_gem_upstream::drm_gem_object_put, fb_types::FbInfo,
    uapi_drm_consts::DRM_MODE_FB_MODIFIERS, uapi_drm_mode_types::DrmModeFbCmd2,
};

use crate::{
    i915_gem_lmem_upstream::{i915_gem_object_create_lmem, i915_gem_object_is_lmem},
    i915_gem_mman_upstream::i915_gem_fb_mmap,
    i915_gem_object_api_upstream::{i915_gem_object_lock, i915_gem_object_put, i915_gem_object_unlock},
    i915_gem_object_header_upstream::{
        DrmFileObjectLookup, i915_gem_object_is_protected,
        i915_gem_object_is_tiled, i915_gem_object_is_userptr, i915_gem_object_lookup,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo, to_intel_bo},
    i915_gem_object_upstream::{i915_gem_object_can_migrate, i915_gem_object_read_from_page},
    i915_gem_pages_upstream::__i915_gem_object_get_dma_address,
    i915_gem_shmem_upstream::{i915_gem_object_create_shmem, i915_gem_object_is_shmem},
    i915_gem_stolen_upstream::i915_gem_object_create_stolen,
    i915_gem_ww_upstream::{I915GemWwCtx, __i915_gem_ww_fini, i915_gem_ww_ctx_init},
    i915_vma_api_upstream::i915_vma_pin_iomap,
    i915_vma_types_upstream::I915Vma,
    intel_fb_upstream::{I915_FORMAT_MOD_X_TILED, intel_fb_modifier_to_tiling},
    intel_parent_upstream::IntelDisplayBoInterface,
    intel_unsupported_hw_upstream::intel_pxp_key_check,
    linux::{
        gem_memory::{INTEL_REGION_LMEM_0, IntelMemoryRegion},
        i915::{GRAPHICS_VER, HAS_LMEM, IS_METEORLAKE, i915_ggtt_offset, to_gt, to_i915},
    },
    i915_gem_tiling_upstream::{I915_TILING_NONE, I915_TILING_X, I915_TILING_Y},
    linux_config::{
        EDEADLK, EINVAL, ENODEV, ENOENT, ENOMEM, ERR_PTR, I915_BO_ALLOC_CONTIGUOUS,
        I915_BO_ALLOC_USER, IS_ERR, PTR_ERR,
    },
    linux_i915_private::DrmI915Private,
};

/// `EREMOTE` (asm-generic errno 66): framebuffer object is not local.
const EREMOTE: i32 = 66;

/// `drm_dbg_kms()`: KMS debug category gate, device argument unused as in `drm_dbg!`.
macro_rules! drm_dbg_kms {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        if tk_linux_kapi::print::drm_debug_enabled(tk_linux_kapi::print::DRM_UT_KMS) {
            let __args: &[&dyn tk_linux_kapi::print::CFormatArg] =
                &[$(&($argument) as &dyn tk_linux_kapi::print::CFormatArg),*];
            let __message = tk_linux_kapi::print::format_message($format, __args);
            tk_linux_kapi::print::drm_log_at(
                tk_linux_kapi::print::DrmLogLevel::Debug,
                "i915 DRM debug",
                file!(),
                line!(),
                &__message,
            );
        }
    }};
}

/// `drm_info()`: device-level info print.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn tk_linux_kapi::print::CFormatArg] =
            &[$(&($argument) as &dyn tk_linux_kapi::print::CFormatArg),*];
        let __message = tk_linux_kapi::print::format_message($format, __args);
        tk_linux_kapi::print::drm_log_at(
            tk_linux_kapi::print::DrmLogLevel::Info,
            "i915 DRM info",
            file!(),
            line!(),
            &__message,
        );
    }};
}

// upstream: i915_bo.c i915_bo_is_tiled()
unsafe extern "C" fn i915_bo_is_tiled(obj: *mut c_void) -> bool {
    unsafe { i915_gem_object_is_tiled(to_intel_bo(obj.cast())) }
}

// upstream: i915_bo.c i915_bo_is_userptr()
unsafe extern "C" fn i915_bo_is_userptr(obj: *mut c_void) -> bool {
    unsafe { i915_gem_object_is_userptr(to_intel_bo(obj.cast())) }
}

// upstream: i915_bo.c i915_bo_is_shmem()
unsafe extern "C" fn i915_bo_is_shmem(obj: *mut c_void) -> bool {
    unsafe { i915_gem_object_is_shmem(to_intel_bo(obj.cast())) }
}

// upstream: i915_bo.c i915_bo_is_protected()
unsafe extern "C" fn i915_bo_is_protected(obj: *mut c_void) -> bool {
    unsafe { i915_gem_object_is_protected(to_intel_bo(obj.cast())) }
}

// upstream: i915_bo.c i915_bo_key_check()
unsafe extern "C" fn i915_bo_key_check(obj: *mut c_void) -> i32 {
    unsafe { intel_pxp_key_check(obj, false) }
}

// upstream: i915_bo.c i915_bo_fb_mmap()
unsafe extern "C" fn i915_bo_fb_mmap(obj: *mut c_void, vma: *mut c_void) -> i32 {
    unsafe { i915_gem_fb_mmap(to_intel_bo(obj.cast()), vma.cast()) }
}

// upstream: i915_bo.c i915_bo_read_from_page()
unsafe extern "C" fn i915_bo_read_from_page(
    obj: *mut c_void,
    offset: u64,
    dst: *mut c_void,
    size: i32,
) -> i32 {
    unsafe { i915_gem_object_read_from_page(to_intel_bo(obj.cast()), offset, dst, size) }
}

// upstream: i915_bo.c i915_bo_framebuffer_init()
unsafe extern "C" fn i915_bo_framebuffer_init(obj_: *mut c_void, mode_cmd: *mut c_void) -> i32 {
    let obj = unsafe { to_intel_bo(obj_.cast()) };
    let i915 = unsafe { to_i915((&(*obj).base.base).dev) };
    let mode_cmd = mode_cmd.cast::<DrmModeFbCmd2>();

    unsafe { i915_gem_object_lock(obj, ptr::null_mut()) };
    let tiling = unsafe { crate::linux::fields::i915_gem_object_get_tiling(obj) };
    let stride = unsafe { crate::linux::fields::i915_gem_object_get_stride(obj) };
    unsafe { i915_gem_object_unlock(obj) };

    if unsafe { (*mode_cmd).flags } & DRM_MODE_FB_MODIFIERS != 0 {
        // If there's a fence, enforce that the fb modifier and tiling mode match.
        if tiling != I915_TILING_NONE
            && tiling != unsafe { intel_fb_modifier_to_tiling((*mode_cmd).modifier[0]) }
        {
            drm_dbg_kms!(i915, "tiling_mode doesn't match fb modifier\n");
            return -EINVAL;
        }
    } else if tiling == I915_TILING_X {
        unsafe { (*mode_cmd).modifier[0] = I915_FORMAT_MOD_X_TILED };
    } else if tiling == I915_TILING_Y {
        drm_dbg_kms!(i915, "No Y tiling for legacy addfb\n");
        return -EINVAL;
    }

    // gen2/3 display engine uses the fence if present, so the tiling mode must
    // match the fb modifier exactly.
    if unsafe { GRAPHICS_VER(i915) } < 4
        && tiling != unsafe { intel_fb_modifier_to_tiling((*mode_cmd).modifier[0]) }
    {
        drm_dbg_kms!(i915, "tiling_mode must match fb modifier exactly on gen2/3\n");
        return -EINVAL;
    }

    // If there's a fence, enforce that the fb pitch and fence stride match.
    if tiling != I915_TILING_NONE && unsafe { (*mode_cmd).pitches[0] } != stride {
        drm_dbg_kms!(
            i915,
            "pitch (%d) must match tiling stride (%d)\n",
            unsafe { (*mode_cmd).pitches[0] },
            stride
        );
        return -EINVAL;
    }

    0
}

// upstream: i915_bo.c i915_bo_framebuffer_fini()
unsafe extern "C" fn i915_bo_framebuffer_fini(_obj: *mut c_void) {
    // Nothing to do for i915.
}

// upstream: i915_bo.c i915_bo_framebuffer_lookup()
unsafe extern "C" fn i915_bo_framebuffer_lookup(
    drm: *mut DrmDevice,
    filp: *mut c_void,
    mode_cmd: *const c_void,
) -> *mut c_void {
    let i915 = unsafe { to_i915(drm.cast()) };
    let mode_cmd = mode_cmd.cast::<DrmModeFbCmd2>();

    let obj = unsafe { i915_gem_object_lookup(filp.cast::<DrmFileObjectLookup>(), (*mode_cmd).handles[0]) };
    if obj.is_null() {
        return ERR_PTR(-ENOENT);
    }

    // Object is backed with LMEM for discrete.
    if unsafe { HAS_LMEM(i915) } && !unsafe { i915_gem_object_can_migrate(obj, INTEL_REGION_LMEM_0) } {
        // Object is "remote", not in local memory.
        unsafe { i915_gem_object_put(obj) };
        drm_dbg_kms!(i915, "framebuffer must reside in local memory\n");
        return ERR_PTR(-EREMOTE);
    }

    unsafe { intel_bo_to_drm_bo(obj).cast() }
}

// upstream: i915_bo.c i915_bo_fbdev_pitch_align()
unsafe extern "C" fn i915_bo_fbdev_pitch_align(stride: u32) -> u32 {
    // ALIGN(stride, 64) with C unsigned wrap-around.
    stride.wrapping_add(64 - 1) & !(64 - 1)
}

/// Read `i915->dsm.usable_size`: `struct i915_dsm` keeps `usable_size` at
/// byte 128 of the embedded 136-byte `dsm` record (see i915_gem_stolen_upstream).
#[inline]
unsafe fn dsm_usable_size(i915: *mut DrmI915Private) -> u64 {
    unsafe {
        ptr::addr_of!((*i915).dsm)
            .cast::<u8>()
            .add(128)
            .cast::<u64>()
            .read_unaligned()
    }
}

// upstream: i915_bo.c i915_bo_fbdev_prefer_stolen()
/// Exported for the display owner's initial-plane path (`i915_initial_plane.c`).
pub unsafe fn i915_bo_fbdev_prefer_stolen(i915: *mut DrmI915Private, size: u32) -> bool {
    // Skip stolen on MTL as Wa_22018444074 mitigation.
    if unsafe { IS_METEORLAKE(i915) } {
        return false;
    }

    // If the FB is too big, just don't use it since fbdev is not very
    // important and we should probably use that space with FBC or other
    // features.
    let usable_size = unsafe { dsm_usable_size(i915) };
    usable_size >= u64::from(size.wrapping_mul(2))
}

// upstream: i915_bo.c i915_bo_fbdev_create()
unsafe extern "C" fn i915_bo_fbdev_create(drm: *mut DrmDevice, size: i32) -> *mut c_void {
    let i915 = unsafe { to_i915(drm.cast()) };
    let mut obj: *mut DrmI915GemObject = ERR_PTR(-ENODEV);

    if unsafe { HAS_LMEM(i915) } {
        obj = unsafe {
            i915_gem_object_create_lmem(
                i915,
                size as u64,
                I915_BO_ALLOC_CONTIGUOUS | I915_BO_ALLOC_USER,
            )
        };
    } else {
        if unsafe { i915_bo_fbdev_prefer_stolen(i915, size as u32) } {
            obj = unsafe { i915_gem_object_create_stolen(i915, size as u64) };
        } else {
            drm_info!(drm, "Allocating fbdev: Stolen memory not preferred.\n");
        }

        if IS_ERR(obj) {
            obj = unsafe { i915_gem_object_create_shmem(i915, size as u64) };
        }
    }

    if IS_ERR(obj) {
        drm_err!(drm, "failed to allocate framebuffer (%pe)\n", obj);
        return ERR_PTR(-ENOMEM);
    }

    unsafe { intel_bo_to_drm_bo(obj).cast() }
}

// upstream: i915_bo.c i915_bo_fbdev_destroy()
unsafe extern "C" fn i915_bo_fbdev_destroy(obj: *mut c_void) {
    unsafe { drm_gem_object_put(obj.cast()) };
}

// upstream: i915_bo.c i915_bo_fbdev_fill_info()
unsafe extern "C" fn i915_bo_fbdev_fill_info(
    obj_: *mut c_void,
    info: *mut c_void,
    vma: *mut c_void,
) -> i32 {
    let info = info.cast::<FbInfo>();
    let vma = vma.cast::<I915Vma>();
    let obj = unsafe { to_intel_bo(obj_.cast()) };
    let i915 = unsafe { to_i915((&(*obj).base.base).dev) };

    if unsafe { i915_gem_object_is_lmem(obj) } {
        let mem: *mut IntelMemoryRegion = unsafe { (*obj).mm.region };

        // Use fbdev's framebuffer from lmem for discrete.
        let dma = unsafe { __i915_gem_object_get_dma_address(obj, 0) };
        unsafe {
            (*info).fix.smem_start = (*mem)
                .io
                .start
                .wrapping_add(dma)
                .wrapping_sub((*mem).region.start) as c_ulong;
            (*info).fix.smem_len = (&(*obj).base.base).size as _;
        }
    } else {
        let ggtt = unsafe { (*to_gt(i915)).ggtt };

        // Our framebuffer is the entirety of fbdev's system memory.
        unsafe {
            (*info).fix.smem_start =
                ((*ggtt).gmadr.start.wrapping_add(u64::from(i915_ggtt_offset(vma)))) as c_ulong;
            (*info).fix.smem_len = (*vma).size as _;
        }
    }

    // for_i915_gem_ww(&ww, ret, false)
    let mut ww = MaybeUninit::<I915GemWwCtx>::uninit();
    let ww = ww.as_mut_ptr();
    let mut vaddr: *mut c_void = ptr::null_mut();
    let mut ret: i32 = -EDEADLK;
    unsafe { i915_gem_ww_ctx_init(ww, false) };
    while ret == -EDEADLK {
        ret = 'body: {
            let r = unsafe { i915_gem_object_lock((*vma).obj, ww) };
            if r != 0 {
                break 'body r;
            }

            let v = unsafe { i915_vma_pin_iomap(vma) };
            if IS_ERR(v) {
                drm_err!(
                    i915,
                    "Failed to remap framebuffer into virtual memory (%pe)\n",
                    v
                );
                break 'body PTR_ERR(v);
            }
            vaddr = v;
            0
        };
        ret = unsafe { __i915_gem_ww_fini(ww, ret) };
    }

    if ret != 0 {
        return ret;
    }

    unsafe {
        (*info).screen_base = vaddr.cast();
        (*info).screen_size = (*intel_bo_to_drm_bo(obj)).size as c_ulong;
    }

    0
}

/// i915-owned bo callbacks exported to the display parent.
#[unsafe(no_mangle)]
pub static i915_display_bo_interface: IntelDisplayBoInterface = IntelDisplayBoInterface {
    is_tiled: Some(i915_bo_is_tiled),
    is_userptr: Some(i915_bo_is_userptr),
    is_shmem: Some(i915_bo_is_shmem),
    is_protected: Some(i915_bo_is_protected),
    key_check: Some(i915_bo_key_check),
    fb_mmap: Some(i915_bo_fb_mmap),
    read_from_page: Some(i915_bo_read_from_page),
    // `describe` is optional in display_parent_interface.h. The debugfs dump
    // (`i915_debugfs_describe_obj`, i915_debugfs.c) is not translated here, so
    // it stays absent and the display side prints nothing for this object.
    describe: None,
    framebuffer_init: Some(i915_bo_framebuffer_init),
    framebuffer_fini: Some(i915_bo_framebuffer_fini),
    framebuffer_lookup: Some(i915_bo_framebuffer_lookup),
    fbdev_create: Some(i915_bo_fbdev_create),
    fbdev_destroy: Some(i915_bo_fbdev_destroy),
    fbdev_fill_info: Some(i915_bo_fbdev_fill_info),
    fbdev_pitch_align: Some(i915_bo_fbdev_pitch_align),
};
