// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux 7.2.3 Intel fbdev client, BIOS framebuffer reuse, and probe lifecycle.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::{
    ffi::{c_char, c_int, c_long, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs, DrmFbHelperSurfaceSize},
    drm_fourcc_types::DrmFormatInfo,
    drm_fourcc_upstream::{drm_format_info, drm_format_info_bpp, drm_mode_legacy_fb_format},
    drm_framebuffer_types::DrmFramebuffer,
    drm_framebuffer_upstream::{drm_framebuffer_free, drm_framebuffer_remove},
    drm_gem_types::DrmGemObject,
    fb_types::{FbCopyArea, FbFillRect, FbImage, FbInfo, FbOps, FbVarScreenInfo},
    iosys_map_types::IosysMap,
    uapi_drm_consts::{DRM_FORMAT_MOD_LINEAR, DRM_MODE_FB_MODIFIERS, DRM_MODE_ROTATE_0},
    uapi_drm_mode_types::DrmModeFbCmd2,
};

use crate::{
    intel_bo_upstream::{
        intel_bo_fb_mmap, intel_bo_fbdev_create, intel_bo_fbdev_destroy, intel_bo_fbdev_fill_info,
        intel_bo_fbdev_pitch_align, intel_bo_is_shmem,
    },
    intel_display_conversion_upstream::__drm_to_display,
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_device_upstream::intel_display_device_present,
    intel_display_rpm_upstream::{intel_display_rpm_get, intel_display_rpm_put},
    intel_display_types_upstream::{
        IntelCrtc, IntelCrtcState, IntelFbView, IntelFramebuffer, IntelPlaneState,
    },
    intel_fb_upstream::{intel_fb_align_height, intel_fb_view_vtd_guard, intel_framebuffer_create},
    intel_frontbuffer_upstream::{ORIGIN_CPU, intel_frontbuffer_invalidate},
    intel_parent_upstream::{
        intel_display_parent, intel_parent_fb_pin_get_map, intel_parent_fb_pin_ggtt_pin,
        intel_parent_fb_pin_ggtt_unpin,
    },
    intel_plane_upstream::intel_plane_needs_low_address,
};

const EINVAL: i32 = tk_linux_kapi::config::EINVAL;
const ENODEV: i32 = tk_linux_kapi::config::ENODEV;
const ENOMEM: i32 = tk_linux_kapi::config::ENOMEM;
const EOVERFLOW: i32 = tk_linux_kapi::config::EOVERFLOW;
const GFP_KERNEL: u32 = tk_linux_kapi::config::GFP_KERNEL;
const PAGE_SIZE: usize = tk_linux_kapi::config::PAGE_SIZE;

#[repr(C)]
struct IntelFbdev {
    display: *mut IntelDisplay,
    fb: *mut IntelFramebuffer,
    vma: *mut c_void,
}

// The parent-interface ABI erases this record to `void *`. Keep the mirror
// local rather than changing the shared B4d pin-provider interface.
#[repr(C)]
struct IntelFbPinParams {
    view: *const crate::i915_gtt_view_types_upstream::I915GttView,
    alignment: u32,
    phys_alignment: u32,
    vtd_guard: u32,
    needs_cpu_lmem_access: bool,
    needs_low_address: bool,
    needs_physical: bool,
    needs_fence: bool,
}

const _: () = assert!(size_of::<IntelFbPinParams>() == 24);
const _: () = assert!(offset_of!(IntelFbPinParams, view) == 0);
const _: () = assert!(offset_of!(IntelFbPinParams, alignment) == 8);
const _: () = assert!(offset_of!(IntelFbPinParams, phys_alignment) == 12);
const _: () = assert!(offset_of!(IntelFbPinParams, vtd_guard) == 16);
const _: () = assert!(offset_of!(IntelFbPinParams, needs_cpu_lmem_access) == 20);

