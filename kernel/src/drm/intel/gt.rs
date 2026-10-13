// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Boot entry for the GT. The translated i915 driver is the only GT owner:
//! this module defers its probe until the rootfs carries the GuC/HuC firmware
//! (`/lib/firmware/i915/*`), and records what the boot said.

#[cfg(target_os = "none")]
use alloc::string::String;

/// Linux `i915` probes on the first firmware-visible rootfs. A successful
/// deferral is a transient scheduling state; the probe callback records the
/// final verdict, so the queued text is not kept beside it.
#[cfg(target_os = "none")]
pub(super) fn upstream_probe_schedule_text(rootfs_ready: bool) -> &'static str {
    if rootfs_ready {
        "upstream i915: rootfs ready; probe callback dispatched"
    } else {
        "upstream i915: probe queued until the rootfs is mounted"
    }
}

/// Independent boot hook; the default path never resets the GT. `intel.gt=0`
/// keeps the GT untouched.
#[cfg(target_os = "none")]
pub(super) fn init_at_boot() {
    use super::{GT_REPORT, upstream_probe};
    axruntime::boot_progress::trace_enter("gt:init_at_boot");
    axruntime::boot_progress::trace_enter("gt:install_providers");
    let provider_res = super::upstream_gt::install_providers();
    axruntime::boot_progress::trace_exit("gt:install_providers");
    if let Err(error) = provider_res {
        axlog::warn!("intel-gt: upstream providers not installed: {error}");
        axruntime::boot_progress::trace_exit("gt:init_at_boot");
        return;
    }
    if axhal::boot::command_line_value("intel.gt") == Some("0") {
        axruntime::boot_progress::trace_exit("gt:init_at_boot");
        return;
    }
    let windows = super::mapped_windows();
    if windows.is_empty() {
        axruntime::boot_progress::trace_exit("gt:init_at_boot");
        return;
    }
    let result: Result<String, String> = if windows.len() != 1 {
        Err(String::from(
            "intel.gt refused: require one mapped supported Gen12 GPU",
        ))
    } else {
        let (bdf, window) = windows[0];
        axruntime::boot_progress::trace_enter("gt:upstream_probe_defer");
        let res = upstream_probe::defer(bdf, window);
        axruntime::boot_progress::trace_exit("gt:upstream_probe_defer");
        res
    };
    let initialized = result.is_ok();
    let text = result.unwrap_or_else(|s| s);
    axlog::info!("{text}");
    // A successful deferral is only scheduling; the callback records the verdict.
    if !initialized && let Some((bdf, _)) = windows.first() {
        GT_REPORT.lock().push((*bdf, text));
    }
    axruntime::boot_progress::trace_exit("gt:init_at_boot");
}
