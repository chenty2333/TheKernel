// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright 2026 TheKernel contributors.
//! Port of Linux `drivers/gpu/drm/drm_gem_framebuffer_helper.c` (batch B2b).

#![allow(non_snake_case, non_upper_case_globals, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_int, c_void};

use tk_linux_kapi::assert::warn_on;
use tk_linux_kapi::config::{EINVAL, ENOENT, ENOMEM, ERR_PTR, GFP_KERNEL};
use tk_linux_kapi::memory::{kfree, kzalloc};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};

use crate::drm_color_mgmt_upstream::drm_drv_uses_atomic_modeset;
use crate::drm_damage_helper_upstream::drm_atomic_helper_dirtyfb;
use crate::drm_device_types::DrmDevice;
use crate::drm_file_types::DrmFile;
use crate::drm_fourcc_types::DrmFormatInfo;
use crate::drm_fourcc_upstream::{drm_format_info_bpp, drm_format_info_min_pitch};
use crate::drm_framebuffer_types::{
    DrmAfbcFramebuffer, DrmFramebuffer, DrmFramebufferFuncs, DRM_FORMAT_MAX_PLANES,
};
use crate::drm_framebuffer_upstream::{drm_framebuffer_cleanup, drm_framebuffer_init};
use crate::drm_gem_types::DrmGemObject;
use crate::drm_gem_upstream::{
    drm_gem_handle_create, drm_gem_object_lookup, drm_gem_object_put, drm_gem_vmap,
    drm_gem_vunmap,
};
use crate::drm_modeset_helper_upstream::drm_helper_mode_fill_fb_struct;
use crate::drm_plane_upstream::drm_any_plane_has_format;
use crate::drm_prime_upstream::{DmaBuf, DmaBufAttachment};
use crate::iosys_map_types::IosysMap;
use crate::uapi_drm_consts::*;
use crate::uapi_drm_mode_types::DrmModeFbCmd2;

unsafe extern "C" {
    fn dma_buf_begin_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int;
    fn dma_buf_end_cpu_access(dmabuf: *mut DmaBuf, direction: c_int) -> c_int;
}

const AFBC_HEADER_SIZE: u32 = 16;
const AFBC_TH_LAYOUT_ALIGNMENT: u32 = 8;
const AFBC_HDR_ALIGN: u32 = 64;
const AFBC_SUPERBLOCK_PIXELS: u32 = 256;
const AFBC_SUPERBLOCK_ALIGNMENT: u32 = 128;
const AFBC_TH_BODY_START_ALIGNMENT: u32 = 4096;

const AFBC_FORMAT_MOD_BLOCK_SIZE_MASK: u64 = 0xf;
const AFBC_FORMAT_MOD_BLOCK_SIZE_16x16: u64 = 1;
const AFBC_FORMAT_MOD_BLOCK_SIZE_32x8: u64 = 2;
const AFBC_FORMAT_MOD_BLOCK_SIZE_64x4: u64 = 3;
const AFBC_FORMAT_MOD_BLOCK_SIZE_32x8_64x4: u64 = 4;
const AFBC_FORMAT_MOD_TILED: u64 = 1 << 8;

#[inline]
const fn align_to(val: u32, align: u32) -> u32 {
    (val + align - 1) & !(align - 1)
}

#[inline]
unsafe fn iosys_map_is_null(map: *const IosysMap) -> bool {
    if map.is_null() {
        return true;
    }
    unsafe { !(*map).is_iomem && (*map)._anon_0.vaddr.is_null() }
}

#[inline]
pub(crate) unsafe fn iosys_map_incr(map: *mut IosysMap, bytes: usize) {
    unsafe {
        if (*map).is_iomem {
            (*map)._anon_0.vaddr_iomem = (*map)._anon_0.vaddr_iomem.cast::<u8>().add(bytes).cast();
        } else {
            (*map)._anon_0.vaddr = (*map)._anon_0.vaddr.cast::<u8>().add(bytes).cast();
        }
    }
}