unsafe extern "C" {
    fn fb_io_read(info: *mut FbInfo, buf: *mut c_char, count: usize, ppos: *mut c_long) -> isize;
    fn fb_io_write(info: *mut FbInfo, buf: *const c_char, count: usize, ppos: *mut c_long)
    -> isize;
    fn cfb_fillrect(info: *mut FbInfo, rect: *const FbFillRect);
    fn cfb_copyarea(info: *mut FbInfo, area: *const FbCopyArea);
    fn cfb_imageblit(info: *mut FbInfo, image: *const FbImage);
    fn fb_set_suspend(info: *mut FbInfo, state: c_int);
}

#[inline]
unsafe fn helper_from_info(info: *mut FbInfo) -> *mut DrmFbHelper {
    if info.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*info).par.cast() }
    }
}

unsafe fn framebuffer_put(fb: *mut DrmFramebuffer) {
    if fb.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::memory::kref_put(
            ptr::addr_of_mut!((*fb).base.refcount),
            drm_framebuffer_free,
        );
    }
}

unsafe fn framebuffer_get(fb: *mut DrmFramebuffer) {
    if !fb.is_null() {
        unsafe { tk_linux_kapi::memory::kref_get(ptr::addr_of_mut!((*fb).base.refcount)) };
    }
}

unsafe extern "C" fn release_pending_fbdev(_drm: *mut c_void, data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let fbdev = data.cast::<IntelFbdev>();
    let display = unsafe { (*fbdev).display };
    unsafe {
        if !(*fbdev).vma.is_null() {
            intel_parent_fb_pin_ggtt_unpin(display, (*fbdev).vma, -1);
            (*fbdev).vma = ptr::null_mut();
        }
        let fb = (*fbdev).fb;
        (*fbdev).fb = ptr::null_mut();
        if !fb.is_null() {
            drm_framebuffer_remove(&mut (*fb).base);
        }
        if !display.is_null() && (*display).fbdev.fbdev == data {
            (*display).fbdev.fbdev = ptr::null_mut();
        }
    }
}

#[inline]
unsafe fn display_from_helper(helper: *mut DrmFbHelper) -> *mut IntelDisplay {
    if helper.is_null() || unsafe { (*helper).client.dev.is_null() } {
        ptr::null_mut()
    } else {
        unsafe { __drm_to_display((*helper).client.dev) }
    }
}

#[inline]
unsafe fn fbdev_from_display(display: *mut IntelDisplay) -> *mut IntelFbdev {
    if display.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*display).fbdev.fbdev.cast() }
    }
}

unsafe fn invalidate_fbdev(display: *mut IntelDisplay) {
    let fbdev = unsafe { fbdev_from_display(display) };
    if fbdev.is_null() || unsafe { (*fbdev).fb.is_null() } {
        return;
    }
    let front = unsafe { (*(*fbdev).fb).frontbuffer };
    if !front.is_null() {
        unsafe { intel_frontbuffer_invalidate(front, ORIGIN_CPU) };
    }
}

unsafe extern "C" fn intel_fbdev_set_par(info: *mut FbInfo) -> c_int {
    if info.is_null() {
        return -EINVAL;
    }
    let ret = unsafe { tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_set_par(info) };
    if ret == 0 {
        let display = unsafe { display_from_helper(helper_from_info(info)) };
        unsafe { invalidate_fbdev(display) };
    }
    ret
}

unsafe extern "C" fn intel_fbdev_blank(blank: c_int, info: *mut FbInfo) -> c_int {
    if info.is_null() {
        return -EINVAL;
    }
    let ret = unsafe { tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_blank(blank, info) };
    if ret == 0 {
        let display = unsafe { display_from_helper(helper_from_info(info)) };
        unsafe { invalidate_fbdev(display) };
    }
    ret
}

unsafe extern "C" fn intel_fbdev_pan_display(
    var: *mut FbVarScreenInfo,
    info: *mut FbInfo,
) -> c_int {
    if info.is_null() || var.is_null() {
        return -EINVAL;
    }
    let ret =
        unsafe { tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_pan_display(var, info) };
    if ret == 0 {
        let display = unsafe { display_from_helper(helper_from_info(info)) };
        unsafe { invalidate_fbdev(display) };
    }
    ret
}

