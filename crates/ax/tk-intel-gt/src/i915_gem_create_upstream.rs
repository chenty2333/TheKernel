// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
//
//! Linux v7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_create.c` translation.
//! The source is MIT-licensed; this file keeps its source order, validation,
//! placement selection, and error/ownership paths. DRM, display, PXP, memory
//! region lookup, tracepoint, and user-extension services remain explicit
//! kernel integration bindings rather than local stubs.

#![allow(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    non_snake_case,
    non_camel_case_types
)]

use core::ffi::{c_char, c_int, c_ulong, c_void};

use crate::{
    i915_gem_context_upstream::I915UserExtension,
    i915_gem_core_upstream::u64_to_user_ptr,
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_header_upstream::i915_gem_object_size_2big,
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_BO_ALLOC_GPU_ONLY, I915_BO_ALLOC_USER, I915_BO_PROTECTED,
        intel_bo_to_drm_bo,
    },
    i915_gem_object_upstream::{
        i915_gem_flush_free_objects, i915_gem_object_alloc, i915_gem_object_free,
        i915_gem_object_set_pat_index,
    },
    i915_gem_region_upstream::IntelMemoryRegionOps,
    i915_request_types_upstream::DrmFile,
    linux::{
        gem::{DrmDevice, DrmGemObject},
        gem_memory::{
            INTEL_MEMORY_LOCAL, INTEL_MEMORY_SYSTEM, INTEL_REGION_SMEM, INTEL_REGION_UNKNOWN,
            IntelMemoryRegion,
        },
        i915::{GRAPHICS_VER_FULL, HAS_LMEM, INTEL_INFO, IP_VER, to_i915},
        memory::{kfree, kmalloc_objs},
        mm_native::copy_from_user,
        primitives::{is_power_of_2_u64, round_up},
    },
    linux_config::{E2BIG, EFAULT, EINVAL, ENODEV, ENOMEM, ERR_PTR, IS_ERR, PAGE_SIZE, PTR_ERR},
    linux_i915_private::DrmI915Private,
};

const I915_BO_INVALID_OFFSET: u64 = u64::MAX;
const DRM_FORMAT_C8: u32 = 0x2020_3843;
const DRM_FORMAT_RGB565: u32 = 0x3631_4752;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const I915_GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS: u32 = 1 << 0;
const I915_GEM_CREATE_EXT_MEMORY_REGIONS: usize = 0;
const I915_GEM_CREATE_EXT_PROTECTED_CONTENT: usize = 1;
const I915_GEM_CREATE_EXT_SET_PAT: usize = 2;
const _: [(); 0] = [(); I915_GEM_CREATE_EXT_MEMORY_REGIONS];
const _: [(); 1] = [(); I915_GEM_CREATE_EXT_PROTECTED_CONTENT];
const _: [(); 3] = [(); I915_GEM_CREATE_EXT_SET_PAT + 1];
const PAT_INDEX_NOT_SET: u32 = 0xffff;

/// UAPI `struct drm_mode_create_dumb` (`include/uapi/drm/drm_mode.h`).
#[repr(C)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}
const _: [(); 32] = [(); core::mem::size_of::<DrmModeCreateDumb>()];

/// UAPI `struct drm_i915_gem_create` (`include/uapi/drm/i915_drm.h`).
#[repr(C)]
pub struct DrmI915GemCreate {
    pub size: u64,
    pub handle: u32,
    pub pad: u32,
}
const _: [(); 16] = [(); core::mem::size_of::<DrmI915GemCreate>()];

/// UAPI `struct drm_i915_gem_memory_class_instance`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmI915GemMemoryClassInstance {
    pub memory_class: u16,
    pub memory_instance: u16,
}
const _: [(); 4] = [(); core::mem::size_of::<DrmI915GemMemoryClassInstance>()];

/// UAPI `struct drm_i915_gem_create_ext`.
#[repr(C)]
pub struct DrmI915GemCreateExt {
    pub size: u64,
    pub handle: u32,
    pub flags: u32,
    pub extensions: u64,
}
const _: [(); 24] = [(); core::mem::size_of::<DrmI915GemCreateExt>()];

