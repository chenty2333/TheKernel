// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! N305 GT/media A0 is independent of display D0. No implicit hardware access,
//! firmware load or userspace command stream. Kernel intel.gt=1 owns invocation.
#![no_std]
#![deny(unsafe_code)]
extern crate alloc;
#[cfg(test)]
extern crate std;
#[cfg(feature = "upstream-gt")]
#[path = "linux/config.rs"]
pub mod linux_config;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/macros.rs"]
mod linux_macros;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/heap.rs"]
pub(crate) mod linux_heap;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/list.rs"]
pub(crate) mod linux_list;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/memory.rs"]
pub(crate) mod linux_memory;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/locks.rs"]
pub(crate) mod linux_locks;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/xarray.rs"]
pub(crate) mod linux_xarray;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/wait.rs"]
pub(crate) mod linux_wait;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/tasklet.rs"]
pub(crate) mod linux_tasklet;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/pm.rs"]
pub(crate) mod linux_pm;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/i915_private.rs"]
pub(crate) mod linux_i915_private;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/mutex.rs"]
pub(crate) mod linux_mutex;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/sw_fence.rs"]
pub(crate) mod linux_sw_fence;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/timer.rs"]
pub(crate) mod linux_timer;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[path = "linux/workqueue.rs"]
pub(crate) mod linux_workqueue;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/assert.rs"]
mod linux_assert;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/print.rs"]
mod linux_print;
pub mod bcs;
pub mod cache;
pub mod execlists;
pub mod guc_ads;
pub mod guc_capture;
pub mod guc_config;
pub mod guc_ct;
pub mod guc_fw;
pub mod guc_log;
pub mod guc_submission;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod gen8_engine_cs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod guc_submission_upstream;
pub mod huc;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_active_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_drm_client_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_context_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_context_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_core_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_mman_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_domain_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_lmem_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_object_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_object_header_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_object_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_object_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_region_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_pages_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_shmem_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_shrinker_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_tiling_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_userptr_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_gem_ww_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_request_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_request_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_scheduler_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_scheduler_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_sw_fence_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_syncmap_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_vma_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_vma_resource_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_vma_types_upstream;
pub mod info;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_breadcrumbs_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_breadcrumbs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_context_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_context_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_context_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_cs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_heartbeat_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_requests_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_regs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_user_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_execlists_submission_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_ggtt_fencing_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_ggtt_fencing_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gsc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gsc_uc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_buffer_pool_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_buffer_pool_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_defines_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod i915_freq_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_mcr_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gt_clock_utils_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_gtt_api_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_actions_abi_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_ct_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_fwif_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_log_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_rc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_slpc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_slpc_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_submission_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_guc_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_huc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_hwconfig_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_llc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_llc_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_lrc_reg_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_lrc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_lrc_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_runtime_pm_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_reset_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_migrate_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_migrate_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_rc6_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_reset_types_upstream;
pub mod intel_ring;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_ring_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_ring_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_rps_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_rps_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_sseu_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_sseu_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_timeline_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_timeline_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_uc_fw_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_uc_fw_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_uc_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_uncore_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_wakeref_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_wopcm_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_workarounds_types_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_workarounds_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod linux;
pub mod lrc;
pub mod ppgtt;
pub mod rcs;
pub mod rcs_page;
pub mod reset;
pub mod uc;
pub mod uncore;
pub mod wopcm;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unavailable(u32),
    Timeout(u32),
    Refused,
    Quarantined,
}
/// One exclusive GT owner. MMIO preserves ordering; an error may have landed.
/// Timing is monotonic, delays bounded. No default implementation or raw uAPI.
pub trait GtIo {
    fn read(&self, offset: u32) -> Result<u32, Error>;
    fn write(&self, offset: u32, value: u32) -> Result<(), Error>;
    fn now_us(&self) -> u64;
    fn delay_us(&self, micros: u32);
}
pub fn masked_enable(bits: u32) -> u32 {
    (bits << 16) | bits
}
pub fn masked_disable(bits: u32) -> u32 {
    bits << 16
}
pub(crate) fn wait(
    io: &impl GtIo,
    reg: u32,
    mask: u32,
    value: u32,
    micros: u64,
) -> Result<u32, Error> {
    let start = io.now_us();
    for _ in 0..1_000_000 {
        let raw = io.read(reg)?;
        if raw == u32::MAX {
            return Err(Error::Unavailable(reg));
        }
        if raw & mask == value {
            return Ok(raw);
        }
        if io.now_us().saturating_sub(start) >= micros {
            return Err(Error::Timeout(reg));
        }
        io.delay_us(1);
    }
    Err(Error::Timeout(reg))
}

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_mm_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_gem_gtt_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_utils_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod intel_memory_region_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_gem_phys_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod intel_engine_pm_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_gem_clflush_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod intel_reset_hw_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod intel_guc_capture_upstream;

#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_gpu_error_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub(crate) mod i915_cmd_parser_upstream;
