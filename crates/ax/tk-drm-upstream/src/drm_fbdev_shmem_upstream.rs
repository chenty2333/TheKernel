// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_fbdev_shmem.c` (Virtio V0).
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_char, c_int, c_long, c_void},
    ptr,
};

use tk_linux_kapi::{
    assert::warn_on,
    config::{ENODEV, HZ, PAGE_SHIFT},
    shmem::get_page,
    types::{ListHead, Page},
};

use crate::{
    drm_client_types::{DrmClientBuffer, DrmClientDev},
    drm_device_types::DrmDevice,
    drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs, DrmFbHelperSurfaceSize},
    drm_framebuffer_types::DrmFramebuffer,
    drm_gem_shmem_helper_types_upstream::{DRM_GEM_SHMEM_MAP_WC, DrmGemShmemObject},
    drm_gem_types::DrmGemObject,
    fb_types::{
        FBINFO_READS_FAST, FBINFO_VIRTFB, FbCopyArea, FbDeferredIo, FbFillRect, FbImage, FbInfo,
        FbOps,
    },
    iosys_map_types::IosysMap,
    uapi_drm_types::DrmClipRect,
};

unsafe extern "C" {
    fn fb_deferred_io_init(info: *mut FbInfo) -> c_int;
    fn fb_deferred_io_cleanup(info: *mut FbInfo);
    fn fb_deferred_io_mmap(info: *mut FbInfo, vma: *mut tk_linux_kapi::mm::VmAreaStruct) -> c_int;
    fn fb_sys_read(info: *mut FbInfo, buf: *mut c_char, count: usize, ppos: *mut c_long) -> isize;
    fn fb_sys_write(
        info: *mut FbInfo,
        buf: *const c_char,
        count: usize,
        ppos: *mut c_long,
    ) -> isize;
    fn sys_fillrect(info: *mut FbInfo, rect: *const FbFillRect);
    fn sys_copyarea(info: *mut FbInfo, area: *const FbCopyArea);
    fn sys_imageblit(info: *mut FbInfo, image: *const FbImage);

    fn drm_fb_helper_check_var(
        var: *mut crate::fb_types::FbVarScreenInfo,
        info: *mut FbInfo,
    ) -> c_int;
    fn drm_fb_helper_set_par(info: *mut FbInfo) -> c_int;
    fn drm_fb_helper_setcmap(cmap: *mut crate::fb_types::FbCmap, info: *mut FbInfo) -> c_int;
    fn drm_fb_helper_blank(blank: c_int, info: *mut FbInfo) -> c_int;
    fn drm_fb_helper_pan_display(
        var: *mut crate::fb_types::FbVarScreenInfo,
        info: *mut FbInfo,
    ) -> c_int;
    fn drm_fb_helper_ioctl(info: *mut FbInfo, cmd: u32, arg: core::ffi::c_ulong) -> c_int;
    fn drm_fb_helper_fill_info(
        info: *mut FbInfo,
        helper: *mut DrmFbHelper,
        sizes: *const DrmFbHelperSurfaceSize,
    );
    fn drm_fb_helper_fini(helper: *mut DrmFbHelper);
    fn drm_fb_helper_deferred_io(info: *mut FbInfo, pageref_list: *mut ListHead);
    fn drm_fb_helper_damage_range(info: *mut FbInfo, offset: c_long, len: usize);
    fn drm_fb_helper_damage_area(info: *mut FbInfo, x: u32, y: u32, width: u32, height: u32);

    fn drm_client_buffer_create_dumb(
        client: *mut DrmClientDev,
        width: u32,
        height: u32,
        format: u32,
    ) -> *mut DrmClientBuffer;
    fn drm_client_buffer_vmap(buffer: *mut DrmClientBuffer, map: *mut IosysMap) -> c_int;
    fn drm_client_buffer_vunmap(buffer: *mut DrmClientBuffer);
    fn drm_client_buffer_delete(buffer: *mut DrmClientBuffer);
    fn drm_client_release(client: *mut DrmClientDev);
}

unsafe fn helper_from_info(info: *mut FbInfo) -> *mut DrmFbHelper {
    unsafe { (*info).par.cast() }
}

unsafe fn gem_from_fb(fb: *mut DrmFramebuffer, plane: u32) -> *mut DrmGemObject {
    unsafe { crate::drm_gem_framebuffer_helper_upstream::drm_gem_fb_get_obj(fb, plane) }
}

#[inline]
unsafe fn shmem_from_gem(obj: *mut DrmGemObject) -> *mut DrmGemShmemObject {
    obj.cast()
}

// Linux built-in drivers have NULL fops->owner; module reference operations
// are therefore the no-op case for this built-in kernel target.
unsafe extern "C" fn drm_fbdev_shmem_fb_open(info: *mut FbInfo, user: c_int) -> c_int {
    let _ = (info, user);
    0
}

