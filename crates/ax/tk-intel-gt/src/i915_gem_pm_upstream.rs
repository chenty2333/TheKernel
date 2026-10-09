// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gem/i915_gem_pm.c.
// The target config has CONFIG_DRM_I915_TRACE_GEM unset, so GEM_TRACE calls
// compile away upstream and are intentionally not evaluated here.

#![allow(dead_code, non_camel_case_types, non_snake_case, unsafe_code)]

use core::ffi::{c_char, c_int, c_ulong, c_void};

use crate::{
    for_each_gt,
    i915_gem_core_upstream::i915_gem_drain_freed_objects,
    i915_gem_object_header_upstream::__start_cpu_write,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_CACHE_COHERENT_FOR_READ},
    i915_gem_shrinker_upstream::{i915_gem_shrink, i915_gem_shrink_all},
    intel_gt_api_upstream::intel_gt_is_wedged,
    intel_gt_types_upstream::IntelGt,
    intel_wakeref_types_upstream::intel_wakeref_auto,
    linux::{
        fields::i915_gem_object_cache_coherent,
        gem_memory::{INTEL_MEMORY_LOCAL, INTEL_REGION_UNKNOWN, IntelMemoryRegion},
        i915_private::DrmI915Private,
        locks::{spin_lock_irqsave, spin_unlock_irqrestore},
        rcu::rcu_barrier,
    },
};

const I915_TTM_BACKUP_ALLOW_GPU: u32 = 1 << 0;
const I915_TTM_BACKUP_PINNED: u32 = 1 << 1;
const I915_GEM_DOMAIN_CPU: u16 = 1;

unsafe extern "C" {
    // These are the actual TTM/GEM owners. The Rust port keeps the source's
    // local-memory type checks and does not invent success behavior when a
    // platform has no LMEM region.
    fn i915_ttm_restore_region(region: *mut IntelMemoryRegion, flags: u32) -> c_int;
    fn i915_ttm_backup_region(region: *mut IntelMemoryRegion, flags: u32) -> c_int;
    fn i915_ttm_recover_region(region: *mut IntelMemoryRegion);

    fn intel_gt_suspend_prepare(gt: *mut IntelGt);
    fn intel_gt_suspend_late(gt: *mut IntelGt);
    fn intel_gt_resume(gt: *mut IntelGt) -> c_int;
    fn intel_gt_set_wedged(gt: *mut IntelGt);

    fn flush_workqueue(workqueue: *mut c_void);
    fn wbinvd_on_all_cpus();
    fn _dev_err(device: *const c_void, format: *const c_char, ...);
}

// upstream: gem/i915_gem_pm.c i915_gem_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_suspend(i915: *mut DrmI915Private) {
    unsafe {
        intel_wakeref_auto(
            core::ptr::addr_of_mut!((*i915).runtime_pm.userfault_wakeref),
            0,
        );
    }
    rcu_barrier();
    unsafe { flush_workqueue((*i915).wq) };

    for_each_gt!(gt, i915, i, {
        unsafe { intel_gt_suspend_prepare(gt) };
    });

    unsafe { i915_gem_drain_freed_objects(i915) };
}

// upstream: gem/i915_gem_pm.c lmem_restore()
unsafe fn lmem_restore(i915: *mut DrmI915Private, flags: u32) -> c_int {
    let mut ret = 0;
    for id in 0..INTEL_REGION_UNKNOWN as usize {
        let mr = unsafe { (*i915).mm.regions[id] };
        if mr.is_null() {
            continue;
        }
        if unsafe { (*mr).r#type } == INTEL_MEMORY_LOCAL {
            ret = unsafe { i915_ttm_restore_region(mr, flags) };
            if ret != 0 {
                break;
            }
        }
    }
    ret
}

// upstream: gem/i915_gem_pm.c lmem_suspend()
unsafe fn lmem_suspend(i915: *mut DrmI915Private, flags: u32) -> c_int {
    let mut ret = 0;
    for id in 0..INTEL_REGION_UNKNOWN as usize {
        let mr = unsafe { (*i915).mm.regions[id] };
        if mr.is_null() {
            continue;
        }
        if unsafe { (*mr).r#type } == INTEL_MEMORY_LOCAL {
            ret = unsafe { i915_ttm_backup_region(mr, flags) };
            if ret != 0 {
                break;
            }
        }
    }
    ret
}