#[inline]
unsafe fn drm_gem_is_imported(obj: *mut DrmGemObject) -> bool {
    unsafe { !(*obj).import_attach.is_null() }
}

#[inline]
pub unsafe fn drm_format_info_plane_width(
    info: *const DrmFormatInfo,
    width: u32,
    plane: u32,
) -> u32 {
    if info.is_null() || plane >= (*info).num_planes as u32 {
        return 0;
    }
    if plane == 0 {
        return width;
    }
    let hsub = (*info).hsub as u32;
    if hsub == 0 {
        return width;
    }
    (width + hsub - 1) / hsub
}

#[inline]
pub unsafe fn drm_format_info_plane_height(
    info: *const DrmFormatInfo,
    height: u32,
    plane: u32,
) -> u32 {
    if info.is_null() || plane >= (*info).num_planes as u32 {
        return 0;
    }
    if plane == 0 {
        return height;
    }
    let vsub = (*info).vsub as u32;
    if vsub == 0 {
        return height;
    }
    (height + vsub - 1) / vsub
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_get_obj()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_get_obj(
    fb: *mut DrmFramebuffer,
    plane: u32,
) -> *mut DrmGemObject {
    if fb.is_null() {
        return core::ptr::null_mut();
    }
    let dev = unsafe { (*fb).dev };
    if warn_on(dev.is_null() || plane as usize >= (*fb).obj.len()) {
        return core::ptr::null_mut();
    }
    let obj = unsafe { (*fb).obj[plane as usize] };
    if warn_on(obj.is_null()) {
        return core::ptr::null_mut();
    }
    obj
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_init()
unsafe fn drm_gem_fb_init(
    dev: *mut DrmDevice,
    fb: *mut DrmFramebuffer,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
    obj: *const *mut DrmGemObject,
    num_planes: u32,
    funcs: *const DrmFramebufferFuncs,
) -> i32 {
    unsafe {
        drm_helper_mode_fill_fb_struct(dev, fb, info, mode_cmd);
        for i in 0..(num_planes as usize) {
            (*fb).obj[i] = *obj.add(i);
        }
        let ret = drm_framebuffer_init(dev, fb, funcs);
        if ret != 0 {
            drm_log_at(
                DrmLogLevel::Error,
                "drm",
                file!(),
                line!(),
                &alloc::format!("Failed to init framebuffer: {}", ret),
            );
        }
        ret
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_destroy(fb: *mut DrmFramebuffer) {
    if fb.is_null() {
        return;
    }
    unsafe {
        let num_planes = if (*fb).format.is_null() {
            0
        } else {
            (*(*fb).format).num_planes as usize
        };
        for i in 0..num_planes {
            drm_gem_object_put((*fb).obj[i]);
        }
        drm_framebuffer_cleanup(fb);
        kfree(fb);
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_create_handle()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_create_handle(
    fb: *mut DrmFramebuffer,
    file: *mut DrmFile,
    handle: *mut u32,
) -> i32 {
    unsafe { drm_gem_handle_create(file, (*fb).obj[0], handle) }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_init_with_funcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_init_with_funcs(
    dev: *mut DrmDevice,
    fb: *mut DrmFramebuffer,
    file: *mut DrmFile,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
    funcs: *const DrmFramebufferFuncs,
) -> i32 {
    unsafe {
        if drm_drv_uses_atomic_modeset(dev)
            && !drm_any_plane_has_format(dev, (*mode_cmd).pixel_format, (*mode_cmd).modifier[0])
        {
            drm_log_at(
                DrmLogLevel::Debug,
                "drm",
                file!(),
                line!(),
                &alloc::format!(
                    "Unsupported pixel format {} / modifier {:#x}",
                    (*mode_cmd).pixel_format,
                    (*mode_cmd).modifier[0]
                ),
            );
            return -EINVAL;
        }

        let num_planes = (*info).num_planes as usize;
        let mut objs: [*mut DrmGemObject; DRM_FORMAT_MAX_PLANES] =
            [core::ptr::null_mut(); DRM_FORMAT_MAX_PLANES];

        let mut i = 0;
        let mut ret = 0;
        while i < num_planes {
            let width = drm_format_info_plane_width(info, (*mode_cmd).width, i as u32);
            let height = drm_format_info_plane_height(info, (*mode_cmd).height, i as u32);

            let obj = drm_gem_object_lookup(file, (*mode_cmd).handles[i]);
            if obj.is_null() {
                drm_log_at(
                    DrmLogLevel::Debug,
                    "drm",
                    file!(),
                    line!(),
                    "Failed to lookup GEM object",
                );
                ret = -ENOENT;
                break;
            }
            objs[i] = obj;

            let min_size = (height.saturating_sub(1)) as usize * (*mode_cmd).pitches[i] as usize
                + drm_format_info_min_pitch(info, i as i32, width) as usize
                + (*mode_cmd).offsets[i] as usize;

            if (*obj).size < min_size {
                drm_log_at(
                    DrmLogLevel::Debug,
                    "drm",
                    file!(),
                    line!(),
                    &alloc::format!(
                        "GEM object size ({}) smaller than minimum size ({}) for plane {}",
                        (*obj).size,
                        min_size,
                        i
                    ),
                );
                drm_gem_object_put(obj);
                objs[i] = core::ptr::null_mut();
                ret = -EINVAL;
                break;
            }
            i += 1;
        }

        if ret != 0 {
            while i > 0 {
                i -= 1;
                drm_gem_object_put(objs[i]);
            }
            return ret;
        }

        ret = drm_gem_fb_init(dev, fb, info, mode_cmd, objs.as_ptr(), i as u32, funcs);
        if ret != 0 {
            while i > 0 {
                i -= 1;
                drm_gem_object_put(objs[i]);
            }
            return ret;
        }

        0
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_create_with_funcs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_create_with_funcs(
    dev: *mut DrmDevice,
    file: *mut DrmFile,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
    funcs: *const DrmFramebufferFuncs,
) -> *mut DrmFramebuffer {
    unsafe {
        let fb = kzalloc(core::mem::size_of::<DrmFramebuffer>(), GFP_KERNEL).cast::<DrmFramebuffer>();
        if fb.is_null() {
            return ERR_PTR(-ENOMEM);
        }
        let ret = drm_gem_fb_init_with_funcs(dev, fb, file, info, mode_cmd, funcs);
        if ret != 0 {
            kfree(fb.cast::<c_void>());
            return ERR_PTR(ret);
        }
        fb
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_funcs
static DRM_GEM_FB_FUNCS: DrmFramebufferFuncs = DrmFramebufferFuncs {
    destroy: Some(drm_gem_fb_destroy),
    create_handle: Some(drm_gem_fb_create_handle),
    dirty: None,
};

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_create(
    dev: *mut DrmDevice,
    file: *mut DrmFile,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    unsafe { drm_gem_fb_create_with_funcs(dev, file, info, mode_cmd, &DRM_GEM_FB_FUNCS) }
}

unsafe extern "C" fn drm_gem_fb_dirtyfb(
    fb: *mut DrmFramebuffer,
    file_priv: *mut DrmFile,
    flags: u32,
    color: u32,
    clips: *mut crate::uapi_drm_types::DrmClipRect,
    num_clips: u32,
) -> i32 {
    unsafe { drm_atomic_helper_dirtyfb(fb, file_priv, flags, color, clips, num_clips) }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_funcs_dirtyfb
static DRM_GEM_FB_FUNCS_DIRTYFB: DrmFramebufferFuncs = DrmFramebufferFuncs {
    destroy: Some(drm_gem_fb_destroy),
    create_handle: Some(drm_gem_fb_create_handle),
    dirty: Some(drm_gem_fb_dirtyfb),
};

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_create_with_dirty()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_create_with_dirty(
    dev: *mut DrmDevice,
    file: *mut DrmFile,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
) -> *mut DrmFramebuffer {
    unsafe { drm_gem_fb_create_with_funcs(dev, file, info, mode_cmd, &DRM_GEM_FB_FUNCS_DIRTYFB) }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_vmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_vmap(
    fb: *mut DrmFramebuffer,
    map: *mut IosysMap,
    data: *mut IosysMap,
) -> i32 {
    unsafe {
        let num_planes = (*(*fb).format).num_planes as usize;
        let mut i = 0;
        let mut ret = 0;
        while i < num_planes {
            let obj = drm_gem_fb_get_obj(fb, i as u32);
            if obj.is_null() {
                ret = -EINVAL;
                break;
            }
            ret = drm_gem_vmap(obj, map.add(i).cast());
            if ret != 0 {
                break;
            }
            i += 1;
        }

        if ret != 0 {
            while i > 0 {
                i -= 1;
                let obj = drm_gem_fb_get_obj(fb, i as u32);
                if !obj.is_null() {
                    drm_gem_vunmap(obj, map.add(i).cast());
                }
            }
            return ret;
        }

        if !data.is_null() {
            for j in 0..num_planes {
                core::ptr::copy_nonoverlapping(map.add(j), data.add(j), 1);
                if iosys_map_is_null(data.add(j)) {
                    continue;
                }
                iosys_map_incr(data.add(j), (*fb).offsets[j] as usize);
            }
        }

        0
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_vunmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_vunmap(fb: *mut DrmFramebuffer, map: *mut IosysMap) {
    unsafe {
        let mut i = (*(*fb).format).num_planes as usize;
        while i > 0 {
            i -= 1;
            let obj = drm_gem_fb_get_obj(fb, i as u32);
            if obj.is_null() {
                continue;
            }
            if iosys_map_is_null(map.add(i)) {
                continue;
            }
            drm_gem_vunmap(obj, map.add(i).cast());
        }
    }
}

// upstream: drm_gem_framebuffer_helper.c __drm_gem_fb_end_cpu_access()
unsafe fn __drm_gem_fb_end_cpu_access(
    fb: *mut DrmFramebuffer,
    dir: c_int,
    mut num_planes: u32,
) {
    unsafe {
        while num_planes > 0 {
            num_planes -= 1;
            let obj = drm_gem_fb_get_obj(fb, num_planes);
            if obj.is_null() {
                continue;
            }
            let import_attach = (*obj).import_attach.cast::<DmaBufAttachment>();
            if !drm_gem_is_imported(obj) {
                continue;
            }
            let ret = dma_buf_end_cpu_access((*import_attach).dmabuf, dir);
            if ret != 0 {
                drm_log_at(
                    DrmLogLevel::Error,
                    "drm",
                    file!(),
                    line!(),
                    &alloc::format!(
                        "dma_buf_end_cpu_access({}, {}) failed: {}",
                        num_planes, dir, ret
                    ),
                );
            }
        }
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_begin_cpu_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_begin_cpu_access(
    fb: *mut DrmFramebuffer,
    dir: c_int,
) -> i32 {
    unsafe {
        let num_planes = (*(*fb).format).num_planes as usize;
        let mut i = 0;
        let mut ret = 0;
        while i < num_planes {
            let obj = drm_gem_fb_get_obj(fb, i as u32);
            if obj.is_null() {
                ret = -EINVAL;
                break;
            }
            let import_attach = (*obj).import_attach.cast::<DmaBufAttachment>();
            if !drm_gem_is_imported(obj) {
                i += 1;
                continue;
            }
            ret = dma_buf_begin_cpu_access((*import_attach).dmabuf, dir);
            if ret != 0 {
                break;
            }
            i += 1;
        }

        if ret != 0 {
            __drm_gem_fb_end_cpu_access(fb, dir, i as u32);
            return ret;
        }

        0
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_end_cpu_access()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_end_cpu_access(fb: *mut DrmFramebuffer, dir: c_int) {
    unsafe {
        __drm_gem_fb_end_cpu_access(fb, dir, (*(*fb).format).num_planes as u32);
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_afbc_get_bpp()
unsafe fn drm_gem_afbc_get_bpp(
    _dev: *mut DrmDevice,
    info: *const DrmFormatInfo,
    _mode_cmd: *const DrmModeFbCmd2,
) -> u32 {
    unsafe {
        match (*info).format {
            DRM_FORMAT_YUV420_8BIT => 12,
            DRM_FORMAT_YUV420_10BIT => 15,
            DRM_FORMAT_VUY101010 => 30,
            _ => drm_format_info_bpp(info, 0) as u32,
        }
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_afbc_min_size()
unsafe fn drm_gem_afbc_min_size(
    dev: *mut DrmDevice,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
    afbc_fb: *mut DrmAfbcFramebuffer,
) -> i32 {
    unsafe {
        let modifier = (*mode_cmd).modifier[0];
        match modifier & AFBC_FORMAT_MOD_BLOCK_SIZE_MASK {
            AFBC_FORMAT_MOD_BLOCK_SIZE_16x16 => {
                (*afbc_fb).block_width = 16;
                (*afbc_fb).block_height = 16;
            }
            AFBC_FORMAT_MOD_BLOCK_SIZE_32x8 => {
                (*afbc_fb).block_width = 32;
                (*afbc_fb).block_height = 8;
            }
            _ => {
                drm_log_at(
                    DrmLogLevel::Debug,
                    "drm",
                    file!(),
                    line!(),
                    &alloc::format!(
                        "Invalid AFBC_FORMAT_MOD_BLOCK_SIZE: {}.\n",
                        modifier & AFBC_FORMAT_MOD_BLOCK_SIZE_MASK
                    ),
                );
                return -EINVAL;
            }
        }

        let mut w_alignment = (*afbc_fb).block_width;
        let mut h_alignment = (*afbc_fb).block_height;
        let mut hdr_alignment = AFBC_HDR_ALIGN;
        if (modifier & AFBC_FORMAT_MOD_TILED) != 0 {
            w_alignment *= AFBC_TH_LAYOUT_ALIGNMENT;
            h_alignment *= AFBC_TH_LAYOUT_ALIGNMENT;
            hdr_alignment = AFBC_TH_BODY_START_ALIGNMENT;
        }

        (*afbc_fb).aligned_width = align_to((*mode_cmd).width, w_alignment);
        (*afbc_fb).aligned_height = align_to((*mode_cmd).height, h_alignment);
        (*afbc_fb).offset = (*mode_cmd).offsets[0];

        let bpp = drm_gem_afbc_get_bpp(dev, info, mode_cmd);
        if bpp == 0 {
            drm_log_at(
                DrmLogLevel::Debug,
                "drm",
                file!(),
                line!(),
                &alloc::format!("Invalid AFBC bpp value: {}\n", bpp),
            );
            return -EINVAL;
        }

        let n_blocks =
            ((*afbc_fb).aligned_width * (*afbc_fb).aligned_height) / AFBC_SUPERBLOCK_PIXELS;
        (*afbc_fb).afbc_size = align_to(n_blocks * AFBC_HEADER_SIZE, hdr_alignment);
        (*afbc_fb).afbc_size +=
            n_blocks * align_to(bpp * AFBC_SUPERBLOCK_PIXELS / 8, AFBC_SUPERBLOCK_ALIGNMENT);

        0
    }
}

// upstream: drm_gem_framebuffer_helper.c drm_gem_fb_afbc_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_gem_fb_afbc_init(
    dev: *mut DrmDevice,
    info: *const DrmFormatInfo,
    mode_cmd: *const DrmModeFbCmd2,
    afbc_fb: *mut DrmAfbcFramebuffer,
) -> i32 {
    unsafe {
        let ret = drm_gem_afbc_min_size(dev, info, mode_cmd, afbc_fb);
        if ret < 0 {
            return ret;
        }

        let obj = (*afbc_fb).base.obj[0];
        if (*obj).size < (*afbc_fb).afbc_size as usize {
            drm_log_at(
                DrmLogLevel::Debug,
                "drm",
                file!(),
                line!(),
                &alloc::format!(
                    "GEM object size ({}) smaller than minimum afbc size ({})\n",
                    (*obj).size,
                    (*afbc_fb).afbc_size
                ),
            );
            return -EINVAL;
        }

        0
    }
}