/// UAPI `struct drm_i915_gem_create_ext_memory_regions`.
#[repr(C)]
pub struct DrmI915GemCreateExtMemoryRegions {
    pub base: I915UserExtension,
    pub pad: u32,
    pub num_regions: u32,
    pub regions: u64,
}
const _: [(); 48] = [(); core::mem::size_of::<DrmI915GemCreateExtMemoryRegions>()];

/// UAPI `struct drm_i915_gem_create_ext_protected_content`.
#[repr(C)]
pub struct DrmI915GemCreateExtProtectedContent {
    pub base: I915UserExtension,
    pub flags: u32,
}
const _: [(); 40] = [(); core::mem::size_of::<DrmI915GemCreateExtProtectedContent>()];

/// UAPI `struct drm_i915_gem_create_ext_set_pat`.
#[repr(C)]
pub struct DrmI915GemCreateExtSetPat {
    pub base: I915UserExtension,
    pub pat_index: u32,
    pub rsvd: u32,
}
const _: [(); 40] = [(); core::mem::size_of::<DrmI915GemCreateExtSetPat>()];
const _: [(); core::mem::size_of::<DrmI915GemCreateExtSetPat>()] =
    [(); core::mem::offset_of!(DrmI915GemCreateExtSetPat, rsvd) + core::mem::size_of::<u32>()];

#[repr(C)]
struct CreateExt {
    i915: *mut DrmI915Private,
    placements: [*mut IntelMemoryRegion; INTEL_REGION_UNKNOWN as usize],
    n_placements: u32,
    placement_mask: u32,
    flags: c_ulong,
    pat_index: u32,
}
const _: [(); 88] = [(); core::mem::size_of::<CreateExt>()];

type I915UserExtensionFn =
    Option<unsafe extern "C" fn(*mut I915UserExtension, *mut c_void) -> c_int>;

unsafe extern "C" {
    fn drm_gem_handle_create(file: *mut DrmFile, obj: *mut DrmGemObject, handle: *mut u32)
    -> c_int;
    fn trace_i915_gem_object_create(obj: *mut DrmI915GemObject);
    fn intel_memory_region_by_type(
        i915: *mut DrmI915Private,
        memory_type: u16,
    ) -> *mut IntelMemoryRegion;
    fn intel_memory_region_lookup(
        i915: *mut DrmI915Private,
        memory_class: u16,
        memory_instance: u16,
    ) -> *mut IntelMemoryRegion;
    fn intel_pxp_is_enabled(pxp: *const c_void) -> bool;
    fn intel_dumb_fb_max_stride(dev: *mut DrmDevice, pixel_format: u32, modifier: u64) -> u32;
    fn i915_user_extensions(
        extensions: *mut I915UserExtension,
        funcs: *const I915UserExtensionFn,
        count: u32,
        data: *mut c_void,
    ) -> c_int;
}

// upstream: i915_gem_create.c object_max_page_size()
unsafe fn object_max_page_size(placements: *mut *mut IntelMemoryRegion, n_placements: u32) -> u32 {
    let mut max_page_size = 0u32;
    for i in 0..n_placements as usize {
        let mr = unsafe { *placements.add(i) };
        let min_page_size = unsafe { (*mr).min_page_size };
        GEM_BUG_ON!(!is_power_of_2_u64(min_page_size));
        max_page_size = max_page_size.max(min_page_size as u32);
    }
    GEM_BUG_ON!(max_page_size == 0);
    max_page_size
}