// upstream: gem/i915_gem_pm.c lmem_recover()
unsafe fn lmem_recover(i915: *mut DrmI915Private) {
    for id in 0..INTEL_REGION_UNKNOWN as usize {
        let mr = unsafe { (*i915).mm.regions[id] };
        if !mr.is_null() && unsafe { (*mr).r#type } == INTEL_MEMORY_LOCAL {
            unsafe { i915_ttm_recover_region(mr) };
        }
    }
}

// upstream: gem/i915_gem_pm.c i915_gem_backup_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_backup_suspend(i915: *mut DrmI915Private) -> c_int {
    let mut ret = unsafe { lmem_suspend(i915, I915_TTM_BACKUP_ALLOW_GPU) };
    if ret != 0 {
        unsafe { lmem_recover(i915) };
        return ret;
    }

    unsafe { i915_gem_suspend(i915) };

    ret = unsafe { lmem_suspend(i915, I915_TTM_BACKUP_ALLOW_GPU | I915_TTM_BACKUP_PINNED) };
    if ret != 0 {
        unsafe { lmem_recover(i915) };
        return ret;
    }

    ret = unsafe { lmem_suspend(i915, I915_TTM_BACKUP_PINNED) };
    if ret != 0 {
        unsafe { lmem_recover(i915) };
        return ret;
    }
    0
}

// upstream: gem/i915_gem_pm.c i915_gem_suspend_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_suspend_late(i915: *mut DrmI915Private) {
    let mut flush = false;
    rcu_barrier();

    for_each_gt!(gt, i915, i, {
        unsafe { intel_gt_suspend_late(gt) };
    });

    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags) };

    let phases = unsafe {
        [
            core::ptr::addr_of_mut!((*i915).mm.shrink_list),
            core::ptr::addr_of_mut!((*i915).mm.purge_list),
            core::ptr::null_mut(),
        ]
    };
    let mut obj: *mut DrmI915GemObject = core::ptr::null_mut();
    for phase in phases {
        if phase.is_null() {
            break;
        }
        list_for_each_entry!(obj, phase, mm.link, {
            if unsafe { i915_gem_object_cache_coherent(obj) } & I915_BO_CACHE_COHERENT_FOR_READ == 0
            {
                flush |= unsafe { (*obj).read_domains & I915_GEM_DOMAIN_CPU == 0 };
            }
            unsafe { __start_cpu_write(obj) };
        });
    }

    unsafe { spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags) };
    if flush {
        unsafe { wbinvd_on_all_cpus() };
    }
}

// upstream: gem/i915_gem_pm.c i915_gem_freeze()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_freeze(i915: *mut DrmI915Private) -> c_int {
    unsafe { i915_gem_shrink_all(i915) };
    0
}

// upstream: gem/i915_gem_pm.c i915_gem_freeze_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_freeze_late(i915: *mut DrmI915Private) -> c_int {
    with_intel_runtime_pm!(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref, {
        let _ = unsafe {
            i915_gem_shrink(
                core::ptr::null_mut(),
                i915,
                c_ulong::MAX,
                core::ptr::null_mut(),
                u32::MAX,
            )
        };
    });
    unsafe { i915_gem_drain_freed_objects(i915) };

    unsafe { wbinvd_on_all_cpus() };
    let head = unsafe { core::ptr::addr_of_mut!((*i915).mm.shrink_list) };
    let mut obj: *mut DrmI915GemObject = core::ptr::null_mut();
    list_for_each_entry!(obj, head, mm.link, {
        unsafe { __start_cpu_write(obj) };
    });

    0
}

// upstream: gem/i915_gem_pm.c i915_gem_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_resume(i915: *mut DrmI915Private) {
    let mut ret = unsafe { lmem_restore(i915, 0) };
    GEM_WARN_ON!(ret != 0);

    // The first restore error is warning-only; the GT loop has its own
    // failure edge to err_wedged.
    ret = 0;
    let mut i = 0usize;
    for_each_gt!(gt, i915, i, {
        ret = unsafe { intel_gt_resume(gt) };
        if ret != 0 {
            break;
        }
    });
    if ret != 0 {
        let mut j = 0usize;
        for_each_gt!(gt, i915, j, {
            if !unsafe { intel_gt_is_wedged(gt) } {
                unsafe {
                    _dev_err(
                        (*i915).drm.dev,
                        c"Failed to re-initialize GPU[%u], declaring it wedged!\n".as_ptr(),
                        j as u32,
                    );
                    intel_gt_set_wedged(gt);
                }
            }
            if j == i {
                break;
            }
        });
        return;
    }

    ret = unsafe { lmem_restore(i915, I915_TTM_BACKUP_ALLOW_GPU) };
    GEM_WARN_ON!(ret != 0);
}