unsafe extern "C" fn intel_fbdev_mmap(
    info: *mut FbInfo,
    vma: *mut tk_linux_kapi::mm::VmAreaStruct,
) -> c_int {
    let helper = unsafe { helper_from_info(info) };
    if helper.is_null() || unsafe { (*helper).fb.is_null() } {
        return -EINVAL;
    }
    unsafe { intel_bo_fb_mmap((*(*helper).fb).obj[0].cast::<c_void>(), vma.cast()) }
}

unsafe extern "C" fn intel_fbdev_fb_destroy(info: *mut FbInfo) {
    let helper = unsafe { helper_from_info(info) };
    if helper.is_null() || unsafe { (*helper).dev.is_null() } {
        return;
    }
    unsafe {
        let display = display_from_helper(helper);
        let fbdev = fbdev_from_display(display);
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_fini(helper);
        if !fbdev.is_null() {
            if !(*fbdev).vma.is_null() {
                intel_parent_fb_pin_ggtt_unpin(display, (*fbdev).vma, -1);
                (*fbdev).vma = ptr::null_mut();
            }
            (*fbdev).fb = ptr::null_mut();
        }
        let fb = (*helper).fb;
        (*helper).fb = ptr::null_mut();
        if !fb.is_null() {
            drm_framebuffer_remove(fb);
        }
        tk_drm_upstream::drm_client_upstream::drm_client_release(ptr::addr_of_mut!(
            (*helper).client
        ));
    }
}

unsafe extern "C" fn intel_fbdev_defio_read(
    info: *mut FbInfo,
    buf: *mut c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    unsafe { fb_io_read(info, buf, count, ppos) }
}

unsafe extern "C" fn intel_fbdev_defio_write(
    info: *mut FbInfo,
    buf: *const c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    if info.is_null() || ppos.is_null() {
        return -(EINVAL as isize);
    }
    let offset = unsafe { *ppos };
    let ret = unsafe { fb_io_write(info, buf, count, ppos) };
    if ret > 0 {
        unsafe {
            tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_damage_range(
                info,
                offset,
                ret as usize,
            )
        };
    }
    ret
}

unsafe extern "C" fn intel_fbdev_defio_fillrect(info: *mut FbInfo, rect: *const FbFillRect) {
    if rect.is_null() {
        return;
    }
    unsafe {
        cfb_fillrect(info, rect);
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_damage_area(
            info,
            (*rect).dx,
            (*rect).dy,
            (*rect).width,
            (*rect).height,
        );
    }
}

unsafe extern "C" fn intel_fbdev_defio_copyarea(info: *mut FbInfo, area: *const FbCopyArea) {
    if area.is_null() {
        return;
    }
    unsafe {
        cfb_copyarea(info, area);
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_damage_area(
            info,
            (*area).dx,
            (*area).dy,
            (*area).width,
            (*area).height,
        );
    }
}

unsafe extern "C" fn intel_fbdev_defio_imageblit(info: *mut FbInfo, image: *const FbImage) {
    if image.is_null() {
        return;
    }
    unsafe {
        cfb_imageblit(info, image);
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_damage_area(
            info,
            (*image).dx,
            (*image).dy,
            (*image).width,
            (*image).height,
        );
    }
}

unsafe extern "C" fn intel_fbdev_dirty(
    helper: *mut DrmFbHelper,
    clip: *mut tk_drm_upstream::uapi_drm_types::DrmClipRect,
) -> c_int {
    if helper.is_null() || clip.is_null() {
        return -EINVAL;
    }
    unsafe {
        if !((*clip).x1 < (*clip).x2 && (*clip).y1 < (*clip).y2) {
            return 0;
        }
        let fb = (*helper).fb;
        if fb.is_null() || (*fb).funcs.is_null() {
            return 0;
        }
        if let Some(dirty) = (*(*fb).funcs).dirty {
            return dirty(fb, ptr::null_mut(), 0, 0, clip, 1);
        }
    }
    0
}

unsafe extern "C" fn intel_fbdev_restore(helper: *mut DrmFbHelper) {
    let display = unsafe { display_from_helper(helper) };
    unsafe { invalidate_fbdev(display) };
}

unsafe fn memset_io(dst: *mut c_void, len: usize) {
    if dst.is_null() {
        return;
    }
    let dst = dst.cast::<u8>();
    for i in 0..len {
        unsafe { ptr::write_volatile(dst.add(i), 0) };
    }
}

