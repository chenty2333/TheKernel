// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Boot entry for the translated i915 driver.
//!
//! The translated `i915_pci_probe()` is the only GT owner: `gt::init_at_boot()`
//! defers [`probe`] to the rootfs-ready callback.

use alloc::{format, string::String};
use core::ffi::c_void;

use intel_gt::i915_pci_upstream::{i915_pci_probe, pci_match_id};

use super::{pci, regs::RegisterWindow, upstream_gt};

unsafe extern "C" {
    fn pci_get_drvdata(pdev: *mut c_void) -> *mut c_void;
}

/// Linux `struct resource` (include/linux/ioport.h), the layout of
/// `tk-intel-gt`'s `linux::gem_memory::Resource`.
#[repr(C)]
pub struct Resource {
    start: u64,
    end: u64,
    name: *const core::ffi::c_char,
    flags: core::ffi::c_ulong,
    desc: core::ffi::c_ulong,
    parent: *mut Resource,
    sibling: *mut Resource,
    child: *mut Resource,
}

const IORESOURCE_MEM: core::ffi::c_ulong = 0x0000_0200;

/// x86 `intel_graphics_stolen_res`: Linux fills it from the BDSM/GMS early
/// quirk before any driver runs; here the probe fills it from the same two
/// registers (see `initial_plane::stolen_range`) before `i915_pci_probe()`. An
/// empty resource (start 0, end 0) is what Linux leaves when no stolen memory
/// was found, and i915 then runs without stolen memory.
#[unsafe(no_mangle)]
pub static mut intel_graphics_stolen_res: Resource = Resource {
    start: 0,
    end: 0,
    name: c"Graphics Stolen Memory".as_ptr(),
    flags: IORESOURCE_MEM,
    desc: 0,
    parent: core::ptr::null_mut(),
    sibling: core::ptr::null_mut(),
    child: core::ptr::null_mut(),
};

/// x86 `tsc_khz`. TheKernel's monotonic clock on x86_64 counts TSC ticks, so
/// the tick count of one millisecond is the TSC frequency in kHz.
#[unsafe(no_mangle)]
pub static mut tsc_khz: u32 = 0;

static PENDING: spin::Mutex<Option<(pci::Bdf, RegisterWindow)>> = spin::Mutex::new(None);

/// Queue the upstream probe for the rootfs-ready callback: `intel_uc_fw.c`
/// requests `/lib/firmware/i915/...` synchronously during the probe.
pub(super) fn defer(bdf: pci::Bdf, window: RegisterWindow) -> Result<String, String> {
    *PENDING.lock() = Some((bdf, window));
    if !axdriver::prelude::firmware::on_rootfs_ready(run_pending) {
        PENDING.lock().take();
        return Err(String::from("upstream i915: rootfs callback table is full; GT left untouched"));
    }
    let status = super::gt::upstream_probe_schedule_text(
        axdriver::prelude::firmware::rootfs_ready(),
    );
    Ok(String::from(status))
}

fn run_pending() {
    axruntime::boot_progress::trace_enter("gt:run_pending");
    let Some((bdf, window)) = PENDING.lock().take() else {
        axruntime::boot_progress::trace_exit("gt:run_pending");
        return;
    };
    let text = probe(bdf, window).unwrap_or_else(|e| e);
    axlog::info!("{text}");
    super::GT_REPORT.lock().push((bdf, text));
    axruntime::boot_progress::trace_exit("gt:run_pending");
}

/// Run the upstream PCI probe for one mapped Intel GPU and publish the
/// resulting device to the kernel IRQ and display-power owners.
pub(super) fn probe(bdf: pci::Bdf, window: RegisterWindow) -> Result<String, String> {
    axruntime::boot_progress::trace_enter("gt:probe");
    let ecam = pci::Ecam::platform().ok_or_else(|| {
        axruntime::boot_progress::trace_exit("gt:probe");
        String::from("upstream i915: PCI facts unavailable")
    })?;
    let info = pci::DeviceInfo::read(&ecam, bdf).ok_or_else(|| {
        axruntime::boot_progress::trace_exit("gt:probe");
        String::from("upstream i915: PCI device unavailable")
    })?;
    if info.vendor_id != 0x8086 {
        axruntime::boot_progress::trace_exit("gt:probe");
        return Err(format!("upstream i915: vendor {:04x} is not Intel", info.vendor_id));
    }
    let device_info = pci_match_id(info.device_id).ok_or_else(|| {
        axruntime::boot_progress::trace_exit("gt:probe");
        format!("upstream i915: device {:04x} not in pciidlist", info.device_id)
    })?;
    // SAFETY: boot-time single-threaded writes to the x86 data symbols before any reader (the probe) runs.
    unsafe {
        tsc_khz = u32::try_from(axhal::time::nanos_to_ticks(1_000_000)).unwrap_or(u32::MAX);
        if let Some(range) = super::initial_plane::stolen_range(&ecam, &window, bdf) {
            intel_graphics_stolen_res.start = range.start;
            intel_graphics_stolen_res.end = range.end - 1;
        }
    }
    upstream_gt::register_gpu_window(window);
    let pdev = upstream_gt::native_pci_device(bdf);
    if let Err(err) = super::upstream_memory::prepare(bdf, &window, pdev) {
        axruntime::boot_progress::trace_exit("gt:probe");
        return Err(err);
    }
    if let Err(e) = super::irq::upstream::install(bdf) {
        axruntime::boot_progress::trace_exit("gt:probe");
        return Err(format!("upstream i915: {e}"));
    }
    axruntime::boot_progress::trace_enter("gt:i915_pci_probe");
    // SAFETY: `pdev` is the native PCI identity for `bdf`; `device_info` matches its device id.
    let ret = unsafe { i915_pci_probe(pdev, info.device_id, device_info) };
    axruntime::boot_progress::trace_exit("gt:i915_pci_probe");
    if ret != 0 {
        super::irq::upstream::disable_msi(bdf);
        axruntime::boot_progress::trace_exit("gt:probe");
        return Err(format!("upstream i915: i915_pci_probe({:04x}) failed: {ret}", info.device_id));
    }
    // SAFETY: a successful probe stored the i915 device as the drvdata of `pdev`.
    let i915 = unsafe { pci_get_drvdata(pdev) };
    if let Err(e) = upstream_gt::register_upstream_i915(i915) {
        axruntime::boot_progress::trace_exit("gt:probe");
        return Err(format!("upstream i915: {e}"));
    }
    axruntime::boot_progress::trace_exit("gt:probe");
    Ok(format!("upstream i915: probed {:04x} rev {:02x}", info.device_id, info.revision))
}
