// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Port of the GuC log buffer lifetime from Linux 7.2.3
// drivers/gpu/drm/i915/gt/uc/intel_guc_log.c (Copyright © 2014-2019 Intel
// Corporation, MIT): section sizing, vma allocation/mapping, default level,
// init_early, destroy and flush-event dispatch.  The relayfs/debugfs channel
// (guc_log_relay_*, intel_guc_log_relay_*) is not ported: it is only reachable
// through debugfs, and `relay.started` is never set without it, so the flush
// work is never queued.

#![allow(unsafe_op_in_unsafe_fn)]

use core::{ffi::c_void, ptr};

use crate::{
    i915_gem_object_types_upstream::I915_MAP_WC,
    intel_guc_fwif_types_upstream::{
        GUC_LOG_CAPTURE_ALLOC_UNITS, GUC_LOG_CAPTURE_MASK, GUC_LOG_CAPTURE_SHIFT,
        GUC_LOG_CRASH_MASK, GUC_LOG_CRASH_SHIFT, GUC_LOG_DEBUG_MASK, GUC_LOG_DEBUG_SHIFT,
        GUC_LOG_LOG_ALLOC_UNITS,
    },
    intel_guc_log_types_upstream::{
        GUC_LOG_LEVEL_IS_VERBOSE, GUC_LOG_LEVEL_TO_VERBOSITY, GUC_LOG_SECTIONS_CAPTURE,
        GUC_LOG_SECTIONS_CRASH, GUC_LOG_SECTIONS_DEBUG, GUC_LOG_SECTIONS_LIMIT, IntelGucLog,
    },
    intel_guc_types_upstream::IntelGuc,
    intel_gt_api_upstream::guc_to_i915,
    linux::{
        mutex::mutex_init,
        workqueue::{INIT_WORK_C, queue_work, system_highpri_wq},
    },
    linux_config::{IS_ERR, PTR_ERR},
};

const PAGE_SIZE: u32 = 4096;
const SZ_4K: u32 = 4 * 1024;
const SZ_64K: u32 = 64 * 1024;
const SZ_1M: u32 = 1024 * 1024;
const SZ_2M: u32 = 2 * 1024 * 1024;
const SZ_8K: u32 = 8 * 1024;
const SZ_16M: u32 = 16 * 1024 * 1024;

// upstream: intel_guc_log.c GUC_LOG_DEFAULT_*_BUFFER_SIZE (CONFIG_DRM_I915_DEBUG=n, DEBUG_GEM=n)
const GUC_LOG_DEFAULT_CRASH_BUFFER_SIZE: u32 = SZ_8K;
const GUC_LOG_DEFAULT_DEBUG_BUFFER_SIZE: u32 = SZ_64K;
const GUC_LOG_DEFAULT_CAPTURE_BUFFER_SIZE: u32 = SZ_1M;

const GUC_LOG_LEVEL_DISABLED: u32 = 0;
const GUC_LOG_LEVEL_NON_VERBOSE: u32 = 1;
const GUC_LOG_LEVEL_MAX: u32 = 5;

// Sections of `sizes[]`, in the order of the C `sections[]` table.
struct GucLogSection {
    max: u32,
    flag: u32,
    default_val: u32,
}

const SECTIONS: [GucLogSection; GUC_LOG_SECTIONS_LIMIT as usize] = [
    GucLogSection {
        max: GUC_LOG_CRASH_MASK >> GUC_LOG_CRASH_SHIFT,
        flag: GUC_LOG_LOG_ALLOC_UNITS,
        default_val: GUC_LOG_DEFAULT_CRASH_BUFFER_SIZE,
    },
    GucLogSection {
        max: GUC_LOG_DEBUG_MASK >> GUC_LOG_DEBUG_SHIFT,
        flag: GUC_LOG_LOG_ALLOC_UNITS,
        default_val: GUC_LOG_DEFAULT_DEBUG_BUFFER_SIZE,
    },
    GucLogSection {
        max: GUC_LOG_CAPTURE_MASK >> GUC_LOG_CAPTURE_SHIFT,
        flag: GUC_LOG_CAPTURE_ALLOC_UNITS,
        default_val: GUC_LOG_DEFAULT_CAPTURE_BUFFER_SIZE,
    },
];

