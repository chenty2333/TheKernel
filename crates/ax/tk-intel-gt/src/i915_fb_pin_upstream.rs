// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation
// Source: drivers/gpu/drm/i915/i915_fb_pin.c
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{c_int, c_void},
    mem::MaybeUninit,
    ptr,
};

use crate::{
    i915_dpt_upstream::{
        IntelDpt, i915_dpt_offset, i915_dpt_pin_to_ggtt, i915_dpt_to_vm, i915_dpt_unpin_from_ggtt,
    },
    i915_gem_domain_upstream::{
        I915CacheLevel, i915_gem_object_flush_if_display, i915_gem_object_pin_to_display_plane,
        i915_gem_object_set_cache_level,
    },
    i915_gem_object_api_upstream::{
        i915_gem_object_lock, i915_gem_object_pin_pages, i915_gem_object_unpin_pages,
    },
    i915_gem_object_header_upstream::i915_gem_object_is_framebuffer,
    i915_gem_object_types_upstream::to_intel_bo,
    i915_gem_object_upstream::{__i915_gem_object_migrate, i915_gem_object_migrate},
    i915_gem_pages_upstream::__i915_gem_object_get_dma_address,
    i915_gem_phys_upstream::i915_gem_object_attach_phys,
    i915_gem_ww_upstream::{
        I915GemWwCtx, __i915_gem_ww_fini, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini,
        i915_gem_ww_ctx_init,
    },
    i915_vma_api_upstream::{
        i915_vma_get, i915_vma_get_iomap, i915_vma_instance, i915_vma_misplaced,
        i915_vma_pin_fence, i915_vma_pin_ww, i915_vma_put, i915_vma_unbind, i915_vma_unpin,
        i915_vma_unpin_fence,
    },
    i915_vma_types_upstream::I915Vma,
    i915_vma_upstream::PIN_MAPPABLE,
    intel_parent_upstream::IntelDisplayFbPinInterface,
    intel_plane_upstream::IntelFbPinParams,
    intel_ring_upstream::i915_vma_is_map_and_fenceable,
    linux::{
        gem_memory::INTEL_REGION_LMEM_0,
        i915::{HAS_LMEM, i915_ggtt_offset, to_i915},
        iosys_map::IosysMap,
        pm::{intel_runtime_pm_get, intel_runtime_pm_put},
        registers::PIN_GLOBAL,
    },
    linux_config::{
        EDEADLK, EINVAL, ERR_PTR, I915_BO_ALLOC_GPU_ONLY, IS_ERR, PTR_ERR,
    },
    linux_memory::{atomic_dec, atomic_inc},
};

// upstream: i915_fb_pin.c intel_fb_pin_to_dpt()
unsafe fn intel_fb_pin_to_dpt(
    obj_: *mut c_void,
    dpt: *mut IntelDpt,
    pin_params: *const IntelFbPinParams,
) -> *mut I915Vma {
    unsafe {
        let obj = to_intel_bo(obj_.cast());
        let i915 = to_i915((&(*obj).base.base).dev.cast());
        let vm = i915_dpt_to_vm(dpt);

        // We are not syncing against the binding (and potential migrations)
        // below, so this vm must never be async.
        if drm_WARN_ON!(&mut (*i915).drm, (*vm).bind_async_flags != 0) {
            return ERR_PTR(-EINVAL);
        }

        if WARN_ON!(!i915_gem_object_is_framebuffer(obj)) {
            return ERR_PTR(-EINVAL);
        }

        atomic_inc(&mut (*i915).pending_fb_pin);

        let mut ww = MaybeUninit::<I915GemWwCtx>::uninit();
        let ww = ww.as_mut_ptr();
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut ret: i32 = -EDEADLK;

        // for_i915_gem_ww(&ww, ret, true)
        i915_gem_ww_ctx_init(ww, true);
        while ret == -EDEADLK {
            ret = 'body: {
                let r = i915_gem_object_lock(obj, ww);
                if r != 0 {
                    break 'body r;
                }

                if HAS_LMEM(i915) {
                    let mut flags = (*obj).flags as u32;

                    // For this type of buffer we need to able to read from the CPU
                    // the clear color value found in the buffer, hence we need to
                    // ensure it is always in the mappable part of lmem, if this is
                    // a small-bar device.
                    if (*pin_params).needs_cpu_lmem_access {
                        flags &= !I915_BO_ALLOC_GPU_ONLY;
                    }
                    let r = __i915_gem_object_migrate(obj, ww, INTEL_REGION_LMEM_0, flags);
                    if r != 0 {
                        break 'body r;
                    }
                }

                let r = i915_gem_object_set_cache_level(obj, I915CacheLevel::None);
                if r != 0 {
                    break 'body r;
                }

                vma = i915_vma_instance(obj, vm, (*pin_params).view);
                if IS_ERR(vma) {
                    break 'body PTR_ERR(vma);
                }

                if i915_vma_misplaced(vma, 0, u64::from((*pin_params).alignment), 0) {
                    let r = i915_vma_unbind(vma);
                    if r != 0 {
                        break 'body r;
                    }
                }

                let r = i915_vma_pin_ww(
                    vma,
                    ww,
                    0,
                    u64::from((*pin_params).alignment),
                    PIN_GLOBAL,
                );
                if r != 0 {
                    break 'body r;
                }
                0
            };
            ret = __i915_gem_ww_fini(ww, ret);
        }

        if ret != 0 {
            vma = ERR_PTR(ret);
        } else {
            (*vma).display_alignment = (*vma).display_alignment.max((*pin_params).alignment);

            i915_gem_object_flush_if_display(obj);

            i915_vma_get(vma);

            // The DPT object contains only one vma, and there is no VT-d
            // guard, so the VMA's offset within the DPT is always 0.
            drm_WARN_ON!(&mut (*i915).drm, i915_dpt_offset(vma) != 0);
        }

        atomic_dec(&mut (*i915).pending_fb_pin);

        vma
    }
}