// upstream: i915_gem_create.c object_set_placements()
unsafe fn object_set_placements(
    obj: *mut DrmI915GemObject,
    placements: *mut *mut IntelMemoryRegion,
    n_placements: u32,
) -> c_int {
    GEM_BUG_ON!(n_placements == 0);

    // Avoid allocating for the common one-region placement.
    if n_placements == 1 {
        let mr = unsafe { *placements };
        let i915 = unsafe { (*mr).i915 };
        let id = unsafe { (*mr).id as usize };
        unsafe {
            (*obj).mm.placements = core::ptr::addr_of_mut!((*i915).mm.regions[id]);
            (*obj).mm.n_placements = 1;
        }
    } else {
        let arr = kmalloc_objs::<*mut IntelMemoryRegion, usize>(n_placements as usize);
        if arr.is_null() {
            return -ENOMEM;
        }
        for i in 0..n_placements as usize {
            unsafe { arr.add(i).write(*placements.add(i)) };
        }
        unsafe {
            (*obj).mm.placements = arr;
            (*obj).mm.n_placements = n_placements as i32;
        }
    }

    0
}

// upstream: i915_gem_create.c i915_gem_publish()
unsafe fn i915_gem_publish(
    obj: *mut DrmI915GemObject,
    file: *mut DrmFile,
    size_p: *mut u64,
    handle_p: *mut u32,
) -> c_int {
    let size = unsafe { (&(*obj).base.base).size };
    let ret = unsafe { drm_gem_handle_create(file, intel_bo_to_drm_bo(obj), handle_p) };
    // The handle now owns the reference from allocation, even on failure.
    unsafe { i915_gem_object_put(obj) };
    if ret != 0 {
        return ret;
    }
    unsafe { *size_p = size };
    0
}

// upstream: i915_gem_create.c __i915_gem_object_create_user_ext()
unsafe fn __i915_gem_object_create_user_ext(
    i915: *mut DrmI915Private,
    mut size: u64,
    placements: *mut *mut IntelMemoryRegion,
    n_placements: u32,
    ext_flags: u32,
) -> *mut DrmI915GemObject {
    let mr = unsafe { *placements };
    unsafe { i915_gem_flush_free_objects(i915) };

    size = round_up(size, object_max_page_size(placements, n_placements) as u64);
    if size == 0 {
        return ERR_PTR(-EINVAL);
    }

    GEM_BUG_ON!(size & (PAGE_SIZE as u64 - 1) != 0);
    if i915_gem_object_size_2big(size) {
        return ERR_PTR(-E2BIG);
    }

    let obj = unsafe { i915_gem_object_alloc() };
    if obj.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    let ret = unsafe { object_set_placements(obj, placements, n_placements) };
    if ret != 0 {
        if unsafe { (*obj).mm.n_placements > 1 } {
            unsafe { kfree((*obj).mm.placements) };
        }
        unsafe { i915_gem_object_free(obj) };
        return ERR_PTR(ret);
    }

    // I915_BO_ALLOC_USER forces clearing before user access.
    let ops = unsafe { (*mr).ops.cast::<IntelMemoryRegionOps>() };
    let ret = unsafe {
        ((*ops).init_object.unwrap())(
            mr,
            obj,
            I915_BO_INVALID_OFFSET,
            size,
            0,
            I915_BO_ALLOC_USER as u32,
        )
    };
    if ret != 0 {
        if unsafe { (*obj).mm.n_placements > 1 } {
            unsafe { kfree((*obj).mm.placements) };
        }
        unsafe { i915_gem_object_free(obj) };
        return ERR_PTR(ret);
    }

    GEM_BUG_ON!(size != unsafe { (&(*obj).base.base).size });
    unsafe { (*obj).flags |= ext_flags as c_ulong };
    unsafe { trace_i915_gem_object_create(obj) };
    obj
}

// upstream: i915_gem_create.c __i915_gem_object_create_user()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_gem_object_create_user(
    i915: *mut DrmI915Private,
    size: u64,
    placements: *mut *mut IntelMemoryRegion,
    n_placements: u32,
) -> *mut DrmI915GemObject {
    unsafe { __i915_gem_object_create_user_ext(i915, size, placements, n_placements, 0) }
}

