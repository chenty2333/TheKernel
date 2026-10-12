// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_rect.c` (batch B1b).
//!
//! Source-faithful transcription of Linux 7.2.3. Header-inline helpers from
//! `include/drm/drm_rect.h` are provided as plain functions below.

use core::ffi::c_char;

use alloc::format;
use tk_linux_kapi::config::{EINVAL, ERANGE};
use tk_linux_kapi::print::{drm_log_at, DrmLogLevel};
use tk_linux_kapi::assert::warn_on;

use crate::drm_rect_types::DrmRect;
use crate::uapi_drm_consts::{
    DRM_MODE_REFLECT_X, DRM_MODE_REFLECT_Y, DRM_MODE_ROTATE_180, DRM_MODE_ROTATE_270,
    DRM_MODE_ROTATE_0, DRM_MODE_ROTATE_90, DRM_MODE_ROTATE_MASK,
};

// Copy of a `struct drm_rect` (C struct assignment `tmp = *r`).
#[inline]
fn copy_rect(r: &DrmRect) -> DrmRect {
    DrmRect { x1: r.x1, y1: r.y1, x2: r.x2, y2: r.y2 }
}

// upstream: include/drm/drm_rect.h drm_rect_width()
#[inline]
pub fn drm_rect_width(r: &DrmRect) -> i32 {
    r.x2 - r.x1
}

// upstream: include/drm/drm_rect.h drm_rect_height()
#[inline]
pub fn drm_rect_height(r: &DrmRect) -> i32 {
    r.y2 - r.y1
}

// upstream: include/drm/drm_rect.h drm_rect_visible()
#[inline]
pub fn drm_rect_visible(r: &DrmRect) -> bool {
    drm_rect_width(r) > 0 && drm_rect_height(r) > 0
}

// upstream: include/drm/drm_rect.h drm_rect_init()
#[inline]
pub fn drm_rect_init(r: &mut DrmRect, x: i32, y: i32, width: i32, height: i32) {
    r.x1 = x;
    r.y1 = y;
    r.x2 = x.wrapping_add(width);
    r.y2 = y.wrapping_add(height);
}

// upstream: drm_rect.c drm_rect_intersect()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_intersect(r1: *mut DrmRect, r2: *const DrmRect) -> bool {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let (r1, r2) = unsafe { (&mut *r1, &*r2) };
    r1.x1 = r1.x1.max(r2.x1);
    r1.y1 = r1.y1.max(r2.y1);
    r1.x2 = r1.x2.min(r2.x2);
    r1.y2 = r1.y2.min(r2.y2);
    drm_rect_visible(r1)
}

// upstream: drm_rect.c clip_scaled()
fn clip_scaled(src: i32, dst: i32, clip: &mut i32) -> u32 {
    if dst == 0 {
        return 0;
    }
    *clip = (*clip).min(dst);
    let tmp: u64 = (src as u32 as u64) * ((dst - *clip) as u32 as u64);
    if src < dst.wrapping_shl(16) {
        // DIV_ROUND_UP_ULL(tmp, dst)
        ((tmp + dst as u64 - 1) / dst as u64) as u32
    } else {
        // DIV_ROUND_DOWN_ULL(tmp, dst)
        (tmp / dst as u64) as u32
    }
}

// upstream: drm_rect.c drm_rect_clip_scaled()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_clip_scaled(
    src: *mut DrmRect,
    dst: *mut DrmRect,
    clip: *const DrmRect,
) -> bool {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let (src, dst, clip) = unsafe { (&mut *src, &mut *dst, &*clip) };
    let mut diff: i32;

    diff = clip.x1 - dst.x1;
    if diff > 0 {
        let new_src_w = clip_scaled(drm_rect_width(src), drm_rect_width(dst), &mut diff);
        src.x1 = src.x2 - new_src_w as i32;
        dst.x1 += diff;
    }

    diff = clip.y1 - dst.y1;
    if diff > 0 {
        let new_src_h = clip_scaled(drm_rect_height(src), drm_rect_height(dst), &mut diff);
        src.y1 = src.y2 - new_src_h as i32;
        dst.y1 += diff;
    }

    diff = dst.x2 - clip.x2;
    if diff > 0 {
        let new_src_w = clip_scaled(drm_rect_width(src), drm_rect_width(dst), &mut diff);
        src.x2 = src.x1 + new_src_w as i32;
        dst.x2 -= diff;
    }

    diff = dst.y2 - clip.y2;
    if diff > 0 {
        let new_src_h = clip_scaled(drm_rect_height(src), drm_rect_height(dst), &mut diff);
        src.y2 = src.y1 + new_src_h as i32;
        dst.y2 -= diff;
    }

    drm_rect_visible(dst)
}

// upstream: drm_rect.c drm_calc_scale()
fn drm_calc_scale(src: i32, dst: i32) -> i32 {
    let scale: i32;

    if warn_on(src < 0 || dst < 0) {
        return -EINVAL;
    }
    if dst == 0 {
        return 0;
    }
    if src > dst.wrapping_shl(16) {
        // DIV_ROUND_UP(src, dst)
        return (src + dst - 1) / dst;
    } else {
        scale = src / dst;
    }
    scale
}

// upstream: drm_rect.c drm_rect_calc_hscale()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_calc_hscale(
    src: *const DrmRect,
    dst: *const DrmRect,
    min_hscale: i32,
    max_hscale: i32,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let (src, dst) = unsafe { (&*src, &*dst) };
    let src_w = drm_rect_width(src);
    let dst_w = drm_rect_width(dst);
    let hscale = drm_calc_scale(src_w, dst_w);

    if hscale < 0 || dst_w == 0 {
        return hscale;
    }
    if hscale < min_hscale || hscale > max_hscale {
        return -ERANGE;
    }
    hscale
}