unsafe extern "C" fn intel_fbdev_set_suspend(helper: *mut DrmFbHelper, suspend: bool) {
    if helper.is_null() || unsafe { (*helper).info.is_null() } {
        return;
    }
    unsafe {
        let info = (*helper).info;
        if !suspend && !(*helper).fb.is_null() {
            let obj = (*(*helper).fb).obj[0];
            if !obj.is_null() && !intel_bo_is_shmem(obj.cast()) {
                memset_io((*info).screen_base.cast(), (*info).screen_size as usize);
            }
        }
        fb_set_suspend(info, if suspend { 1 } else { 0 });
    }
}

static INTEL_FB_HELPER_FUNCS: DrmFbHelperFuncs = DrmFbHelperFuncs {
    fb_dirty: Some(intel_fbdev_dirty),
    fb_restore: Some(intel_fbdev_restore),
    fb_set_suspend: Some(intel_fbdev_set_suspend),
};

#[repr(transparent)]
struct SyncFbOps(FbOps);
unsafe impl Sync for SyncFbOps {}

static INTEL_FB_OPS: SyncFbOps = SyncFbOps(FbOps {
    owner: ptr::null_mut(),
    fb_open: None,
    fb_release: None,
    fb_read: Some(intel_fbdev_defio_read),
    fb_write: Some(intel_fbdev_defio_write),
    fb_check_var: Some(tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_check_var),
    fb_set_par: Some(intel_fbdev_set_par),
    fb_setcolreg: None,
    fb_setcmap: Some(tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_setcmap),
    fb_blank: Some(intel_fbdev_blank),
    fb_pan_display: Some(intel_fbdev_pan_display),
    fb_fillrect: Some(intel_fbdev_defio_fillrect),
    fb_copyarea: Some(intel_fbdev_defio_copyarea),
    fb_imageblit: Some(intel_fbdev_defio_imageblit),
    fb_cursor: None,
    fb_sync: None,
    fb_ioctl: Some(tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_ioctl),
    fb_compat_ioctl: None,
    fb_mmap: Some(intel_fbdev_mmap),
    fb_get_caps: None,
    fb_destroy: Some(intel_fbdev_fb_destroy),
});

unsafe fn log_error(display: *mut IntelDisplay, message: &'static str, error: i32) {
    if display.is_null() || unsafe { (*display).drm.is_null() } {
        return;
    }
    unsafe {
        tk_linux_kapi::print::drm_log_at(
            tk_linux_kapi::print::DrmLogLevel::Error,
            "i915",
            file!(),
            line!(),
            &alloc::format!("{message}: {error}"),
        )
    };
}

unsafe fn fbdev_parent_ops_complete(display: *mut IntelDisplay) -> bool {
    let parent = unsafe { intel_display_parent(display) };
    if parent.is_null() {
        return false;
    }
    let bo = unsafe { (*parent).bo };
    let pin = unsafe { (*parent).fb_pin };
    !bo.is_null()
        && !pin.is_null()
        && unsafe {
            (*bo).is_shmem.is_some()
                && (*bo).fb_mmap.is_some()
                && (*bo).fbdev_create.is_some()
                && (*bo).fbdev_destroy.is_some()
                && (*bo).fbdev_fill_info.is_some()
                && (*bo).fbdev_pitch_align.is_some()
                && (*pin).ggtt_pin.is_some()
                && (*pin).ggtt_unpin.is_some()
                && (*pin).get_map.is_some()
        }
}

