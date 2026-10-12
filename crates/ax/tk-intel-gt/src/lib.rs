// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! N305 GT/media A0 is independent of display D0. No implicit hardware access,
//! firmware load or userspace command stream. Kernel intel.gt=1 owns invocation.
#![no_std]
#![feature(c_variadic)]
#![deny(unsafe_code)]
// The i915 translation modules are line-by-line translations of C: their unsafe
// blocks mirror the C statements and carry `// upstream:` provenance rather
// than per-block SAFETY prose, and their own `#![allow(unsafe_code)]` repeats
// the module declaration.
#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::duplicated_attributes,
    // C pointer-taking functions, `goto`-as-loop, GEM_BUG_ON range checks
    // and `(0 << n)` field constants are kept as written upstream.
    clippy::not_unsafe_ptr_arg_deref,
    clippy::macro_metavars_in_unsafe,
    clippy::never_loop,
    clippy::absurd_extreme_comparisons,
    clippy::swap_ptr_to_ref,
    clippy::erasing_op,
    clippy::empty_line_after_doc_comments
)]
extern crate alloc;
#[cfg(test)]
extern crate std;
#[path = "linux/config.rs"]
pub mod linux_config;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/macros.rs"]
mod linux_macros;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/heap.rs"]
pub(crate) mod linux_heap;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/list.rs"]
pub(crate) mod linux_list;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/memory.rs"]
pub(crate) mod linux_memory;
#[allow(unsafe_code)]
#[path = "linux/locks.rs"]
pub(crate) mod linux_locks;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/xarray.rs"]
pub(crate) mod linux_xarray;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/wait.rs"]
pub(crate) mod linux_wait;
#[allow(unsafe_code)]
#[path = "linux/tasklet.rs"]
pub(crate) mod linux_tasklet;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/pm.rs"]
pub mod linux_pm;
#[allow(unsafe_code)]
#[path = "linux/i915_private.rs"]
pub mod linux_i915_private;
#[allow(unsafe_code)]
#[path = "linux/mutex.rs"]
pub(crate) mod linux_mutex;
#[allow(unsafe_code)]
#[path = "linux/sw_fence.rs"]
pub(crate) mod linux_sw_fence;
#[allow(unsafe_code)]
#[path = "linux/timer.rs"]
pub(crate) mod linux_timer;
#[allow(unsafe_code)]
#[path = "linux/workqueue.rs"]
pub(crate) mod linux_workqueue;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/assert.rs"]
mod linux_assert;
#[allow(unsafe_code)]
#[macro_use]
#[path = "linux/print.rs"]
mod linux_print;
#[allow(unsafe_code)]
#[path = "linux/platform.rs"]
pub mod linux_platform;
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
#[allow(unsafe_code)]
pub mod gen8_engine_cs_upstream;
#[allow(unsafe_code)]
pub mod guc_submission_upstream;
pub mod huc;
#[allow(unsafe_code)]
pub mod i915_active_upstream;
#[allow(unsafe_code)]
pub mod i915_drm_client_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_context_types_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_context_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_core_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_execbuffer_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_busy_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_internal_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_wait_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_throttle_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_create_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_pm_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_dmabuf_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_object_frontbuffer_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_getparam_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_stolen_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_query_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gem_evict_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_vma_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_guc_rc_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_mman_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_domain_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_lmem_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_object_api_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_object_header_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_object_types_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_object_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_region_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_pages_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_shmem_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_shrinker_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_tiling_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_userptr_upstream;
#[allow(unsafe_code)]
pub mod i915_gem_ww_upstream;
#[allow(unsafe_code)]
pub mod i915_request_types_upstream;
#[allow(unsafe_code)]
pub mod i915_request_upstream;
#[allow(unsafe_code)]
pub mod i915_scheduler_types_upstream;
#[allow(unsafe_code)]
pub mod i915_scheduler_upstream;
#[allow(unsafe_code)]
pub mod i915_sw_fence_upstream;
#[allow(unsafe_code)]
pub mod i915_syncmap_upstream;
#[allow(unsafe_code)]
pub mod i915_vma_api_upstream;
#[allow(unsafe_code)]
pub mod i915_vma_resource_types_upstream;
#[allow(unsafe_code)]
pub mod i915_vma_types_upstream;
pub mod info;
#[allow(unsafe_code)]
pub mod intel_breadcrumbs_types_upstream;
#[allow(unsafe_code)]
pub mod intel_breadcrumbs_upstream;
#[allow(unsafe_code)]
pub mod intel_context_api_upstream;
#[allow(unsafe_code)]
pub mod intel_context_types_upstream;
#[allow(unsafe_code)]
pub mod intel_context_sseu_upstream;
#[allow(unsafe_code)]
pub mod intel_context_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_api_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_cs_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_heartbeat_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_requests_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_regs_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_types_upstream;
#[allow(unsafe_code)]
pub mod intel_engine_user_upstream;
#[allow(unsafe_code)]
pub mod intel_execlists_submission_upstream;
#[allow(unsafe_code)]
pub mod intel_ggtt_fencing_types_upstream;
#[allow(unsafe_code)]
pub mod intel_ggtt_fencing_upstream;
#[allow(unsafe_code)]
pub mod intel_gsc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_gsc_uc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_api_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_buffer_pool_types_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_buffer_pool_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_defines_types_upstream;
#[allow(unsafe_code)]
pub mod i915_freq_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_mcr_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_mcr_impl_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_pm_irq_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_irq_upstream;