// upstream: drm_rect.c drm_rect_calc_vscale()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_calc_vscale(
    src: *const DrmRect,
    dst: *const DrmRect,
    min_vscale: i32,
    max_vscale: i32,
) -> i32 {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let (src, dst) = unsafe { (&*src, &*dst) };
    let src_h = drm_rect_height(src);
    let dst_h = drm_rect_height(dst);
    let vscale = drm_calc_scale(src_h, dst_h);

    if vscale < 0 || dst_h == 0 {
        return vscale;
    }
    if vscale < min_vscale || vscale > max_vscale {
        return -ERANGE;
    }
    vscale
}

// upstream: drm_rect.c drm_rect_debug_print()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_debug_print(prefix: *const c_char, r: *const DrmRect, fixed_point: bool) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let r = unsafe { &*r };
    let prefix = if prefix.is_null() {
        ""
    } else {
        // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
        unsafe { core::ffi::CStr::from_ptr(prefix) }.to_str().unwrap_or("")
    };
    let w = drm_rect_width(r);
    let h = drm_rect_height(r);
    let body = if fixed_point {
        // DRM_RECT_FP_FMT "%d.%06ux%d.%06u%+d.%06u%+d.%06u" with DRM_RECT_FP_ARG(r)
        format!(
            "{}.{:06}x{}.{:06}{:+}.{:06}{:+}.{:06}",
            w >> 16,
            ((w & 0xffff) * 15625) >> 10,
            h >> 16,
            ((h & 0xffff) * 15625) >> 10,
            r.x1 >> 16,
            ((r.x1 & 0xffff) * 15625) >> 10,
            r.y1 >> 16,
            ((r.y1 & 0xffff) * 15625) >> 10,
        )
    } else {
        // DRM_RECT_FMT "%dx%d%+d%+d"
        format!("{}x{}{:+}{:+}", w, h, r.x1, r.y1)
    };
    drm_log_at(DrmLogLevel::Debug, "drm_rect", file!(), line!(), &format!("{prefix}{body}"));
}

// upstream: drm_rect.c drm_rect_rotate()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_rotate(r: *mut DrmRect, width: i32, height: i32, rotation: u32) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let r = unsafe { &mut *r };
    let mut tmp: DrmRect;

    if rotation & (DRM_MODE_REFLECT_X | DRM_MODE_REFLECT_Y) != 0 {
        tmp = copy_rect(r);
        if rotation & DRM_MODE_REFLECT_X != 0 {
            r.x1 = width - tmp.x2;
            r.x2 = width - tmp.x1;
        }
        if rotation & DRM_MODE_REFLECT_Y != 0 {
            r.y1 = height - tmp.y2;
            r.y2 = height - tmp.y1;
        }
    }

    match rotation & DRM_MODE_ROTATE_MASK {
        DRM_MODE_ROTATE_0 => {}
        DRM_MODE_ROTATE_90 => {
            tmp = copy_rect(r);
            r.x1 = tmp.y1;
            r.x2 = tmp.y2;
            r.y1 = width - tmp.x2;
            r.y2 = width - tmp.x1;
        }
        DRM_MODE_ROTATE_180 => {
            tmp = copy_rect(r);
            r.x1 = width - tmp.x2;
            r.x2 = width - tmp.x1;
            r.y1 = height - tmp.y2;
            r.y2 = height - tmp.y1;
        }
        DRM_MODE_ROTATE_270 => {
            tmp = copy_rect(r);
            r.x1 = height - tmp.y2;
            r.x2 = height - tmp.y1;
            r.y1 = tmp.x1;
            r.y2 = tmp.x2;
        }
        _ => {}
    }
}

// upstream: drm_rect.c drm_rect_rotate_inv()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_rect_rotate_inv(r: *mut DrmRect, width: i32, height: i32, rotation: u32) {
    // SAFETY: the enclosing `unsafe` fn contract (live, correctly aligned Linux objects and caller-owned buffers) covers this access.
    let r = unsafe { &mut *r };
    let mut tmp: DrmRect;

    match rotation & DRM_MODE_ROTATE_MASK {
        DRM_MODE_ROTATE_0 => {}
        DRM_MODE_ROTATE_90 => {
            tmp = copy_rect(r);
            r.x1 = width - tmp.y2;
            r.x2 = width - tmp.y1;
            r.y1 = tmp.x1;
            r.y2 = tmp.x2;
        }
        DRM_MODE_ROTATE_180 => {
            tmp = copy_rect(r);
            r.x1 = width - tmp.x2;
            r.x2 = width - tmp.x1;
            r.y1 = height - tmp.y2;
            r.y2 = height - tmp.y1;
        }
        DRM_MODE_ROTATE_270 => {
            tmp = copy_rect(r);
            r.x1 = tmp.y1;
            r.x2 = tmp.y2;
            r.y1 = height - tmp.x2;
            r.y2 = height - tmp.x1;
        }
        _ => {}
    }

    if rotation & (DRM_MODE_REFLECT_X | DRM_MODE_REFLECT_Y) != 0 {
        tmp = copy_rect(r);
        if rotation & DRM_MODE_REFLECT_X != 0 {
            r.x1 = width - tmp.x2;
            r.x2 = width - tmp.x1;
        }
        if rotation & DRM_MODE_REFLECT_Y != 0 {
            r.y1 = height - tmp.y2;
            r.y2 = height - tmp.y1;
        }
    }
}