unsafe fn fill_mode_cmd(
    display: *mut IntelDisplay,
    sizes: *mut DrmFbHelperSurfaceSize,
    mode_cmd: *mut DrmModeFbCmd2,
) -> i32 {
    if display.is_null() || sizes.is_null() || mode_cmd.is_null() {
        return -EINVAL;
    }
    unsafe { ptr::write_bytes(mode_cmd, 0, 1) };
    if unsafe { (*sizes).surface_bpp == 24 } {
        unsafe { (*sizes).surface_bpp = 32 };
    }
    let width = unsafe { (*sizes).surface_width };
    let height = unsafe { (*sizes).surface_height };
    let bpp = unsafe { (*sizes).surface_bpp };
    let depth = unsafe { (*sizes).surface_depth };
    if width == 0 || height == 0 || bpp == 0 {
        return -EINVAL;
    }
    let bytes = bpp.div_ceil(8);
    let raw_pitch = match width.checked_mul(bytes) {
        Some(pitch) => pitch,
        None => return -EOVERFLOW,
    };
    let pitch = unsafe { intel_bo_fbdev_pitch_align(display, raw_pitch) };
    if pitch < raw_pitch || pitch == 0 {
        return -EINVAL;
    }
    let format = drm_mode_legacy_fb_format(bpp, depth);
    if drm_format_info(format).is_null() {
        return -EINVAL;
    }
    unsafe {
        (*mode_cmd).flags = DRM_MODE_FB_MODIFIERS;
        (*mode_cmd).width = width;
        (*mode_cmd).height = height;
        (*mode_cmd).pitches[0] = pitch;
        (*mode_cmd).pixel_format = format;
        (*mode_cmd).modifier[0] = DRM_FORMAT_MOD_LINEAR;
    }
    0
}

fn page_aligned_fb_size(pitch: u32, height: u32) -> Option<i32> {
    let bytes = (pitch as usize).checked_mul(height as usize)?;
    let size = bytes.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    if size == 0 || size > i32::MAX as usize {
        None
    } else {
        Some(size as i32)
    }
}

unsafe fn fbdev_fb_alloc(
    display: *mut IntelDisplay,
    sizes: *mut DrmFbHelperSurfaceSize,
) -> Result<*mut IntelFramebuffer, i32> {
    let mut mode_cmd: DrmModeFbCmd2 = unsafe { core::mem::zeroed() };
    let ret = unsafe { fill_mode_cmd(display, sizes, &mut mode_cmd) };
    if ret != 0 {
        return Err(ret);
    }
    let Some(size) = page_aligned_fb_size(mode_cmd.pitches[0], mode_cmd.height) else {
        return Err(-EOVERFLOW);
    };
    let obj = unsafe { intel_bo_fbdev_create(display, size) };
    if obj.is_null() {
        return Err(-ENODEV);
    }
    if tk_linux_kapi::config::IS_ERR(obj) {
        return Err(tk_linux_kapi::config::PTR_ERR(obj));
    }
    let info = drm_format_info(mode_cmd.pixel_format);
    let fb = unsafe { intel_framebuffer_create(obj.cast::<DrmGemObject>(), info, &mut mode_cmd) };
    if fb.is_null() || tk_linux_kapi::config::IS_ERR(fb) {
        let error = if fb.is_null() {
            -ENOMEM
        } else {
            tk_linux_kapi::config::PTR_ERR(fb)
        };
        unsafe { intel_bo_fbdev_destroy(obj) };
        return Err(error);
    }
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(obj.cast()) };
    Ok(fb.cast())
}

