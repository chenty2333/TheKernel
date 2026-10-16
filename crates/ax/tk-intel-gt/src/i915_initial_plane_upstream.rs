// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_initial_plane.c.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::{offset_of, zeroed},
    ptr,
};

use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_framebuffer_types::DrmFramebuffer,
    drm_gem_types::DrmGemObject,
    drm_plane_types::DrmPlaneState,
    uapi_drm_consts::{DRM_FORMAT_MOD_LINEAR, DRM_MODE_FB_MODIFIERS},
    uapi_drm_mode_types::DrmModeFbCmd2,
};
use tk_linux_kapi::types::DrmMmNode;

use crate::{
    i915_gem_gtt_upstream::i915_gem_gtt_reserve,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_api_upstream::i915_gem_object_put,
    i915_gem_object_header_upstream::i915_gem_object_is_tiled,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915CacheLevel, intel_bo_to_drm_bo},
    i915_gem_object_upstream::i915_gem_object_set_cache_coherency,
    i915_gem_region_upstream::i915_gem_object_create_region_at,
    i915_gem_tiling_upstream::{I915_TILING_NONE, I915_TILING_X, I915_TILING_Y},
    i915_vma_api_upstream::{__i915_vma_pin, i915_vma_get, i915_vma_is_map_and_fenceable, i915_vma_put},
    i915_vma_types_upstream::I915Vma,
    i915_vma_upstream::{PIN_MAPPABLE, PIN_OFFSET_FIXED, i915_vma_instance, i915_vma_pin},
    intel_display_types_upstream::{IntelFramebuffer, IntelInitialPlaneConfig, IntelPlaneState},
    intel_fb_upstream::{intel_fb_modifier_to_tiling, intel_framebuffer_init, intel_plane_uses_fence},
    intel_ggtt_fencing_upstream::i915_vma_pin_fence,
    i915_bo_upstream::i915_bo_fbdev_prefer_stolen,
    intel_ggtt_upstream::intel_ggtt_read_entry,
    intel_memory_region_upstream::{intel_memory_region_by_type, intel_memory_type_str},
    intel_parent_upstream::IntelDisplayInitialPlaneInterface,
    linux::{
        gem_memory::{
            INTEL_MEMORY_LOCAL, INTEL_MEMORY_STOLEN_LOCAL, INTEL_MEMORY_STOLEN_SYSTEM,
            IntelMemoryRegion, drm_mm_node_allocated, intel_memory_type_is_local,
        },
        i915::{GRAPHICS_VER_FULL, HAS_LMEM, HAS_WT, IP_VER, IS_DGFX, i915_ggtt_offset, to_gt, to_i915},
        registers::PIN_GLOBAL,
    },
    linux_assert::MISSING_CASE,
    linux_config::{I915_BO_ALLOC_USER, I915_BO_PREALLOC, IS_ERR},
    linux_i915_private::DrmI915Private,
};

unsafe extern "C" {
    fn drm_mm_remove_node(node: *mut DrmMmNode);
}

/// `I915_GTT_MIN_ALIGNMENT` (intel_gtt.h), as the `u32` used by this file.
const I915_GTT_MIN_ALIGNMENT_U32: u32 = 4096;
/// Kconfig values from config/linux/7.2.3-q35-abi.config (both `=y`).
const CONFIG_DRM_FBDEV_EMULATION: bool = true;
const CONFIG_FRAMEBUFFER_CONSOLE: bool = true;
/// `I915_CACHE_NONE` / `I915_CACHE_WT` as the `unsigned int` cache level.
const I915_CACHE_NONE_U32: u32 = I915CacheLevel::I915_CACHE_NONE as u32;
const I915_CACHE_WT_U32: u32 = I915CacheLevel::I915_CACHE_WT as u32;

/// `HAS_LMEMBAR_SMEM_STOLEN(i915)` from i915_drv.h: `!HAS_LMEM(i915) &&
/// GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70)`.
#[allow(non_snake_case)]
unsafe fn HAS_LMEMBAR_SMEM_STOLEN(i915: *mut DrmI915Private) -> bool {
    unsafe { !HAS_LMEM(i915) && GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) }
}

