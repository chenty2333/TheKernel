// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! The Intel GPU, as the kernel sees it.
//!
//! The i915 port (`tk-intel-gt`, reached through the `upstream_*` modules) is
//! the only owner of the GPU. This module holds what is needed to find the
//! device, read its identity, and hand it to i915:
//!
//! | module | what it owns |
//! |---|---|
//! | [`pci`] | BDF arithmetic, ECAM configuration-space reads, header and BAR decoding |
//! | [`id`] | the device table: device ids, generations, steppings |
//! | [`regs`] | the register window and typed 32-bit access |
//! | [`probe`] | the boot-time PCI walk, the identity decision, and the report |
//! | [`gt_probe`] | the read-only GT forcewake/ack observation written into the report |
//! | [`debugfs`] | the report, exposed as a file in the DRM debug filesystem |
//! | [`initial_plane`] | the firmware framebuffer's plane and stolen pages, read before i915 takes the GGTT |
//! | [`upstream_probe`] | the deferred i915 probe, run once the rootfs has the GuC/HuC firmware |
//! | [`upstream_memory`] | the GGTT reservation of the firmware console and its DMA identity lease |
//! | [`upstream_gt`], [`upstream_drm`], [`upstream_file`], [`upstream_sync`] | the kernel providers i915 calls into |
//! | [`irq`] | MSI for the GPU and the GT interrupt forwarder |
//! | [`power`] | the `POWER_DOMAIN_GT_IRQ` reference rule for a device without a display |
//!
//! There is no KMS modesetting on Intel yet. The screen is the firmware (GOP)
//! framebuffer console, registered by [`crate::drm::linear`]. i915 runs with
//! `HAS_DISPLAY() == false` for everything display-related, as Linux does for a
//! device without display pipes.

pub(crate) mod debugfs;
mod dma;
mod gt;
mod gt_probe;
mod id;
mod initial_plane;
#[cfg(target_os = "none")]
mod irq;
mod pci;
mod power;
mod probe;
mod regs;
pub(crate) mod upstream_file;
pub(crate) mod upstream_sync;
mod upstream_drm;
mod upstream_display;
mod upstream_gt;
#[cfg(target_os = "none")]
mod upstream_probe;
mod upstream_memory;

#[cfg(test)]
mod testbus;
mod upstream_mm;

use alloc::{string::String, vec::Vec};

use spin::Mutex;

use self::{
    probe::{BusFacts, ProbeReport, WindowStatus},
    regs::{RegisterWindow},
};
#[cfg(target_os = "none")]
use self::regs::PROBE_WINDOW;
#[cfg(target_os = "none")]
use self::id::Aperture;

/// Why the Intel path refused a step. The reason is logged where it happens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Refused,
}

/// The report of the one probe this kernel runs, kept for the debug file.
///
/// The probe runs once, at boot, and its findings do not change afterwards:
/// the device is either there or it is not, and re-reading registers on every
/// open of a debug file would make a diagnostic tool a source of hardware
/// traffic.
static REPORT: Mutex<Option<ProbeReport>> = Mutex::new(None);

/// What each GPU's GT bring-up said, kept for the debug file.
static GT_REPORT: Mutex<Vec<(pci::Bdf, String)>> = Mutex::new(Vec::new());

/// A value as grouped hexadecimal, the way a register dump is written down.
///
/// `0x0000_6000_0000_0000` can be read a field at a time; `0x600000000000`
/// cannot.  Rust's formatter has no digit-grouping flag, so the grouping is
/// done here rather than left out of every log line.
pub(crate) fn hex(value: u64, digits: usize) -> String {
    use alloc::format;

    let digits = digits.clamp(1, 16);
    let raw = format!("{value:0>digits$x}");
    let mut out = String::with_capacity(2 + raw.len() + raw.len() / 4);
    out.push_str("0x");
    for (index, digit) in raw.char_indices() {
        if index > 0 && (raw.len() - index) % 4 == 0 {
            out.push('_');
        }
        out.push(digit);
    }
    out
}

/// Run the Intel display probe once and print what it found.
///
/// The report goes to the log as it is produced, and [`debugfs`] serves the
/// same text afterwards.
///
pub(crate) fn probe_at_boot() {
    axruntime::boot_progress::trace_enter("drm:probe_at_boot");
    axruntime::boot_progress::trace_enter("drm:platform_probe");
    let report = platform_probe();
    axruntime::boot_progress::trace_exit("drm:platform_probe");
    report.log();
    *REPORT.lock() = Some(report);
    axruntime::boot_progress::trace_enter("drm:gt_probe_report");
    let observations: Vec<_> = mapped_windows()
        .into_iter()
        .map(|(bdf, window)| {
            let text = gt_probe::report(window);
            axlog::info!("{bdf}: {text}");
            (bdf, text)
        })
        .collect();
    *GT_REPORT.lock() = observations;
    axruntime::boot_progress::trace_exit("drm:gt_probe_report");
    axruntime::boot_progress::trace_exit("drm:probe_at_boot");
}