// upstream: i915_fb_pin.c intel_fb_pin_to_ggtt()
unsafe fn intel_fb_pin_to_ggtt(
    obj_: *mut c_void,
    pin_params: *const IntelFbPinParams,
    out_fence_id: *mut i32,
) -> *mut I915Vma {
    unsafe {
        let obj = to_intel_bo(obj_.cast());
        let i915 = to_i915((&(*obj).base.base).dev.cast());

        if drm_WARN_ON!(&mut (*i915).drm, !i915_gem_object_is_framebuffer(obj)) {
            return ERR_PTR(-EINVAL);
        }

        if drm_WARN_ON!(
            &mut (*i915).drm,
            (*pin_params).alignment != 0 && !(*pin_params).alignment.is_power_of_two()
        ) {
            return ERR_PTR(-EINVAL);
        }

        // Global gtt pte registers are special registers which actually forward
        // writes to a chunk of system memory. Which means that there is no risk
        // that the register values disappear as soon as we call
        // intel_runtime_pm_put(), so it is correct to wrap only the
        // pin/unpin/fence and not more.
        let wakeref = intel_runtime_pm_get(&mut (*i915).runtime_pm);

        atomic_inc(&mut (*i915).pending_fb_pin);

        let mut pinctl: u32 = 0;
        // PIN_MAPPABLE limits the address to GMADR size.
        if (*pin_params).needs_low_address {
            pinctl |= PIN_MAPPABLE as u32;
        }

        let mut ww = MaybeUninit::<I915GemWwCtx>::uninit();
        let ww = ww.as_mut_ptr();
        i915_gem_ww_ctx_init(ww, true);

        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut ret: i32;
        loop {
            // retry:
            ret = i915_gem_object_lock(obj, ww);
            if ret == 0 && (*pin_params).needs_physical {
                ret = i915_gem_object_attach_phys(obj, (*pin_params).phys_alignment as c_int);
            } else if ret == 0 && HAS_LMEM(i915) {
                ret = i915_gem_object_migrate(obj, ww, INTEL_REGION_LMEM_0);
            }
            if ret == 0 {
                ret = i915_gem_object_pin_pages(obj);
            }

            if ret == 0 {
                vma = i915_gem_object_pin_to_display_plane(
                    obj,
                    ww,
                    (*pin_params).alignment,
                    (*pin_params).vtd_guard,
                    (*pin_params).view,
                    pinctl,
                );
                if IS_ERR(vma) {
                    ret = PTR_ERR(vma);
                } else {
                    if !out_fence_id.is_null() {
                        *out_fence_id = -1;
                    }

                    if !out_fence_id.is_null() && i915_vma_is_map_and_fenceable(vma) {
                        // Install a fence for tiled scan-out. Pre-i965 always needs a
                        // fence, whereas 965+ only requires a fence if using
                        // framebuffer compression. For simplicity, we always, when
                        // possible, install a fence as the cost is not that onerous.
                        ret = i915_vma_pin_fence(vma);
                        if ret != 0 && (*pin_params).needs_fence {
                            i915_vma_unpin(vma);
                        } else {
                            ret = 0;

                            if !(*vma).fence.is_null() {
                                *out_fence_id = (*(*vma).fence).id;
                            }
                        }
                    }

                    if ret == 0 {
                        i915_vma_get(vma);
                    }
                }

                // err_unpin:
                i915_gem_object_unpin_pages(obj);
            }

            // err:
            if ret == -EDEADLK {
                ret = i915_gem_ww_ctx_backoff(ww);
                if ret == 0 {
                    continue;
                }
            }
            break;
        }
        i915_gem_ww_ctx_fini(ww);

        if ret != 0 {
            vma = ERR_PTR(ret);
        }

        atomic_dec(&mut (*i915).pending_fb_pin);
        intel_runtime_pm_put(&mut (*i915).runtime_pm, wakeref);
        vma
    }
}