#[inline]
unsafe fn log_to_guc(log: *mut IntelGucLog) -> *mut IntelGuc {
    // SAFETY: `log` is the `guc->log` member of a live `intel_guc`.
    let offset = core::mem::offset_of!(IntelGuc, log);
    (log as *mut u8).sub(offset) as *mut IntelGuc
}

// upstream: intel_guc_log.c _guc_log_init_sizes()
unsafe fn _guc_log_init_sizes(log: *mut IntelGucLog) {
    let guc = log_to_guc(log);
    let i915 = guc_to_i915(guc);

    for (i, section) in SECTIONS.iter().enumerate() {
        (*log).sizes[i].bytes = section.default_val as i32;
    }

    // If debug size > 1MB then bump default crash size to keep the same units
    if (*log).sizes[GUC_LOG_SECTIONS_DEBUG as usize].bytes >= SZ_1M as i32
        && GUC_LOG_DEFAULT_CRASH_BUFFER_SIZE < SZ_1M
    {
        (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].bytes = SZ_1M as i32;
    }

    // Prepare the GuC API structure fields:
    for (i, section) in SECTIONS.iter().enumerate() {
        let sz = &mut (*log).sizes[i];
        // Convert to correct units
        if (sz.bytes as u32) % SZ_1M == 0 {
            sz.units = SZ_1M as i32;
            sz.flag = section.flag;
        } else {
            sz.units = SZ_4K as i32;
            sz.flag = 0;
        }
        if (sz.bytes as u32) % (sz.units as u32) != 0 {
            guc_err!(guc, "Mis-aligned log section size: 0x%X vs 0x%X!\n", sz.bytes, sz.units);
        }
        sz.count = sz.bytes / sz.units;
        if sz.count == 0 {
            guc_err!(guc, "Zero log section size!\n");
        } else {
            // Size is +1 unit
            sz.count -= 1;
        }
        // Clip to field size
        if sz.count as u32 > section.max {
            guc_err!(guc, "log section size too large: %d vs %d!\n", sz.count + 1, section.max + 1);
            sz.count = section.max as i32;
        }
    }

    if (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].units
        != (*log).sizes[GUC_LOG_SECTIONS_DEBUG as usize].units
    {
        guc_err!(
            guc,
            "Unit mismatch for crash and debug sections: %d vs %d!\n",
            (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].units,
            (*log).sizes[GUC_LOG_SECTIONS_DEBUG as usize].units
        );
        let debug_units = (*log).sizes[GUC_LOG_SECTIONS_DEBUG as usize].units;
        (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].units = debug_units;
        (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].count = 0;
    }

    (*log).sizes_initialised = true;
}

// upstream: intel_guc_log.c guc_log_init_sizes()
unsafe fn guc_log_init_sizes(log: *mut IntelGucLog) {
    if (*log).sizes_initialised {
        return;
    }
    _guc_log_init_sizes(log);
}

// upstream: intel_guc_log.c intel_guc_log_section_size_crash()
unsafe fn intel_guc_log_section_size_crash(log: *mut IntelGucLog) -> u32 {
    guc_log_init_sizes(log);
    (*log).sizes[GUC_LOG_SECTIONS_CRASH as usize].bytes as u32
}

// upstream: intel_guc_log.c intel_guc_log_section_size_debug()
unsafe fn intel_guc_log_section_size_debug(log: *mut IntelGucLog) -> u32 {
    guc_log_init_sizes(log);
    (*log).sizes[GUC_LOG_SECTIONS_DEBUG as usize].bytes as u32
}

// upstream: intel_guc_log.c intel_guc_log_section_size_capture()
unsafe fn intel_guc_log_section_size_capture(log: *mut IntelGucLog) -> u32 {
    guc_log_init_sizes(log);
    (*log).sizes[GUC_LOG_SECTIONS_CAPTURE as usize].bytes as u32
}

// upstream: intel_guc_log.c intel_guc_log_size()
unsafe fn intel_guc_log_size(log: *mut IntelGucLog) -> u32 {
    // GuC Log buffer Layout: debug state header (32B), crash state header
    // (32B), capture state header (32B), padding to PAGE_SIZE, then the
    // debug, crash and capture sections.
    PAGE_SIZE
        + intel_guc_log_section_size_crash(log)
        + intel_guc_log_section_size_debug(log)
        + intel_guc_log_section_size_capture(log)
}