// upstream: i915_gem_create.c i915_gem_dumb_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_dumb_create(
    file: *mut DrmFile,
    dev: *mut DrmDevice,
    args: *mut DrmModeCreateDumb,
) -> c_int {
    let args = unsafe { &mut *args };
    let cpp = args.bpp.wrapping_add(7) / 8;
    let format = match cpp {
        1 => DRM_FORMAT_C8,
        2 => DRM_FORMAT_RGB565,
        4 => DRM_FORMAT_XRGB8888,
        _ => return -EINVAL,
    };

    args.pitch = round_up(args.width.wrapping_mul(cpp), 64);
    if args.pitch > unsafe { intel_dumb_fb_max_stride(dev, format, DRM_FORMAT_MOD_LINEAR) } {
        args.pitch = round_up(args.pitch, 4096);
    }
    if args.pitch < args.width {
        return -EINVAL;
    }
    args.size = (args.pitch as u64) * (args.height as u64);

    let i915 = unsafe { to_i915(dev.cast()) };
    let memory_type = if unsafe { HAS_LMEM(i915) } {
        INTEL_MEMORY_LOCAL
    } else {
        INTEL_MEMORY_SYSTEM
    };
    let mut mr = unsafe { intel_memory_region_by_type(i915, memory_type) };
    let obj =
        unsafe { __i915_gem_object_create_user(i915, args.size, core::ptr::addr_of_mut!(mr), 1) };
    if IS_ERR(obj) {
        return PTR_ERR(obj);
    }

    unsafe { i915_gem_publish(obj, file, &mut args.size, &mut args.handle) }
}

// upstream: i915_gem_create.c i915_gem_create_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_create_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let i915 = unsafe { to_i915(dev.cast()) };
    let args = unsafe { &mut *data.cast::<DrmI915GemCreate>() };
    let mut mr = unsafe { intel_memory_region_by_type(i915, INTEL_MEMORY_SYSTEM) };
    let obj =
        unsafe { __i915_gem_object_create_user(i915, args.size, core::ptr::addr_of_mut!(mr), 1) };
    if IS_ERR(obj) {
        return PTR_ERR(obj);
    }
    unsafe { i915_gem_publish(obj, file, &mut args.size, &mut args.handle) }
}

// Source `struct create_ext` in i915_gem_create.c; its exact x86_64 layout is
// asserted above. It accumulates all extensions before object allocation.