// upstream: i915_fb_pin.c intel_fb_unpin_vma()
unsafe fn intel_fb_unpin_vma(vma: *mut I915Vma, fence_id: i32) {
    unsafe {
        if fence_id >= 0 {
            i915_vma_unpin_fence(vma);
        }
        i915_vma_unpin(vma);
        i915_vma_put(vma);
    }
}

// upstream: i915_fb_pin.c i915_fb_pin_ggtt_pin()
unsafe extern "C" fn i915_fb_pin_ggtt_pin(
    obj: *mut c_void,
    pin_params: *const c_void,
    out_ggtt_vma: *mut *mut c_void,
    out_offset: *mut u32,
    out_fence_id: *mut i32,
) -> i32 {
    unsafe {
        let pin_params = pin_params.cast::<IntelFbPinParams>();
        let ggtt_vma = intel_fb_pin_to_ggtt(obj, pin_params, out_fence_id);
        if IS_ERR(ggtt_vma) {
            return PTR_ERR(ggtt_vma);
        }

        *out_ggtt_vma = ggtt_vma.cast();

        // Pre-populate the dma address before we enter the vblank evade critical
        // section as i915_gem_object_get_dma_address() will trigger might_sleep()
        // even if it won't actually sleep, which is the case when the fb has
        // already been pinned.
        if (*pin_params).needs_physical {
            *out_offset = __i915_gem_object_get_dma_address(to_intel_bo(obj.cast()), 0) as u32;
        } else {
            *out_offset = i915_ggtt_offset(ggtt_vma);
        }

        0
    }
}

// upstream: i915_fb_pin.c i915_fb_pin_ggtt_unpin()
unsafe extern "C" fn i915_fb_pin_ggtt_unpin(ggtt_vma: *mut c_void, fence_id: i32) {
    if !ggtt_vma.is_null() {
        unsafe { intel_fb_unpin_vma(ggtt_vma.cast(), fence_id) };
    }
}

// upstream: i915_fb_pin.c i915_fb_pin_dpt_pin()
unsafe extern "C" fn i915_fb_pin_dpt_pin(
    obj: *mut c_void,
    dpt: *mut c_void,
    pin_params: *const c_void,
    out_dpt_vma: *mut *mut c_void,
    out_ggtt_vma: *mut *mut c_void,
    out_offset: *mut u32,
) -> i32 {
    unsafe {
        let dpt = dpt.cast::<IntelDpt>();
        let pin_params = pin_params.cast::<IntelFbPinParams>();

        WARN_ON!(dpt.is_null());

        let ggtt_vma = i915_dpt_pin_to_ggtt(dpt, (*pin_params).alignment / 512);
        if IS_ERR(ggtt_vma) {
            return PTR_ERR(ggtt_vma);
        }

        let dpt_vma = intel_fb_pin_to_dpt(obj, dpt, pin_params);
        if IS_ERR(dpt_vma) {
            i915_dpt_unpin_from_ggtt(dpt);
            return PTR_ERR(dpt_vma);
        }

        drm_WARN_ON!(&mut (*to_intel_bo(obj.cast())).base.base, ggtt_vma == dpt_vma);

        *out_ggtt_vma = ggtt_vma.cast();
        *out_dpt_vma = dpt_vma.cast();

        *out_offset = i915_ggtt_offset(ggtt_vma);

        0
    }
}

// upstream: i915_fb_pin.c i915_fb_pin_dpt_unpin()
unsafe extern "C" fn i915_fb_pin_dpt_unpin(
    dpt: *mut c_void,
    dpt_vma: *mut c_void,
    ggtt_vma: *mut c_void,
) {
    unsafe {
        WARN_ON!(dpt.is_null());
        WARN_ON!(dpt_vma.is_null() != ggtt_vma.is_null());

        if !dpt_vma.is_null() {
            intel_fb_unpin_vma(dpt_vma.cast(), -1);
        }
        if !ggtt_vma.is_null() {
            i915_dpt_unpin_from_ggtt(dpt.cast());
        }
    }
}

// upstream: i915_fb_pin.c i915_fb_pin_get_map()
unsafe extern "C" fn i915_fb_pin_get_map(vma: *mut c_void, map: *mut c_void) {
    unsafe {
        (*map.cast::<IosysMap>()).set_vaddr_iomem(i915_vma_get_iomap(vma.cast()));
    }
}

/// i915-owned framebuffer pin callbacks exported to the display parent.
#[unsafe(no_mangle)]
pub static i915_display_fb_pin_interface: IntelDisplayFbPinInterface =
    IntelDisplayFbPinInterface {
        ggtt_pin: Some(i915_fb_pin_ggtt_pin),
        ggtt_unpin: Some(i915_fb_pin_ggtt_unpin),
        dpt_pin: Some(i915_fb_pin_dpt_pin),
        dpt_unpin: Some(i915_fb_pin_dpt_unpin),
        // i915_fb_pin.c does not set `reuse_vma`; it stays NULL as in C.
        reuse_vma: None,
        get_map: Some(i915_fb_pin_get_map),
    };