unsafe fn fbdev_init_bios(display: *mut IntelDisplay, fbdev: *mut IntelFbdev) -> bool {
    if display.is_null() || fbdev.is_null() || unsafe { (*display).drm.is_null() } {
        return false;
    }
    let drm = unsafe { (*display).drm };
    let head = unsafe { ptr::addr_of_mut!((*drm).mode_config.crtc_list) };
    let mut candidate: *mut IntelFramebuffer = ptr::null_mut();
    let mut max_size = 0usize;
    let mut node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let crtc = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(tk_drm_upstream::drm_crtc_types::DrmCrtc, head))
                .cast::<IntelCrtc>()
        };
        let next = unsafe { (*node).next };
        let crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        let primary = unsafe {
            (*crtc)
                .base
                .primary
                .cast::<crate::intel_display_types_upstream::IntelPlane>()
        };
        let plane_state = if primary.is_null() {
            ptr::null_mut()
        } else {
            unsafe { (*primary).base.state.cast::<IntelPlaneState>() }
        };
        if !crtc_state.is_null()
            && unsafe { (*crtc_state).uapi.active }
            && !plane_state.is_null()
            && !unsafe { (*plane_state).uapi.fb.is_null() }
        {
            let fb = unsafe { (*plane_state).uapi.fb.cast::<IntelFramebuffer>() };
            let obj = unsafe { (*fb).base.obj[0] };
            if !obj.is_null() && unsafe { (*obj).size } > max_size {
                candidate = fb;
                max_size = unsafe { (*obj).size };
            }
        }
        node = next;
    }
    if candidate.is_null() {
        return false;
    }

    // The BIOS surface must cover every active primary. Use its stride and
    // format, while each CRTC contributes the dimensions it currently scans.
    let mut valid = true;
    node = unsafe { (*head).next };
    while !node.is_null() && node != head {
        let crtc = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(tk_drm_upstream::drm_crtc_types::DrmCrtc, head))
                .cast::<IntelCrtc>()
        };
        let next = unsafe { (*node).next };
        let crtc_state = unsafe { (*crtc).base.state.cast::<IntelCrtcState>() };
        if !crtc_state.is_null() && unsafe { (*crtc_state).uapi.active } {
            let width = unsafe { (*crtc_state).uapi.adjusted_mode.crtc_hdisplay as usize };
            let height = unsafe { (*crtc_state).uapi.adjusted_mode.crtc_vdisplay as u32 };
            let format = unsafe { (*candidate).base.format };
            if format.is_null() {
                valid = false;
                break;
            }
            let cpp = unsafe { (*format)._anon_6.cpp[0] as usize };
            let min_pitch = match width.checked_mul(cpp) {
                Some(value) => value,
                None => {
                    valid = false;
                    break;
                }
            };
            let aligned_height = unsafe { intel_fb_align_height(&(*candidate).base, 0, height) };
            let need = match (aligned_height as usize)
                .checked_mul(unsafe { (*candidate).base.pitches[0] as usize })
            {
                Some(value) => value,
                None => {
                    valid = false;
                    break;
                }
            };
            if unsafe { (*candidate).base.pitches[0] as usize } < min_pitch || need > max_size {
                valid = false;
                break;
            }
        }
        node = next;
    }
    if !valid {
        return false;
    }
    unsafe {
        framebuffer_get(&mut (*candidate).base);
        (*fbdev).fb = candidate;
    }
    true
}

unsafe fn fbdev_color_mode(info: *const DrmFormatInfo) -> u32 {
    if info.is_null()
        || unsafe {
            (*info).depth == 0 || (*info).num_planes != 1 || (*info).has_alpha || (*info).is_yuv
        }
    {
        return 0;
    }
    let bpp = unsafe { drm_format_info_bpp(info, 0) };
    if bpp == 16 {
        unsafe { (*info).depth as u32 }
    } else {
        bpp
    }
}

