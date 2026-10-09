// SPDX-License-Identifier: MIT
// Copyright © 2008 Intel Corporation.
//
// Source-faithful Rust transcription of Linux v7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_tiling.c. Keep the upstream function and
// branch order, including legacy fence sizing and swizzling behavior. The
// GEM/GGTT records and kernel helpers used here are integration bindings; they
// must resolve to the real i915 and Linux implementations, not compatibility
// shims.

use core::ffi::c_void;

use crate::{
    for_each_ggtt_vma,
    i915_gem_object_api_upstream::{i915_gem_object_has_pages, i915_gem_object_put},
    i915_gem_object_header_upstream::{
        i915_gem_object_clear_tiling_quirk, i915_gem_object_has_tiling_quirk,
        i915_gem_object_is_proxy, i915_gem_object_lookup, i915_gem_object_lookup_rcu,
        i915_gem_object_set_tiling_quirk, i915_gem_object_lock, i915_gem_object_unlock,
    },
    i915_gem_shrinker_upstream::{i915_gem_object_make_shrinkable, i915_gem_object_make_unshrinkable},
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_vma_api_upstream::*,
    intel_context_upstream::I915Vma,
    intel_engine_cs_upstream::ListHead,
    linux::{bits::IS_ALIGNED, i915::GRAPHICS_VER},
    linux_config::*,
    linux_i915_private::DrmI915Private,
    linux_macros::*,
    linux_list::*,
};