// upstream: i915_initial_plane.c initial_plane_memory_type()
unsafe fn initial_plane_memory_type(i915: *mut DrmI915Private) -> u16 {
    if unsafe { IS_DGFX(i915) } {
        INTEL_MEMORY_LOCAL
    } else if unsafe { HAS_LMEMBAR_SMEM_STOLEN(i915) } {
        INTEL_MEMORY_STOLEN_LOCAL
    } else {
        INTEL_MEMORY_STOLEN_SYSTEM
    }
}

// upstream: i915_initial_plane.c initial_plane_phys()
unsafe fn initial_plane_phys(
    i915: *mut DrmI915Private,
    base: u32,
    out_phys_base: *mut u64,
    out_mem: *mut *mut IntelMemoryRegion,
) -> bool {
    unsafe {
        let ggtt = (*to_gt(i915)).ggtt;
        let mem_type = initial_plane_memory_type(i915);
        let mem = intel_memory_region_by_type(i915, mem_type);
        if mem.is_null() {
            drm_dbg!(
                &(*i915).drm,
                "Initial plane memory region (type %s) not initialized\n",
                intel_memory_type_str(mem_type)
            );
            return false;
        }

        // round_down(base, I915_GTT_MIN_ALIGNMENT)
        let base = base & !(I915_GTT_MIN_ALIGNMENT_U32 - 1);

        let mut is_present = false;
        let mut is_local = false;
        let dma_addr =
            intel_ggtt_read_entry(&mut (*ggtt).vm, base as u64, &mut is_present, &mut is_local);

        if !is_present {
            drm_err!(&(*i915).drm, "Initial plane FB PTE not present\n");
            return false;
        }

        if intel_memory_type_is_local((*mem).r#type) != is_local {
            drm_err!(
                &(*i915).drm,
                "Initial plane FB PTE unsuitable for %s\n",
                (*mem).region.name
            );
            return false;
        }

        if dma_addr < (*mem).region.start || dma_addr > (*mem).region.end {
            drm_err!(
                &(*i915).drm,
                "Initial plane programming using invalid range, dma_addr=%llx (%s [%llx-%llx])\n",
                dma_addr,
                (*mem).region.name,
                (*mem).region.start,
                (*mem).region.end
            );
            return false;
        }

        drm_dbg!(
            &(*i915).drm,
            "Using dma_addr=%llx, based on initial plane programming\n",
            dma_addr
        );

        *out_phys_base = dma_addr - (*mem).region.start;
        *out_mem = mem;

        true
    }
}