/// DRM driver's `fbdev_probe` callback. It uses only the driver's registered
/// framebuffer/BO and GGTT pin providers; a missing provider is an error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbdev_driver_fbdev_probe(
    helper: *mut DrmFbHelper,
    sizes: *mut DrmFbHelperSurfaceSize,
) -> c_int {
    if helper.is_null()
        || sizes.is_null()
        || unsafe { (*helper).dev.is_null() || (*helper).info.is_null() }
    {
        return -EINVAL;
    }
    let display = unsafe { __drm_to_display((*helper).dev) };
    if display.is_null() || !unsafe { intel_display_device_present(display) } {
        return -ENODEV;
    }
    if !unsafe { fbdev_parent_ops_complete(display) } {
        unsafe { log_error(display, "fbdev parent providers are incomplete", -ENODEV) };
        return -ENODEV;
    }
    let fbdev = unsafe { fbdev_from_display(display) };
    if fbdev.is_null() {
        return -ENODEV;
    }
    let mut fb = unsafe { (*fbdev).fb };
    unsafe { (*fbdev).fb = ptr::null_mut() };
    let mut prealloc = false;
    if !fb.is_null()
        && (unsafe { (*sizes).fb_width > (*fb).base.width }
            || unsafe { (*sizes).fb_height > (*fb).base.height })
    {
        unsafe { framebuffer_put(&mut (*fb).base) };
        fb = ptr::null_mut();
    }

    let wakeref = unsafe { intel_display_rpm_get(display) };
    let mut allocated = false;
    if fb.is_null() || unsafe { crate::intel_fb_upstream::intel_fb_bo(&(*fb).base).is_null() } {
        match unsafe { fbdev_fb_alloc(display, sizes) } {
            Ok(created) => {
                fb = created;
                allocated = true;
            }
            Err(error) => {
                unsafe { intel_display_rpm_put(display, wakeref) };
                return error;
            }
        }
    } else {
        prealloc = true;
        unsafe {
            (*sizes).fb_width = (*fb).base.width;
            (*sizes).fb_height = (*fb).base.height;
        }
    }

    let obj = unsafe { (*fb).base.obj[0] };
    if obj.is_null() {
        unsafe { intel_display_rpm_put(display, wakeref) };
        if allocated {
            unsafe { drm_framebuffer_remove(&mut (*fb).base) };
        } else {
            unsafe { framebuffer_put(&mut (*fb).base) };
        }
        return -ENODEV;
    }
    let params = IntelFbPinParams {
        view: ptr::addr_of!((*fb).normal_view.gtt),
        alignment: unsafe { (*fb).min_alignment },
        phys_alignment: 0,
        vtd_guard: unsafe {
            intel_fb_view_vtd_guard(
                &(*fb).base,
                ptr::addr_of!((*fb).normal_view),
                DRM_MODE_ROTATE_0,
            )
        },
        needs_cpu_lmem_access: false,
        needs_low_address: unsafe { intel_plane_needs_low_address(display) },
        needs_physical: false,
        needs_fence: false,
    };
    let mut vma = ptr::null_mut();
    let mut offset = 0;
    let ret = unsafe {
        intel_parent_fb_pin_ggtt_pin(
            display,
            obj.cast(),
            (&params as *const IntelFbPinParams).cast(),
            &mut vma,
            &mut offset,
            ptr::null_mut(),
        )
    };
    let ret = if ret == 0 && vma.is_null() {
        -ENODEV
    } else {
        ret
    };
    if ret != 0 {
        if !vma.is_null() {
            unsafe { intel_parent_fb_pin_ggtt_unpin(display, vma, -1) };
        }
        unsafe { intel_display_rpm_put(display, wakeref) };
        if allocated {
            unsafe { drm_framebuffer_remove(&mut (*fb).base) };
        } else {
            unsafe { framebuffer_put(&mut (*fb).base) };
        }
        return ret;
    }

    unsafe {
        (*helper).funcs = (&INTEL_FB_HELPER_FUNCS as *const DrmFbHelperFuncs).cast_mut();
        (*helper).fb = &mut (*fb).base;
        (*(*helper).info).fbops = &INTEL_FB_OPS.0;
    }
    let info = unsafe { (*helper).info };
    let ret = unsafe { intel_bo_fbdev_fill_info(obj.cast(), info.cast(), vma) };
    if ret != 0 {
        unsafe { intel_parent_fb_pin_ggtt_unpin(display, vma, -1) };
        unsafe { (*helper).fb = ptr::null_mut() };
        unsafe { intel_display_rpm_put(display, wakeref) };
        if allocated {
            unsafe { drm_framebuffer_remove(&mut (*fb).base) };
        } else {
            unsafe { framebuffer_put(&mut (*fb).base) };
        }
        return ret;
    }
    unsafe {
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_fill_info(info, helper, sizes);
    }
    let visible_size = unsafe { (*fb).base.height as usize }
        .checked_mul(unsafe { (*fb).base.pitches[0] as usize });
    if unsafe { (*info).screen_base.is_null() }
        || visible_size.is_none()
        || unsafe { ((*info).screen_size as usize) < visible_size.unwrap_or(usize::MAX) }
    {
        unsafe { intel_parent_fb_pin_ggtt_unpin(display, vma, -1) };
        unsafe { (*helper).fb = ptr::null_mut() };
        unsafe { intel_display_rpm_put(display, wakeref) };
        if allocated {
            unsafe { drm_framebuffer_remove(&mut (*fb).base) };
        } else {
            unsafe { framebuffer_put(&mut (*fb).base) };
        }
        return -EINVAL;
    }

    if !unsafe { intel_bo_is_shmem(obj.cast()) } && !prealloc {
        unsafe { memset_io((*info).screen_base.cast(), (*info).screen_size as usize) };
    }
    unsafe {
        (*fbdev).fb = fb;
        (*fbdev).vma = vma;
        intel_display_rpm_put(display, wakeref);
    }
    0
}