unsafe extern "C" fn drm_fbdev_shmem_fb_release(info: *mut FbInfo, user: c_int) -> c_int {
    let _ = (info, user);
    0
}

// upstream macro: FB_GEN_DEFAULT_DEFERRED_SYSMEM_OPS()
unsafe extern "C" fn drm_fbdev_shmem_defio_read(
    info: *mut FbInfo,
    buf: *mut c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    unsafe { fb_sys_read(info, buf, count, ppos) }
}

unsafe extern "C" fn drm_fbdev_shmem_defio_write(
    info: *mut FbInfo,
    buf: *const c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    unsafe {
        let offset = *ppos;
        let ret = fb_sys_write(info, buf, count, ppos);
        if ret > 0 {
            drm_fb_helper_damage_range(info, offset, ret as usize);
        }
        ret
    }
}

unsafe extern "C" fn drm_fbdev_shmem_defio_fillrect(info: *mut FbInfo, rect: *const FbFillRect) {
    unsafe {
        sys_fillrect(info, rect);
        drm_fb_helper_damage_area(info, (*rect).dx, (*rect).dy, (*rect).width, (*rect).height);
    }
}

unsafe extern "C" fn drm_fbdev_shmem_defio_copyarea(info: *mut FbInfo, area: *const FbCopyArea) {
    unsafe {
        sys_copyarea(info, area);
        drm_fb_helper_damage_area(info, (*area).dx, (*area).dy, (*area).width, (*area).height);
    }
}

unsafe extern "C" fn drm_fbdev_shmem_defio_imageblit(info: *mut FbInfo, image: *const FbImage) {
    unsafe {
        sys_imageblit(info, image);
        drm_fb_helper_damage_area(
            info,
            (*image).dx,
            (*image).dy,
            (*image).width,
            (*image).height,
        );
    }
}

// upstream: drm_fbdev_shmem.c drm_fbdev_shmem_fb_mmap()
unsafe extern "C" fn drm_fbdev_shmem_fb_mmap(
    info: *mut FbInfo,
    vma: *mut tk_linux_kapi::mm::VmAreaStruct,
) -> c_int {
    unsafe {
        let helper = helper_from_info(info);
        let obj = gem_from_fb((*helper).fb, 0);
        let shmem = shmem_from_gem(obj);
        if (*shmem).flags & DRM_GEM_SHMEM_MAP_WC != 0 {
            (*vma).vm_page_prot = tk_linux_kapi::vm::pgprot_writecombine((*vma).vm_page_prot);
        }
        fb_deferred_io_mmap(info, vma)
    }
}

// upstream: drm_fbdev_shmem.c drm_fbdev_shmem_fb_destroy()
unsafe extern "C" fn drm_fbdev_shmem_fb_destroy(info: *mut FbInfo) {
    unsafe {
        let helper = helper_from_info(info);
        if (*helper).dev.is_null() {
            return;
        }
        fb_deferred_io_cleanup(info);
        drm_fb_helper_fini(helper);
        drm_client_buffer_vunmap((*helper).buffer);
        drm_client_buffer_delete((*helper).buffer);
        drm_client_release(ptr::addr_of_mut!((*helper).client));
    }
}

#[repr(transparent)]
struct SyncFbOps(FbOps);
unsafe impl Sync for SyncFbOps {}

static DRM_FBDEV_SHMEM_FB_OPS: SyncFbOps = SyncFbOps(FbOps {
    owner: ptr::null_mut(),
    fb_open: Some(drm_fbdev_shmem_fb_open),
    fb_release: Some(drm_fbdev_shmem_fb_release),
    fb_read: Some(drm_fbdev_shmem_defio_read),
    fb_write: Some(drm_fbdev_shmem_defio_write),
    fb_check_var: Some(drm_fb_helper_check_var),
    fb_set_par: Some(drm_fb_helper_set_par),
    fb_setcolreg: None,
    fb_setcmap: Some(drm_fb_helper_setcmap),
    fb_blank: Some(drm_fb_helper_blank),
    fb_pan_display: Some(drm_fb_helper_pan_display),
    fb_fillrect: Some(drm_fbdev_shmem_defio_fillrect),
    fb_copyarea: Some(drm_fbdev_shmem_defio_copyarea),
    fb_imageblit: Some(drm_fbdev_shmem_defio_imageblit),
    fb_cursor: None,
    fb_sync: None,
    fb_ioctl: Some(drm_fb_helper_ioctl),
    fb_compat_ioctl: None,
    fb_mmap: Some(drm_fbdev_shmem_fb_mmap),
    fb_get_caps: None,
    fb_destroy: Some(drm_fbdev_shmem_fb_destroy),
});