// UAPI and GEM records referenced by this translation. The complete records
// are owned by the DRM/i915 integration layer.
#[repr(C)]
pub struct DrmDevice {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmFile {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmI915GemSetTiling {
    pub handle: u32,
    pub tiling_mode: u32,
    pub stride: u32,
    pub swizzle_mode: u32,
}

#[repr(C)]
pub struct DrmI915GemGetTiling {
    pub handle: u32,
    pub tiling_mode: u32,
    pub swizzle_mode: u32,
    pub phys_swizzle_mode: u32,
}

// Values from include/uapi/drm/i915_drm.h.
pub const I915_TILING_NONE: u32 = 0;
pub const I915_TILING_X: u32 = 1;
pub const I915_TILING_Y: u32 = 2;
pub const I915_TILING_LAST: u32 = I915_TILING_Y;
pub const I915_BIT_6_SWIZZLE_NONE: u32 = 0;
pub const I915_BIT_6_SWIZZLE_9: u32 = 1;
pub const I915_BIT_6_SWIZZLE_9_10: u32 = 2;
pub const I915_BIT_6_SWIZZLE_UNKNOWN: u32 = 5;
pub const I915_BIT_6_SWIZZLE_9_17: u32 = 6;
pub const I915_BIT_6_SWIZZLE_9_10_17: u32 = 7;

// Values from i915_gem_object_types.h, intel_ggtt_fencing.h, i915_reg.h,
// intel_gtt.h, and i915_gem.h.
const FENCE_MINIMUM_STRIDE: u32 = 128;
const TILING_MASK: u32 = FENCE_MINIMUM_STRIDE - 1;
const STRIDE_MASK: u32 = !TILING_MASK;
const I965_FENCE_PAGE: u32 = 4096;
const I915_GTT_MIN_ALIGNMENT: u32 = 4096;
const I965_FENCE_MAX_PITCH_VAL: u32 = 0x0400;
const GEN7_FENCE_MAX_PITCH_VAL: u32 = 0x0800;
const GEM_QUIRK_PIN_SWIZZLED_PAGES: u32 = 1 << 0;
const PAGE_SHIFT: u32 = 12;
const EOPNOTSUPP: i32 = 95;

#[inline]
fn is_power_of_2(value: u32) -> bool {
    value.is_power_of_two()
}

// Linux's roundup(x, y) macro uses unsigned arithmetic in the source type.
#[inline]
fn roundup(value: u32, divisor: u32) -> u32 {
    (value.wrapping_add(divisor.wrapping_sub(1)) / divisor).wrapping_mul(divisor)
}

// Binding boundary supplied by the integration layer: i915/GEM/VM records
// (including the source fields accessed below), the fence/GTT helpers, RCU,
// object lookups and reference operations, Linux locks/lists/bitmap routines,
// and the usual i915 BUG/BUILD_BUG macros. The names below intentionally map
// one-for-one to the upstream helpers and are not fallback implementations.

/// DOC: buffer object tiling
///
/// i915_gem_set_tiling_ioctl() and i915_gem_get_tiling_ioctl() are the userspace
/// interface to declare fence register requirements.
///
/// In principle GEM does not care at all about an object's internal data
/// layout, and hence it also does not care about tiling or swizzling. There are
/// two exceptions:
///
/// - For X and Y tiling the hardware provides detilers for CPU access, so-called
///   fences. Since there are only a limited number of them the kernel must
///   manage these, and therefore userspace must tell the kernel the object
///   tiling if it wants to use fences for detiling.
/// - On gen3 and gen4 platforms there is a swizzling pattern for tiled objects
///   which depends on the physical page frame number. When swapping such
///   objects the page frame number might change and the kernel must be able to
///   fix this up and hence know the tiling. On a subset of platforms with
///   asymmetric memory-channel population the swizzling pattern changes in an
///   unknown way, and for those the kernel simply forbids swapping completely.
///
/// Since neither of these applies for new tiling layouts on modern platforms
/// like W, Ys and Yf tiling, GEM only allows object tiling to be set to X or Y
/// tiled. Anything else can be handled entirely in userspace without the
/// kernel's involvement.

/// i915_gem_fence_size - required global GTT size for a fence
/// @i915: i915 device
/// @size: object size
/// @tiling: tiling mode
/// @stride: tiling stride
///
/// Return the required global GTT size for a fence (view of a tiled object),
/// taking into account potential fence register mapping.
// upstream: i915_gem_tiling.c i915_gem_fence_size()
pub unsafe fn i915_gem_fence_size(
    i915: *mut DrmI915Private,
    size: u32,
    tiling: u32,
    mut stride: u32,
) -> u32 {
    let mut ggtt_size: u32;

    GEM_BUG_ON!(size == 0);

    if tiling == I915_TILING_NONE {
        return size;
    }

    GEM_BUG_ON!(stride == 0);

    if GRAPHICS_VER(i915) >= 4 {
        stride *= i915_gem_tile_height(tiling);
        GEM_BUG_ON!(!IS_ALIGNED(stride, I965_FENCE_PAGE));
        return roundup(size, stride);
    }

    // Previous chips need a power-of-two fence region when tiling.
    if GRAPHICS_VER(i915) == 3 {
        ggtt_size = 1024 * 1024;
    } else {
        ggtt_size = 512 * 1024;
    }

    while ggtt_size < size {
        ggtt_size <<= 1;
    }

    ggtt_size
}

/// i915_gem_fence_alignment - required global GTT alignment for a fence
/// @i915: i915 device
/// @size: object size
/// @tiling: tiling mode
/// @stride: tiling stride
///
/// Return the required global GTT alignment for a fence (a view of a tiled
/// object), taking into account potential fence register mapping.
// upstream: i915_gem_tiling.c i915_gem_fence_alignment()
pub unsafe fn i915_gem_fence_alignment(
    i915: *mut DrmI915Private,
    size: u32,
    tiling: u32,
    stride: u32,
) -> u32 {
    GEM_BUG_ON!(size == 0);

    // Minimum alignment is 4k (GTT page size), but might be greater if a
    // fence register is needed for the object.
    if tiling == I915_TILING_NONE {
        return I915_GTT_MIN_ALIGNMENT;
    }

    if GRAPHICS_VER(i915) >= 4 {
        return I965_FENCE_PAGE;
    }

    // Previous chips need to be aligned to the size of the smallest fence
    // register that can contain the object.
    i915_gem_fence_size(i915, size, tiling, stride)
}

// Check pitch constraints for all chips & tiling formats.
// upstream: i915_gem_tiling.c i915_tiling_ok()
unsafe fn i915_tiling_ok(obj: *mut DrmI915GemObject, tiling: u32, stride: u32) -> bool {
    let i915 = to_i915((*obj).base.base.dev);
    let tile_width: u32;

    // Linear is always fine.
    if tiling == I915_TILING_NONE {
        return true;
    }

    if tiling > I915_TILING_LAST {
        return false;
    }

    // Check maximum stride & object size. i965+ stores the end address of
    // the GTT mapping in the fence register, so does not need the size check.
    if GRAPHICS_VER(i915) >= 7 {
        if stride / 128 > GEN7_FENCE_MAX_PITCH_VAL {
            return false;
        }
    } else if GRAPHICS_VER(i915) >= 4 {
        if stride / 128 > I965_FENCE_MAX_PITCH_VAL {
            return false;
        }
    } else {
        if stride > 8192 {
            return false;
        }

        if !is_power_of_2(stride) {
            return false;
        }
    }

    if tiling == I915_TILING_Y && HAS_128_BYTE_Y_TILING(i915) {
        tile_width = 128;
    } else if GRAPHICS_VER(i915) == 2 {
        tile_width = 128;
    } else {
        tile_width = 512;
    }

    if stride == 0 || !IS_ALIGNED(stride, tile_width) {
        return false;
    }

    true
}

// upstream: i915_gem_tiling.c i915_vma_fence_prepare()
unsafe fn i915_vma_fence_prepare(vma: *mut I915Vma, tiling_mode: i32, stride: u32) -> bool {
    let i915 = (*(*vma).vm).i915;
    let size: u32;
    let alignment: u32;

    if !i915_vma_is_map_and_fenceable(vma) {
        return true;
    }

    size = i915_gem_fence_size(i915, (*vma).size as u32, tiling_mode as u32, stride);
    if (*vma).size < size as u64 {
        return false;
    }

    alignment = i915_gem_fence_alignment(i915, (*vma).size as u32, tiling_mode as u32, stride);
    if !IS_ALIGNED(i915_ggtt_offset(vma), alignment) {
        return false;
    }

    true
}

// Make the current GTT allocation valid for the change in tiling.
// upstream: i915_gem_tiling.c i915_gem_object_fence_prepare()
unsafe fn i915_gem_object_fence_prepare(
    obj: *mut DrmI915GemObject,
    tiling_mode: i32,
    stride: u32,
) -> i32 {
    let i915 = to_i915((*obj).base.base.dev);
    let ggtt = (*to_gt(i915)).ggtt;
    let mut vma: *mut I915Vma = core::ptr::null_mut();
    let mut vn: *mut I915Vma = core::ptr::null_mut();
    let mut unbind = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    let mut ret = 0;

    INIT_LIST_HEAD(&mut unbind);

    if tiling_mode == I915_TILING_NONE as i32 {
        return 0;
    }

    mutex_lock(&mut (*ggtt).vm.mutex);

    spin_lock(&mut (*obj).vma.lock);
    for_each_ggtt_vma!(vma, obj, {
        GEM_BUG_ON!((*vma).vm != core::ptr::addr_of_mut!((*ggtt).vm));

        if i915_vma_fence_prepare(vma, tiling_mode, stride) {
            continue;
        }

        list_move(&mut (*vma).vm_link, &mut unbind);
    });
    spin_unlock(&mut (*obj).vma.lock);

    list_for_each_entry_safe!(vma, vn, &mut unbind, vm_link, {
        ret = __i915_vma_unbind(vma);
        if ret != 0 {
            // Restore the remaining VMA on an error.
            list_splice(&mut unbind, &mut (*ggtt).vm.bound_list);
            break;
        }
    });

    mutex_unlock(&mut (*ggtt).vm.mutex);

    ret
}

// upstream: i915_gem_tiling.c i915_gem_object_needs_bit17_swizzle()
pub unsafe fn i915_gem_object_needs_bit17_swizzle(obj: *mut DrmI915GemObject) -> bool {
    let i915 = to_i915((*obj).base.base.dev);

    (*(*to_gt(i915)).ggtt).bit_6_swizzle_x == I915_BIT_6_SWIZZLE_9_10_17
        && i915_gem_object_is_tiled(obj)
}

// upstream: i915_gem_tiling.c i915_gem_object_set_tiling()
pub unsafe fn i915_gem_object_set_tiling(
    obj: *mut DrmI915GemObject,
    tiling: u32,
    stride: u32,
) -> i32 {
    let i915 = to_i915((*obj).base.base.dev);
    let mut vma: *mut I915Vma = core::ptr::null_mut();
    let mut err: i32;

    // Make sure we do not cross-contaminate obj->tiling_and_stride.
    BUILD_BUG_ON!((I915_TILING_LAST & STRIDE_MASK) != 0);

    GEM_BUG_ON!(!i915_tiling_ok(obj, tiling, stride));
    GEM_BUG_ON!((stride == 0) ^ (tiling == I915_TILING_NONE));

    if (tiling | stride) == (*obj).tiling_and_stride {
        return 0;
    }

    if i915_gem_object_is_framebuffer(obj) {
        return -EBUSY;
    }

    // Rebind if the current allocation no longer meets the new tiling's
    // alignment restrictions. Otherwise leave it allocated and update any
    // fence before the next fenced access, including an unfenced register
    // used by the GPU for fenced commands on an untiled object.
    i915_gem_object_lock(obj, core::ptr::null_mut());
    if i915_gem_object_is_framebuffer(obj) {
        i915_gem_object_unlock(obj);
        return -EBUSY;
    }

    err = i915_gem_object_fence_prepare(obj, tiling as i32, stride);
    if err != 0 {
        i915_gem_object_unlock(obj);
        return err;
    }

    // If memory has unknown (i.e. varying) swizzling, pin the pages to
    // prevent them from being swapped out and corrupting their contents.
    if i915_gem_object_has_pages(obj)
        && (*obj).mm.madv() == I915_MADV_WILLNEED
        && (*i915).gem_quirks & GEM_QUIRK_PIN_SWIZZLED_PAGES as u64 != 0
    {
        if tiling == I915_TILING_NONE {
            GEM_BUG_ON!(!i915_gem_object_has_tiling_quirk(obj));
            i915_gem_object_clear_tiling_quirk(obj);
            i915_gem_object_make_shrinkable(obj);
        }
        if !i915_gem_object_is_tiled(obj) {
            GEM_BUG_ON!(i915_gem_object_has_tiling_quirk(obj));
            i915_gem_object_make_unshrinkable(obj);
            i915_gem_object_set_tiling_quirk(obj);
        }
    }

    spin_lock(&mut (*obj).vma.lock);
    for_each_ggtt_vma!(vma, obj, {
        (*vma).fence_size = i915_gem_fence_size(i915, (*vma).size as u32, tiling, stride);
        (*vma).fence_alignment = i915_gem_fence_alignment(i915, (*vma).size as u32, tiling, stride);

        if !(*vma).fence.is_null() {
            (*(*vma).fence).dirty = true;
        }
    });
    spin_unlock(&mut (*obj).vma.lock);

    (*obj).tiling_and_stride = tiling | stride;

    // Try to preallocate memory required to save swizzling on put-pages.
    if i915_gem_object_needs_bit17_swizzle(obj) {
        if (*obj).bit_17.is_null() {
            (*obj).bit_17 = bitmap_zalloc(((*obj).base.base.size >> PAGE_SHIFT) as usize, GFP_KERNEL);
        }
    } else {
        bitmap_free((*obj).bit_17);
        (*obj).bit_17 = core::ptr::null_mut();
    }

    i915_gem_object_unlock(obj);

    // Force the fence to be reacquired for GTT access.
    i915_gem_object_release_mmap_gtt(obj);

    0
}

/// i915_gem_set_tiling_ioctl - IOCTL handler to set tiling mode
/// @dev: DRM device
/// @data: data pointer for the ioctl
/// @file: DRM file for the ioctl call
///
/// Sets the tiling mode of an object, returning the required swizzling of bit
/// 6 of addresses in the object. Called by the user via ioctl.
///
/// Returns zero on success, negative errno on failure.
// upstream: i915_gem_tiling.c i915_gem_set_tiling_ioctl()
pub unsafe fn i915_gem_set_tiling_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let i915 = to_i915(dev);
    let args = data.cast::<DrmI915GemSetTiling>();
    let obj: *mut DrmI915GemObject;