#[allow(unsafe_code)]
pub mod i915_irq_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_pm_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_types_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_clock_utils_upstream;
#[allow(unsafe_code)]
pub mod intel_gtt_api_upstream;
#[allow(unsafe_code)]
pub mod intel_gt_upstream;
#[allow(unsafe_code)]
pub(crate) mod gen8_ppgtt_upstream;

#[allow(unsafe_code)]
pub(crate) mod shmem_utils_upstream;

#[allow(unsafe_code)]
pub mod intel_ppgtt_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_actions_abi_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_ct_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_ct_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_fwif_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_log_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_rc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_slpc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_slpc_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_submission_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_huc_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_mocs_upstream;

#[allow(unsafe_code)]
pub mod intel_huc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_huc_fw_upstream;
#[allow(unsafe_code)]
pub mod intel_hwconfig_types_upstream;
#[allow(unsafe_code)]
pub mod intel_guc_hwconfig_upstream;
#[allow(unsafe_code)]
pub mod intel_llc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_llc_upstream;
#[allow(unsafe_code)]
pub mod intel_lrc_reg_types_upstream;
#[allow(unsafe_code)]
pub mod intel_lrc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_lrc_upstream;
#[allow(unsafe_code)]
pub mod intel_runtime_pm_upstream;
#[allow(unsafe_code)]
pub mod intel_reset_upstream;
#[allow(unsafe_code)]
pub mod intel_migrate_types_upstream;
#[allow(unsafe_code)]
pub mod intel_migrate_upstream;
#[allow(unsafe_code)]
pub mod intel_rc6_types_upstream;
#[allow(unsafe_code)]
pub mod intel_rc6_upstream;
#[allow(unsafe_code)]
pub mod intel_renderstate_upstream;
#[allow(unsafe_code)]
pub mod intel_reset_types_upstream;
pub mod intel_ring;
#[allow(unsafe_code)]
pub mod intel_ring_types_upstream;
#[allow(unsafe_code)]
pub mod intel_ring_upstream;
#[allow(unsafe_code)]
pub mod intel_rps_types_upstream;
#[allow(unsafe_code)]
pub mod intel_rps_upstream;
#[allow(unsafe_code)]
pub mod intel_sseu_types_upstream;
#[allow(unsafe_code)]
pub mod intel_sseu_upstream;
#[allow(unsafe_code)]
pub mod intel_timeline_types_upstream;
#[allow(unsafe_code)]
pub mod intel_tlb_upstream;
#[allow(unsafe_code)]
pub mod intel_timeline_upstream;
#[allow(unsafe_code)]
pub mod intel_uc_fw_types_upstream;
#[allow(unsafe_code)]
pub mod intel_uc_fw_upstream;
#[allow(unsafe_code)]
pub mod intel_uc_types_upstream;
#[allow(unsafe_code)]
pub mod intel_uc_upstream;
#[allow(unsafe_code)]
pub mod intel_uncore_types_upstream;
#[allow(unsafe_code)]
pub mod intel_wakeref_types_upstream;
#[allow(unsafe_code)]
pub mod intel_wopcm_types_upstream;
#[allow(unsafe_code)]
pub mod intel_wopcm_upstream;
#[allow(unsafe_code)]
pub mod intel_workarounds_types_upstream;
#[allow(unsafe_code)]
pub mod intel_workarounds_upstream;
#[allow(unsafe_code)]
pub mod linux;
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

#[allow(unsafe_code)]
pub(crate) mod i915_mm_upstream;

#[allow(unsafe_code)]
pub(crate) mod i915_gem_gtt_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_utils_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_memory_region_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_region_ttm_upstream;

#[allow(unsafe_code)]
pub(crate) mod i915_gem_phys_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_engine_pm_upstream;

#[allow(unsafe_code)]
pub(crate) mod i915_gem_clflush_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_reset_hw_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_guc_ads_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_ggtt_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_gtt_upstream;

#[allow(unsafe_code)]
pub(crate) mod intel_guc_capture_upstream;

#[allow(unsafe_code)]
pub(crate) mod i915_gpu_error_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_cmd_parser_upstream;
#[allow(unsafe_code)]
pub mod intel_uncore_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_pcode_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_wakeref_upstream;
#[allow(unsafe_code)]
pub(crate) mod vlv_iosf_sb_upstream;

#[allow(unsafe_code)]
pub(crate) mod gt_header_inline_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_guc_log_upstream;

#[allow(unsafe_code)]
pub mod i915_driver_upstream;
#[allow(unsafe_code)]
pub mod i915_pci_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_device_info_types_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_device_info_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_pciids_upstream;
#[allow(unsafe_code)]
pub mod i915_probe_provider_upstream;

#[allow(unsafe_code)]
pub mod i915_ioctl_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_memcpy_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_vgpu_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_clock_gating_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_vma_resource_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_guc_fw_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_unsupported_hw_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_edram_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_params_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_step_upstream;
#[allow(unsafe_code)]
pub(crate) mod vlv_suspend_upstream;
#[allow(unsafe_code)]
pub(crate) mod i915_gmch_upstream;
#[allow(unsafe_code)]
pub(crate) mod intel_renderstate_data_upstream;