// upstream: i915_initial_plane.c initial_plane_vma()
unsafe fn initial_plane_vma(
    i915: *mut DrmI915Private,
    plane_config: *mut IntelInitialPlaneConfig,
) -> *mut I915Vma {
    unsafe {
        let mut mem: *mut IntelMemoryRegion = ptr::null_mut();
        let mut phys_base: u64 = 0;
        let mut orig_mm: DrmMmNode = zeroed();
        let vma: *mut I915Vma;

        if (*plane_config).size == 0 {
            return ptr::null_mut();
        }

        if !initial_plane_phys(i915, (*plane_config).base, &mut phys_base, &mut mem) {
            return ptr::null_mut();
        }

        let mut base: u32 = (*plane_config).base & !(I915_GTT_MIN_ALIGNMENT_U32 - 1);
        // round_up(plane_config->base + plane_config->size, mem->min_page_size) - base,
        // computed in the C `u32` domain for the operands and truncated to `u32`.
        let end = (*plane_config).base.wrapping_add((*plane_config).size as u32) as u64;
        let min_page = (*mem).min_page_size;
        let rounded_end = ((end + min_page - 1) / min_page) * min_page;
        let size: u32 = (rounded_end as u32).wrapping_sub(base);

        // IS_ENABLED(CONFIG_DRM_FBDEV_EMULATION) && IS_ENABLED(CONFIG_FRAMEBUFFER_CONSOLE)
        // are both `=y` in config/linux/7.2.3-q35-abi.config.
        if CONFIG_DRM_FBDEV_EMULATION
            && CONFIG_FRAMEBUFFER_CONSOLE
            && mem == (*i915).mm.stolen_region
            && !unsafe { i915_bo_fbdev_prefer_stolen(i915, size) }
        {
            drm_dbg!(
                &(*i915).drm,
                "Initial FB size exceeds half of stolen, discarding\n"
            );
            return ptr::null_mut();
        }

        let obj: *mut DrmI915GemObject = i915_gem_object_create_region_at(
            mem,
            phys_base,
            size as u64,
            I915_BO_ALLOC_USER | I915_BO_PREALLOC,
        );
        if IS_ERR(obj) {
            drm_dbg!(
                &(*i915).drm,
                "Failed to preallocate initial FB in %s\n",
                (*mem).region.name
            );
            return ptr::null_mut();
        }

        // Mark it WT ahead of time to avoid changing the cache_level during
        // fbdev initialization. The unbind there would get stuck waiting for rcu.
        i915_gem_object_set_cache_coherency(
            obj,
            if HAS_WT(i915) { I915_CACHE_WT_U32 } else { I915_CACHE_NONE_U32 },
        );

        let tiling = intel_fb_modifier_to_tiling((*(*plane_config).fb).modifier);

        // `goto err_obj` in the C body is `break 'err_obj null`; the cleanup
        // below runs only when the labeled block yields null.
        vma = 'err_obj: {
            match tiling {
                I915_TILING_NONE => {}
                I915_TILING_X | I915_TILING_Y => {
                    (*obj).tiling_and_stride = (*(*plane_config).fb).pitches[0] | tiling;
                }
                _ => {
                    MISSING_CASE(tiling);
                    break 'err_obj ptr::null_mut();
                }
            }

            // MTL GOP likes to place the framebuffer high up in ggtt, which can
            // cause problems for ggtt_reserve_guc_top(). Pin it low instead.
            base = 0;

            if base != (*plane_config).base {
                let ggtt = (*to_gt(i915)).ggtt;
                // Make sure the original and new locations can't overlap. That
                // would corrupt the original PTEs which are still used for scanout.
                let ret = i915_gem_gtt_reserve(
                    &mut (*ggtt).vm,
                    ptr::null_mut(),
                    &mut orig_mm,
                    size as u64,
                    (*plane_config).base as u64,
                    c_ulong::MAX,
                    crate::linux_config::PIN_NOEVICT as u32,
                );
                if ret != 0 {
                    break 'err_obj ptr::null_mut();
                }
            }

            let ggtt = (*to_gt(i915)).ggtt;
            let vma = i915_vma_instance(obj, &mut (*ggtt).vm, ptr::null());
            if IS_ERR(vma) {
                break 'err_obj ptr::null_mut();
            }

            // retry:
            loop {
                let mut pinctl = PIN_GLOBAL | PIN_OFFSET_FIXED | base as u64;
                if !i915_gem_object_is_lmem(obj) {
                    pinctl |= PIN_MAPPABLE;
                }
                if i915_vma_pin(vma, 0, 0, pinctl) != 0 {
                    if drm_mm_node_allocated(&orig_mm) {
                        drm_mm_remove_node(&mut orig_mm);
                        // Try again, but this time pin it to its original location.
                        base = (*plane_config).base;
                        continue;
                    }
                    break 'err_obj ptr::null_mut();
                }
                break;
            }

            if i915_gem_object_is_tiled(obj) && !i915_vma_is_map_and_fenceable(vma) {
                break 'err_obj ptr::null_mut();
            }

            if drm_mm_node_allocated(&orig_mm) {
                drm_mm_remove_node(&mut orig_mm);
            }

            drm_dbg!(
                &(*i915).drm,
                "Initial plane fb bound to 0x%x in the ggtt (original 0x%x)\n",
                i915_ggtt_offset(vma),
                (*plane_config).base
            );

            vma
        };

        if !vma.is_null() {
            return vma;
        }

        // err_obj:
        if drm_mm_node_allocated(&orig_mm) {
            drm_mm_remove_node(&mut orig_mm);
        }
        i915_gem_object_put(obj);
        ptr::null_mut()
    }
}