    if (*(*to_gt(i915)).ggtt).num_fences == 0 {
        return -EOPNOTSUPP;
    }

    obj = i915_gem_object_lookup(file, (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }

    // The tiling mode of proxy objects is handled by their generator and is
    // not allowed to be changed by userspace.
    let err = 'set_tiling: {
        if i915_gem_object_is_proxy(obj) {
            break 'set_tiling -ENXIO;
        }

        if !i915_tiling_ok(obj, (*args).tiling_mode, (*args).stride) {
            break 'set_tiling -EINVAL;
        }

        if (*args).tiling_mode == I915_TILING_NONE {
            (*args).swizzle_mode = I915_BIT_6_SWIZZLE_NONE;
            (*args).stride = 0;
        } else {
            if (*args).tiling_mode == I915_TILING_X {
                (*args).swizzle_mode = (*(*to_gt(i915)).ggtt).bit_6_swizzle_x;
            } else {
                (*args).swizzle_mode = (*(*to_gt(i915)).ggtt).bit_6_swizzle_y;
            }

            // Hide bit 17 swizzling from userspace. This prevents old Mesa from
            // aborting on software fallbacks to bit 17; pread/pwrite handle the
            // swizzling for it.
            if (*args).swizzle_mode == I915_BIT_6_SWIZZLE_9_17 {
                (*args).swizzle_mode = I915_BIT_6_SWIZZLE_9;
            }
            if (*args).swizzle_mode == I915_BIT_6_SWIZZLE_9_10_17 {
                (*args).swizzle_mode = I915_BIT_6_SWIZZLE_9_10;
            }

            // If the swizzling cannot be handled, make the object untiled.
            if (*args).swizzle_mode == I915_BIT_6_SWIZZLE_UNKNOWN {
                (*args).tiling_mode = I915_TILING_NONE;
                (*args).swizzle_mode = I915_BIT_6_SWIZZLE_NONE;
                (*args).stride = 0;
            }
        }

        let err = i915_gem_object_set_tiling(obj, (*args).tiling_mode, (*args).stride);

        // Maintain this existing ABI.
        (*args).stride = i915_gem_object_get_stride(obj);
        (*args).tiling_mode = i915_gem_object_get_tiling(obj);

        err
    };