/// Initialize the DRM fbdev client and transfer a reference to a usable BIOS
/// primary framebuffer when all active pipes can share it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbdev_setup(display: *mut IntelDisplay) {
    if display.is_null()
        || !unsafe { intel_display_device_present(display) }
        || unsafe { (*display).drm.is_null() }
    {
        return;
    }
    if !unsafe { fbdev_parent_ops_complete(display) } {
        unsafe { log_error(display, "fbdev parent providers are incomplete", -ENODEV) };
        return;
    }
    if unsafe { (*display).fbdev.fbdev.is_null() } {
        let dev = unsafe { (*display).drm };
        let fbdev = unsafe {
            tk_linux_kapi::memory::drmm_kzalloc_c(dev.cast(), size_of::<IntelFbdev>(), GFP_KERNEL)
        }
        .cast::<IntelFbdev>();
        if fbdev.is_null() {
            unsafe { log_error(display, "failed to allocate Intel fbdev", -ENOMEM) };
            return;
        }
        unsafe {
            (*fbdev).display = display;
            (*display).fbdev.fbdev = fbdev.cast();
        }
        let ret = unsafe {
            tk_linux_kapi::kernel_core::drmm_add_action_or_reset(
                dev.cast(),
                release_pending_fbdev,
                fbdev.cast(),
            )
        };
        if ret != 0 {
            unsafe {
                (*display).fbdev.fbdev = ptr::null_mut();
                log_error(display, "failed to register fbdev cleanup", ret);
            }
            return;
        }
    }
    let driver = unsafe { (*(*display).drm).driver };
    if driver.is_null() {
        unsafe { log_error(display, "DRM driver is not installed", -ENODEV) };
        return;
    }
    unsafe {
        (*driver).fbdev_probe = Some(intel_fbdev_driver_fbdev_probe);
    }

    let fbdev = unsafe { fbdev_from_display(display) };
    let has_bios_fb = unsafe { fbdev_init_bios(display, fbdev) };
    let mut preferred_bpp = 0;
    if has_bios_fb {
        let fb = unsafe { (*fbdev).fb };
        preferred_bpp = unsafe { fbdev_color_mode((*fb).base.format) };
    }
    if preferred_bpp == 0 {
        preferred_bpp = 32;
    }
    unsafe {
        tk_drm_upstream::drm_client_setup_upstream::drm_client_setup_with_color_mode(
            (*display).drm,
            preferred_bpp,
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbdev_framebuffer(fbdev: *mut c_void) -> *mut IntelFramebuffer {
    if fbdev.is_null() {
        ptr::null_mut()
    } else {
        unsafe { (*fbdev.cast::<IntelFbdev>()).fb }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_fbdev_get_map(display: *mut IntelDisplay, map: *mut IosysMap) {
    if map.is_null() {
        return;
    }
    unsafe { ptr::write_bytes(map, 0, 1) };
    if display.is_null() || unsafe { (*display).fbdev.fbdev.is_null() } {
        return;
    }
    let fbdev = unsafe { fbdev_from_display(display) };
    if fbdev.is_null() || unsafe { (*fbdev).vma.is_null() } {
        return;
    }
    unsafe { intel_parent_fb_pin_get_map(display, (*fbdev).vma, map.cast()) };
}

#[cfg(test)]
mod tests {
    use super::page_aligned_fb_size;

    #[test]
    fn fbdev_storage_is_page_aligned_and_bounded_by_parent_abi() {
        assert_eq!(page_aligned_fb_size(1920, 1080), Some(2_076_672));
        assert_eq!(page_aligned_fb_size(4, 1), Some(4096));
        assert_eq!(page_aligned_fb_size(0, 480), None);
        assert_eq!(page_aligned_fb_size(u32::MAX, u32::MAX), None);
    }
}