// upstream: i915_gem_create.c repr_placements()
unsafe fn repr_placements(
    mut buf: *mut c_char,
    mut size: usize,
    placements: *mut *mut IntelMemoryRegion,
    n_placements: i32,
) {
    unsafe { buf.write(0) };
    for i in 0..n_placements as usize {
        let mr = unsafe { *placements.add(i) };
        let r = snprintf!(
            buf,
            size,
            "\n  %s -> { class: %d, inst: %d }",
            unsafe { (*mr).name },
            unsafe { (*mr).r#type },
            unsafe { (*mr).instance },
        );
        // C compares signed `r` against unsigned `size`; a negative result is
        // consequently treated as a full/truncated buffer too.
        if r as usize >= size {
            return;
        }
        buf = unsafe { buf.add(r as usize) };
        size -= r as usize;
    }
}

// upstream: i915_gem_create.c set_placements()
unsafe fn set_placements(
    args: *const DrmI915GemCreateExtMemoryRegions,
    ext_data: *mut CreateExt,
) -> c_int {
    let i915 = unsafe { (*ext_data).i915 };
    let mut uregions =
        unsafe { u64_to_user_ptr((*args).regions) }.cast::<DrmI915GemMemoryClassInstance>();
    let mut placements = [core::ptr::null_mut(); INTEL_REGION_UNKNOWN as usize];
    let mut mask = 0u32;
    let mut ret = 0;

    if unsafe { (*args).pad } != 0 {
        drm_dbg!(&(*i915).drm, "pad should be zero\n");
        ret = -EINVAL;
    }
    if unsafe { (*args).num_regions } == 0 {
        drm_dbg!(&(*i915).drm, "num_regions is zero\n");
        ret = -EINVAL;
    }
    if unsafe { (*args).num_regions } > INTEL_REGION_UNKNOWN as u32 {
        drm_dbg!(&(*i915).drm, "num_regions is too large\n");
        ret = -EINVAL;
    }
    if ret != 0 {
        return ret;
    }

    let n_regions = unsafe { (*args).num_regions } as usize;
    let mut i = 0usize;
    while i < n_regions {
        let mut region = DrmI915GemMemoryClassInstance {
            memory_class: 0,
            memory_instance: 0,
        };
        if unsafe {
            copy_from_user(
                (&mut region as *mut DrmI915GemMemoryClassInstance).cast(),
                uregions.cast_const().cast(),
                core::mem::size_of::<DrmI915GemMemoryClassInstance>(),
            )
        } != 0
        {
            return -EFAULT;
        }

        let mr = unsafe {
            intel_memory_region_lookup(i915, region.memory_class, region.memory_instance)
        };
        if mr.is_null() || unsafe { (*mr).private } {
            drm_dbg!(
                &(*i915).drm,
                "Device is missing region { class: %d, inst: %d } at index = %d\n",
                region.memory_class,
                region.memory_instance,
                i as c_int,
            );
            ret = -EINVAL;
            break;
        }

        let id = unsafe { (*mr).id };
        if mask & (1u32 << id as u32) != 0 {
            drm_dbg!(
                &(*i915).drm,
                "Found duplicate placement %s -> { class: %d, inst: %d } at index = %d\n",
                unsafe { (*mr).name },
                region.memory_class,
                region.memory_instance,
                i as c_int,
            );
            ret = -EINVAL;
            break;
        }

        placements[i] = mr;
        mask |= 1u32 << id as u32;
        uregions = unsafe { uregions.add(1) };
        i += 1;
    }

    if ret == 0 && unsafe { (*ext_data).n_placements } != 0 {
        ret = -EINVAL;
    }

    if ret == 0 {
        let n_placements = unsafe { (*args).num_regions };
        unsafe {
            (*ext_data).n_placements = n_placements;
            for i in 0..n_placements as usize {
                (*ext_data).placements[i] = placements[i];
            }
            (*ext_data).placement_mask = mask;
        }
        return 0;
    }

    let mut buf = [0 as c_char; 256];
    if unsafe { (*ext_data).n_placements } != 0 {
        unsafe {
            repr_placements(
                buf.as_mut_ptr(),
                buf.len(),
                (*ext_data).placements.as_mut_ptr(),
                (*ext_data).n_placements as i32,
            )
        };
        drm_dbg!(
            &(*i915).drm,
            "Placements were already set in previous EXT. Existing placements: %s\n",
            buf,
        );
    }
    unsafe {
        repr_placements(
            buf.as_mut_ptr(),
            buf.len(),
            placements.as_mut_ptr(),
            i as i32,
        )
    };
    drm_dbg!(&(*i915).drm, "New placements(so far validated): %s\n", buf,);
    ret
}

// upstream: i915_gem_create.c ext_set_placements()
unsafe extern "C" fn ext_set_placements(base: *mut I915UserExtension, data: *mut c_void) -> c_int {
    let mut ext = core::mem::MaybeUninit::<DrmI915GemCreateExtMemoryRegions>::uninit();
    if unsafe {
        copy_from_user(
            ext.as_mut_ptr().cast(),
            base.cast_const().cast(),
            core::mem::size_of::<DrmI915GemCreateExtMemoryRegions>(),
        )
    } != 0
    {
        return -EFAULT;
    }
    unsafe { set_placements(ext.as_ptr(), data.cast()) }
}

// upstream: i915_gem_create.c ext_set_protected()
unsafe extern "C" fn ext_set_protected(base: *mut I915UserExtension, data: *mut c_void) -> c_int {
    let mut ext = core::mem::MaybeUninit::<DrmI915GemCreateExtProtectedContent>::uninit();
    let ext_data = unsafe { &mut *data.cast::<CreateExt>() };
    if unsafe {
        copy_from_user(
            ext.as_mut_ptr().cast(),
            base.cast_const().cast(),
            core::mem::size_of::<DrmI915GemCreateExtProtectedContent>(),
        )
    } != 0
    {
        return -EFAULT;
    }
    let ext = unsafe { ext.assume_init() };
    if ext.flags != 0 {
        return -EINVAL;
    }
    if !unsafe { intel_pxp_is_enabled((*ext_data.i915).pxp) } {
        return -ENODEV;
    }
    ext_data.flags |= I915_BO_PROTECTED;
    0
}

// upstream: i915_gem_create.c ext_set_pat()
unsafe extern "C" fn ext_set_pat(base: *mut I915UserExtension, data: *mut c_void) -> c_int {
    let ext_data = unsafe { &mut *data.cast::<CreateExt>() };
    let i915 = ext_data.i915;
    if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 70) {
        return -ENODEV;
    }

    let mut ext = core::mem::MaybeUninit::<DrmI915GemCreateExtSetPat>::uninit();
    if unsafe {
        copy_from_user(
            ext.as_mut_ptr().cast(),
            base.cast_const().cast(),
            core::mem::size_of::<DrmI915GemCreateExtSetPat>(),
        )
    } != 0
    {
        return -EFAULT;
    }
    let ext = unsafe { ext.assume_init() };
    let max_pat_index = unsafe { (*INTEL_INFO(i915)).max_pat_index };
    if ext.pat_index > max_pat_index {
        drm_dbg!(&(*i915).drm, "PAT index is invalid: %u\n", ext.pat_index,);
        return -EINVAL;
    }
    ext_data.pat_index = ext.pat_index;
    0
}