/// The devices whose register window the probe mapped, with those windows.
///
/// A device the probe found but could not map is skipped here rather than
/// guessed at, which is the policy the probe itself applies to a part it has no
/// model for.
fn mapped_windows() -> alloc::vec::Vec<(pci::Bdf, RegisterWindow)> {
    let report = REPORT.lock();
    let Some(report) = report.as_ref() else {
        return alloc::vec::Vec::new();
    };
    report
        .displays
        .iter()
        .filter_map(|found| match found.status {
            WindowStatus::Mapped { window, .. } => Some((found.info.bdf, window)),
            WindowStatus::Refused(_) => None,
        })
        .collect()
}

/// The boot report as text: the probe, then each GPU's GT bring-up.
pub(crate) fn report_text() -> String {
    let mut text = match &*REPORT.lock() {
        Some(report) => report.render(),
        None => String::from(
            "intel-gpu: the Intel display probe has not run in this boot; it runs from the DRM \
             initialization path, so this file is empty only if that path never executed\n",
        ),
    };
    for (bdf, report) in GT_REPORT.lock().iter() {
        text.push_str(&alloc::format!("{bdf}: {report}"));
    }
    text
}

/// The refusal a step returns, with its call site logged when boot tracing is on.
#[track_caller]
pub(super) fn traced_refusal() -> Error {
    #[cfg(target_os = "none")]
    if axhal::boot::command_line_value("boot.progress") == Some("1") {
        let at = core::panic::Location::caller();
        axlog::warn!("intel: refusal at {}:{}", at.file(), at.line());
    }
    Error::Refused
}

/// Bring up the GT path on the device the probe mapped: i915 is deferred to the
/// rootfs-ready callback, which fetches its GuC/HuC firmware.
pub(crate) fn bring_up_gt_at_boot() {
    axruntime::boot_progress::trace_enter("drm:bring_up_gt_at_boot");
    #[cfg(target_os = "none")]
    gt::init_at_boot();
    axruntime::boot_progress::trace_exit("drm:bring_up_gt_at_boot");
}

/// Whether the translated i915 driver has registered its DRM device.
pub(super) fn gt_registered() -> bool {
    upstream_drm::registered_device().is_some()
}

/// PCI identity of the live GT owner (the render node's device).
pub(crate) fn gt_pci_identity() -> Option<axdriver_display::DisplayPciIdentity> {
    if !gt_registered() {
        return None;
    }
    let bdf = upstream_gt::registered_bdf()?;
    let report = REPORT.lock();
    let info = report
        .as_ref()?
        .displays
        .iter()
        .find(|display| display.info.bdf == bdf)?
        .info;
    Some(axdriver_display::DisplayPciIdentity {
        bus: bdf.bus,
        device: bdf.device,
        function: bdf.function,
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        subsystem_vendor: info.subsystem_vendor_id,
        subsystem_device: info.subsystem_id,
        revision: info.revision,
    })
}

/// Run the probe against the platform's PCI bus.
#[cfg(target_os = "none")]
fn platform_probe() -> ProbeReport {
    let Some(ecam) = pci::Ecam::platform() else {
        return ProbeReport::unavailable(
            "this build declares no ECAM aperture, so PCI configuration space cannot be reached",
            BusFacts {
                ecam_base: None,
                bus_end: 0,
            },
        );
    };
    let facts = BusFacts {
        ecam_base: Some(ecam.base()),
        bus_end: ecam.bus_end(),
    };
    probe::run(&ecam, facts, open_register_window)
}

/// The probe cannot run in a hosted test build, and says so rather than
/// reporting an empty bus, which would be indistinguishable from a machine
/// with no GPU.
#[cfg(not(target_os = "none"))]
fn platform_probe() -> ProbeReport {
    ProbeReport::unavailable(
        "hosted test build: PCI configuration space and device memory are not reachable here, so \
         the probe did not run and this report describes nothing on this machine",
        BusFacts {
            ecam_base: None,
            bus_end: 0,
        },
    )
}

#[cfg(target_os = "none")]
fn open_register_window(info: &pci::DeviceInfo, aperture: &'static Aperture) -> WindowStatus {
    let Some(bar) = info.memory_bar(aperture.bar) else {
        return WindowStatus::Refused(
            "the device declares no memory BAR where the device table puts its registers",
        );
    };
    let Some(address) = bar.address() else {
        return WindowStatus::Refused("the register BAR decodes no address");
    };
    if !aperture.admits_base(address) {
        return WindowStatus::Refused(
            "the register BAR address contradicts the aperture size this kernel models, so it \
             will not map on top of it",
        );
    }
    match RegisterWindow::map(address, PROBE_WINDOW) {
        Ok(window) => WindowStatus::Mapped {
            window,
            aperture,
            physical: address,
        },
        Err(_) => WindowStatus::Refused("mapping the register aperture failed"),
    }
}