    i915_gem_object_put(obj);
    err
}

/// i915_gem_get_tiling_ioctl - IOCTL handler to get tiling mode
/// @dev: DRM device
/// @data: data pointer for the ioctl
/// @file: DRM file for the ioctl call
///
/// Returns the current tiling mode and required bit 6 swizzling for the object.
/// Called by the user via ioctl. Returns zero on success, negative errno on
/// failure.
// upstream: i915_gem_tiling.c i915_gem_get_tiling_ioctl()
pub unsafe fn i915_gem_get_tiling_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmI915GemGetTiling>();
    let i915 = to_i915(dev);
    let mut obj: *mut DrmI915GemObject;
    let mut err = -ENOENT;

    if (*(*to_gt(i915)).ggtt).num_fences == 0 {
        return -EOPNOTSUPP;
    }

    rcu_read_lock();
    obj = i915_gem_object_lookup_rcu(file, (*args).handle);
    if !obj.is_null() {
        (*args).tiling_mode = READ_ONCE!((*obj).tiling_and_stride) & TILING_MASK;
        err = 0;
    }
    rcu_read_unlock();
    if unlikely(err != 0) {
        return err;
    }

    match (*args).tiling_mode {
        I915_TILING_X => {
            (*args).swizzle_mode = (*(*to_gt(i915)).ggtt).bit_6_swizzle_x;
        }
        I915_TILING_Y => {
            (*args).swizzle_mode = (*(*to_gt(i915)).ggtt).bit_6_swizzle_y;
        }
        _ => {
            // This is the source switch's default case and the explicit
            // I915_TILING_NONE case.
            (*args).swizzle_mode = I915_BIT_6_SWIZZLE_NONE;
        }
    }

    // Hide bit 17 from userspace -- see the set-tiling comment.
    if (*i915).gem_quirks & GEM_QUIRK_PIN_SWIZZLED_PAGES as u64 != 0 {
        (*args).phys_swizzle_mode = I915_BIT_6_SWIZZLE_UNKNOWN;
    } else {
        (*args).phys_swizzle_mode = (*args).swizzle_mode;
    }
    if (*args).swizzle_mode == I915_BIT_6_SWIZZLE_9_17 {
        (*args).swizzle_mode = I915_BIT_6_SWIZZLE_9;
    }
    if (*args).swizzle_mode == I915_BIT_6_SWIZZLE_9_10_17 {
        (*args).swizzle_mode = I915_BIT_6_SWIZZLE_9_10;
    }

    0
}