// upstream: i915_initial_plane.c i915_alloc_initial_plane_obj()
unsafe extern "C" fn i915_alloc_initial_plane_obj(
    drm: *mut DrmDevice,
    plane_config: *mut IntelInitialPlaneConfig,
) -> *mut c_void {
    unsafe {
        let i915 = to_i915(drm.cast());
        let mut mode_cmd: DrmModeFbCmd2 = zeroed();
        let fb: *mut DrmFramebuffer = (*plane_config).fb;
        let vma = initial_plane_vma(i915, plane_config);

        if vma.is_null() {
            return ptr::null_mut();
        }

        mode_cmd.pixel_format = (*(*fb).format).format;
        mode_cmd.width = (*fb).width;
        mode_cmd.height = (*fb).height;
        mode_cmd.pitches[0] = (*fb).pitches[0];
        mode_cmd.modifier[0] = (*fb).modifier;
        mode_cmd.flags = DRM_MODE_FB_MODIFIERS;

        // to_intel_framebuffer(fb): `base` is the first member of IntelFramebuffer.
        if intel_framebuffer_init(
            fb.cast::<IntelFramebuffer>(),
            intel_bo_to_drm_bo((*vma).obj).cast::<DrmGemObject>(),
            (*fb).format,
            &mut mode_cmd,
        ) != 0
        {
            drm_dbg!(&(*i915).drm, "intel fb init failed\n");
            i915_vma_put(vma);
            return ptr::null_mut();
        }

        (*plane_config).vma = vma.cast();
        intel_bo_to_drm_bo((*vma).obj).cast()
    }
}

// upstream: i915_initial_plane.c i915_initial_plane_setup()
unsafe extern "C" fn i915_initial_plane_setup(
    plane_state: *mut DrmPlaneState,
    _plane_config: *mut IntelInitialPlaneConfig,
    fb: *mut DrmFramebuffer,
    vma_ptr: *mut c_void,
) -> c_int {
    unsafe {
        let vma = vma_ptr.cast::<I915Vma>();
        // to_intel_plane_state(_plane_state): `uapi` is embedded in IntelPlaneState.
        let state = (plane_state as *mut u8)
            .sub(offset_of!(IntelPlaneState, uapi))
            .cast::<IntelPlaneState>();
        let dev_priv = to_i915((*(*plane_state).plane).dev.cast());

        __i915_vma_pin(vma);
        (*state).ggtt_vma = i915_vma_get(vma).cast();
        if intel_plane_uses_fence(state) && i915_vma_pin_fence(vma) == 0 && !(*vma).fence.is_null() {
            (*state).fence_id = (*(*vma).fence).id as i8;
        }

        (*state).surf = i915_ggtt_offset((*state).ggtt_vma.cast::<I915Vma>());

        if (*fb).modifier != DRM_FORMAT_MOD_LINEAR {
            (*dev_priv).preserve_bios_swizzle = true;
        }

        0
    }
}

// upstream: i915_initial_plane.c i915_plane_config_fini()
unsafe extern "C" fn i915_plane_config_fini(plane_config: *mut IntelInitialPlaneConfig) {
    unsafe {
        if !(*plane_config).vma.is_null() {
            i915_vma_put((*plane_config).vma.cast());
        }
    }
}

// upstream: i915_initial_plane.c i915_display_initial_plane_interface
#[unsafe(no_mangle)]
pub static i915_display_initial_plane_interface: IntelDisplayInitialPlaneInterface =
    IntelDisplayInitialPlaneInterface {
        alloc_obj: Some(i915_alloc_initial_plane_obj),
        setup: Some(i915_initial_plane_setup),
        config_fini: Some(i915_plane_config_fini),
    };