static CREATE_EXTENSIONS: [I915UserExtensionFn; 3] = [
    Some(ext_set_placements),
    Some(ext_set_protected),
    Some(ext_set_pat),
];

// upstream: i915_gem_create.c i915_gem_create_ext_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_create_ext_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let i915 = unsafe { to_i915(dev.cast()) };
    let args = unsafe { &mut *data.cast::<DrmI915GemCreateExt>() };
    let mut ext_data = CreateExt {
        i915,
        placements: [core::ptr::null_mut(); INTEL_REGION_UNKNOWN as usize],
        n_placements: 0,
        placement_mask: 0,
        flags: 0,
        pat_index: 0,
    };

    if args.flags & !I915_GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS != 0 {
        return -EINVAL;
    }

    ext_data.pat_index = PAT_INDEX_NOT_SET;
    let ret = unsafe {
        i915_user_extensions(
            u64_to_user_ptr(args.extensions).cast(),
            CREATE_EXTENSIONS.as_ptr(),
            CREATE_EXTENSIONS.len() as u32,
            (&mut ext_data as *mut CreateExt).cast(),
        )
    };
    if ret != 0 {
        return ret;
    }

    if ext_data.n_placements == 0 {
        ext_data.placements[0] = unsafe { intel_memory_region_by_type(i915, INTEL_MEMORY_SYSTEM) };
        ext_data.n_placements = 1;
    }

    if args.flags & I915_GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS != 0 {
        if ext_data.n_placements == 1 {
            return -EINVAL;
        }
        if ext_data.placement_mask & (1u32 << INTEL_REGION_SMEM as u32) == 0 {
            return -EINVAL;
        }
    } else if ext_data.n_placements > 1
        || unsafe { (*ext_data.placements[0]).r#type } != INTEL_MEMORY_SYSTEM
    {
        ext_data.flags |= I915_BO_ALLOC_GPU_ONLY;
    }

    let obj = unsafe {
        __i915_gem_object_create_user_ext(
            i915,
            args.size,
            ext_data.placements.as_mut_ptr(),
            ext_data.n_placements,
            ext_data.flags as u32,
        )
    };
    if IS_ERR(obj) {
        return PTR_ERR(obj);
    }

    if ext_data.pat_index != PAT_INDEX_NOT_SET {
        unsafe {
            i915_gem_object_set_pat_index(obj, ext_data.pat_index);
            // Set the source `pat_set_by_user` bit without disturbing its
            // adjacent pat index, cache coherency, dirty, or DPT bitfields.
            (*obj).cache_state_bits |= 1 << 6;
        }
    }

    unsafe { i915_gem_publish(obj, file, &mut args.size, &mut args.handle) }
}