// upstream: intel_guc_log.c __get_default_log_level()
unsafe fn __get_default_log_level(log: *mut IntelGucLog) -> u32 {
    let guc = log_to_guc(log);
    let i915 = guc_to_i915(guc);
    // CONFIG_DRM_I915_DEBUG and CONFIG_DRM_I915_DEBUG_GEM are both n.
    let level = (*i915).params.guc_log_level;

    // A negative value means "use platform/config default"
    if level < 0 {
        return GUC_LOG_LEVEL_NON_VERBOSE;
    }

    if level as u32 > GUC_LOG_LEVEL_MAX {
        guc_warn!(
            guc,
            "Log verbosity param out of range: %d > %d!\n",
            level,
            GUC_LOG_LEVEL_MAX
        );
        return GUC_LOG_LEVEL_DISABLED;
    }

    level as u32
}

// upstream: intel_guc_log.c copy_debug_logs_work()
// Reached only through `intel_guc_log_handle_flush_event()` with
// `relay.started`, which requires the debugfs relay channel. Fail closed.
unsafe extern "C" fn copy_debug_logs_work(_work: *mut crate::intel_engine_cs_upstream::WorkStruct) {
    panic!("intel_guc_log: relay flush work requires the debugfs relay channel, which TheKernel does not provide");
}

// upstream: intel_guc_log.c intel_guc_log_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_log_init_early(log: *mut IntelGucLog) {
    let guc = log_to_guc(log);
    mutex_init(ptr::addr_of_mut!((*log).relay.lock));
    mutex_init(ptr::addr_of_mut!((*log).guc_lock));
    INIT_WORK_C(&mut (*log).relay.flush_work, copy_debug_logs_work);
    (*log).relay.started = false;
}

// upstream: intel_guc_log.c intel_guc_log_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_log_create(log: *mut IntelGucLog) -> i32 {
    let guc = log_to_guc(log);

    let guc_log_size = intel_guc_log_size(log);

    let vma = crate::intel_guc_upstream::intel_guc_allocate_vma(guc, guc_log_size);
    if IS_ERR(vma) {
        let ret = PTR_ERR(vma);
        guc_err!(guc, "Failed to allocate or map log buffer %d\n", ret);
        return ret;
    }

    (*log).vma = vma;
    // Create a WC (Uncached for read) vmalloc mapping up front immediate access to
    // data from memory during critical events such as error capture
    let vaddr: *mut c_void =
        crate::i915_gem_pages_upstream::i915_gem_object_pin_map_unlocked((*vma).obj, I915_MAP_WC);
    if IS_ERR(vaddr) {
        let ret = PTR_ERR(vaddr);
        crate::i915_vma_api_upstream::i915_vma_unpin_and_release(
            ptr::addr_of_mut!((*log).vma),
            0,
        );
        guc_err!(guc, "Failed to allocate or map log buffer %d\n", ret);
        return ret;
    }
    (*log).buf_addr = vaddr;

    (*log).level = __get_default_log_level(log);
    guc_dbg!(
        guc,
        "guc_log_level=%d (%s, verbose:%s, verbosity:%d)\n",
        (*log).level,
        if (*log).level == GUC_LOG_LEVEL_DISABLED { "disabled" } else { "enabled" },
        if GUC_LOG_LEVEL_IS_VERBOSE((*log).level) { "yes" } else { "no" },
        GUC_LOG_LEVEL_TO_VERBOSITY((*log).level)
    );

    0
}

// upstream: intel_guc_log.c intel_guc_log_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_log_destroy(log: *mut IntelGucLog) {
    (*log).buf_addr = ptr::null_mut();
    crate::i915_vma_api_upstream::i915_vma_unpin_and_release(
        ptr::addr_of_mut!((*log).vma),
        crate::i915_vma_api_upstream::I915_VMA_RELEASE_MAP,
    );
}

// upstream: intel_guc_log.c intel_guc_log_handle_flush_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_log_handle_flush_event(log: *mut IntelGucLog) {
    if (*log).relay.started {
        queue_work(system_highpri_wq, ptr::addr_of_mut!((*log).relay.flush_work));
    }
}