unsafe extern "C" fn drm_fbdev_shmem_get_page(info: *mut FbInfo, offset: u64) -> *mut Page {
    unsafe {
        let helper = helper_from_info(info);
        let fb = (*helper).fb;
        let obj = gem_from_fb(fb, 0);
        let shmem = shmem_from_gem(obj);
        if offset >= (*obj).size as u64 {
            warn_on(true);
            return ptr::null_mut();
        }
        let index = (offset >> PAGE_SHIFT) as usize;
        let page = *(*shmem).pages.add(index);
        if !page.is_null() {
            get_page(page);
        } else {
            warn_on(true);
        }
        page
    }
}

// upstream: drm_fbdev_shmem.c drm_fbdev_shmem_helper_fb_dirty()
unsafe extern "C" fn drm_fbdev_shmem_helper_fb_dirty(
    helper: *mut DrmFbHelper,
    clip: *mut DrmClipRect,
) -> c_int {
    unsafe {
        if !((*clip).x1 < (*clip).x2 && (*clip).y1 < (*clip).y2) {
            return 0;
        }
        let funcs = (*(*helper).fb).funcs;
        if !funcs.is_null() {
            if let Some(dirty) = (*funcs).dirty {
                let ret = dirty((*helper).fb, ptr::null_mut(), 0, 0, clip, 1);
                if warn_on(ret != 0) {
                    return ret;
                }
            }
        }
        0
    }
}

static DRM_FBDEV_SHMEM_HELPER_FUNCS: DrmFbHelperFuncs = DrmFbHelperFuncs {
    fb_dirty: Some(drm_fbdev_shmem_helper_fb_dirty),
    fb_restore: None,
    fb_set_suspend: None,
};

// upstream: drm_fbdev_shmem.c drm_fbdev_shmem_driver_fbdev_probe()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fbdev_shmem_driver_fbdev_probe(
    helper: *mut DrmFbHelper,
    sizes: *mut DrmFbHelperSurfaceSize,
) -> c_int {
    unsafe {
        let client = ptr::addr_of_mut!((*helper).client);
        let dev = (*helper).dev;
        let info = (*helper).info;
        let format = crate::drm_fourcc_upstream::drm_driver_legacy_fb_format(
            dev,
            (*sizes).surface_bpp,
            (*sizes).surface_depth,
        );
        let buffer = drm_client_buffer_create_dumb(
            client,
            (*sizes).surface_width,
            (*sizes).surface_height,
            format,
        );
        if tk_linux_kapi::config::IS_ERR(buffer) {
            return tk_linux_kapi::config::PTR_ERR(buffer);
        }
        let gem = (*buffer).gem;
        let shmem = shmem_from_gem(gem);
        let fb = (*buffer).fb;
        let mut map: IosysMap = core::mem::zeroed();
        let ret = drm_client_buffer_vmap(buffer, &mut map);
        if ret != 0 {
            drm_client_buffer_delete(buffer);
            return ret;
        }
        if map.is_iomem {
            drm_client_buffer_vunmap(buffer);
            drm_client_buffer_delete(buffer);
            return -ENODEV;
        }

        (*helper).funcs =
            (&DRM_FBDEV_SHMEM_HELPER_FUNCS as *const DrmFbHelperFuncs) as *mut DrmFbHelperFuncs;
        (*helper).buffer = buffer;
        (*helper).fb = fb;
        drm_fb_helper_fill_info(info, helper, sizes);
        (*info).fbops = &DRM_FBDEV_SHMEM_FB_OPS.0;
        (*info).flags |= FBINFO_VIRTFB as c_int;
        if (*shmem).flags & DRM_GEM_SHMEM_MAP_WC == 0 {
            (*info).flags |= FBINFO_READS_FAST as c_int;
        }
        let screen_size = (*sizes).surface_height.wrapping_mul((*fb).pitches[0]) as usize;
        (*info).screen_size = screen_size as _;
        (*info).screen_base = map._anon_0.vaddr.cast();
        (*info).fix.smem_len = screen_size as u32;

        (*helper).fbdefio.delay = (HZ / 20) as u64;
        (*helper).fbdefio.get_page = Some(drm_fbdev_shmem_get_page);
        (*helper).fbdefio.deferred_io = Some(drm_fb_helper_deferred_io);
        (*info).fbdefio = ptr::addr_of_mut!((*helper).fbdefio);
        let ret = fb_deferred_io_init(info);
        if ret != 0 {
            (*helper).fb = ptr::null_mut();
            (*helper).buffer = ptr::null_mut();
            drm_client_buffer_vunmap(buffer);
            drm_client_buffer_delete(buffer);
            return ret;
        }
        0
    }
}
