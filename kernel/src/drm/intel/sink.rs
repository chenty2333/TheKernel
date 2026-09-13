//! Phase 2 of the bring-up order: the sink.
//!
//! Reference §11 phase 2 is three steps:
//!
//! 1. Enable the AUX/DDC power well for the port, "needed for GMBUS/DCC on that
//!    pin pair", whose symptom when skipped is "*GMBUS returns NAK on every
//!    address, always*" (§11 phase 2.1).
//! 2. Enable hotplug, then immediately read the status once (§11 phase 2.2).
//! 3. Read the EDID over GMBUS, 128 bytes from slave `0x50` register `0x00`,
//!    checking the header and the checksum before believing a byte of it
//!    (§11 phase 2.3).
//!
//! This module is the boot-time form of steps 2 and 3.  **Step 1 is
//! deliberately not here**: the AUX/DDC power well is the power workstream's
//! register to write, and this workstream's answer to a well that is off is to
//! *name* it — [`GmbusError::AuxWellDown`] carries the well and its state bit —
//! rather than to enable it behind that workstream's back.  On a machine where
//! the well is off, the boot log says so in those words, which is the finding
//! the power workstream needs.
//!
//! The result is a fact the firmware did not give this kernel: what monitor is
//! attached, on which pin, and what timing the mode layer would program for it.
//! It goes to the kernel log *and* into the probe's debug file, because the
//! target machine has no serial port: a boot log scrolls away, and an EDID is
//! worth reading twice.
//!
//! Nothing here has run on the target hardware.

use alloc::{format, string::String, vec::Vec};

use axlog::{info, warn};

use super::{
    gmbus::{self, EdidBytes, MonotonicTimer, Pin, PollTimer, SinkProbe},
    hpd::{self, Ddi, HpdError, HpdStatus},
    pci::Bdf,
    probe::{ProbeReport, WindowStatus},
    regs::Registers,
};
use crate::drm::modes::{self, Constraints, ModePlan};

/// What phase 2 found on one display device.
pub(crate) struct DeviceSink {
    pub(crate) bdf: Bdf,
    /// Every DDC pin that was asked, and what it answered.
    pub(crate) pins: SinkProbe,
    /// The pin a monitor answered on.
    pub(crate) monitor: Option<Pin>,
    /// The validated EDID base block, when there was one.
    pub(crate) edid: Option<EdidBytes>,
    /// The first extension block, when the sink declared one and it read.
    pub(crate) extension: Option<EdidBytes>,
    /// What the mode layer made of the bytes.  `None` only when no monitor
    /// answered at all, so a plan is never a plan made from nothing.
    pub(crate) plan: Option<ModePlan>,
    /// Hotplug status per DDI, and the DDIs whose status could not be read.
    pub(crate) hotplug: Vec<HpdStatus>,
    pub(crate) hotplug_errors: Vec<(Ddi, HpdError)>,
}

/// What phase 2 found, for the log and for the debug file.
#[derive(Default)]
pub(crate) struct SinkReport {
    pub(crate) devices: Vec<DeviceSink>,
}

impl DeviceSink {
    /// This device's lines, as the boot report writes them.
    ///
    /// Split out of [`SinkReport::render`] because a phase-2 probe is no longer
    /// only a boot step: the after-boot hotplug watch re-runs
    /// [`probe_one`] for the device whose connect state changed, and the result
    /// has to read the same way in the debug file as the boot result did.
    /// Rendering it in one place is what keeps the two comparable.
    pub(crate) fn render_into(&self, out: &mut String) {
        out.push_str(&format!("display {}:\n", self.bdf));
        out.push_str(&self.pins.render());
        if let Some(extension) = &self.extension {
            out.push_str(&format!("  extension block: {}\n", extension.describe()));
        }
        if let Some(plan) = &self.plan {
            out.push_str(&format!(
                "  mode layer chose {} ({:?}{})\n",
                plan.selection.mode,
                plan.selection.reason,
                if plan.strict { "" } else { ", lenient parse" }
            ));
        }
        for status in &self.hotplug {
            out.push_str(&format!("  {}\n", status.describe()));
        }
        for (ddi, error) in &self.hotplug_errors {
            out.push_str(&format!("  {ddi}: {}\n", error.describe()));
        }
    }
}

impl SinkReport {
    /// The same text the log carries, for `/sys/kernel/debug/dri/0/intel_gpu`.
    ///
    /// Every line is prefixed, so a reader who pipes the file into a log can
    /// grep one word to find all of it, exactly as the probe's own report does.
    pub(crate) fn render(&self) -> String {
        if self.devices.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n--- the sink (reference section 11 phase 2) ---\n");
        for device in &self.devices {
            device.render_into(&mut out);
        }
        out
    }
}

/// Ask every display device the probe mapped what is attached to it.
///
/// The devices are the ones whose register window the probe mapped and
/// identified; a device with no window is skipped rather than guessed at, which
/// is the same policy the probe applies to a part it has no model for.  One
/// device's failure never stops the next.
pub(crate) fn probe_at_boot(report: &ProbeReport) -> SinkReport {
    let mut devices = Vec::new();
    for found in &report.displays {
        let WindowStatus::Mapped { window, .. } = found.status else {
            continue;
        };
        info!(
            "intel-sink: looking for a monitor on {} (reference section 11 phase 2)",
            found.info.bdf
        );
        devices.push(probe_one(found.info.bdf, &window, &MonotonicTimer));
    }
    SinkReport { devices }
}

/// Phase 2 against one register file.
///
/// This is the seam the tests drive: the boot path passes the mapped aperture
/// and the machine's clock, and a host test passes a model of the controller
/// and a clock it owns, so the whole chain -- pins, EDID validation, the mode
/// layer, hotplug -- runs on a machine that has no display hardware.
pub(crate) fn probe_one<R: Registers, T: PollTimer>(bdf: Bdf, regs: &R, timer: &T) -> DeviceSink {
    let pins = gmbus::probe_sink_with(regs, timer);
    pins.log();
    let monitor = pins.found();
    let edid = pins.edid();
    // The extension is only asked for when the base block declared one, so a
    // sink without one costs no extra transaction.
    let extension = match monitor {
        Some(pin) => match gmbus::read_edid_extension_with(regs, timer, pin) {
            Ok(extension) => extension,
            Err(error) => {
                warn!("intel-sink: {}", error.describe());
                None
            }
        },
        None => None,
    };
    // The mode layer gets the base block and the extension the sink declared,
    // concatenated, because its strict parse requires every block the base
    // block declares.  `plan_modeset` never fails: a sink that advertises
    // nothing usable gets the built-in fallback and says so, and it logs its
    // own decision line.
    let plan = edid.map(|base| {
        let mut bytes = Vec::with_capacity(2 * gmbus::EDID_BLOCK_LEN);
        bytes.extend_from_slice(base.as_slice());
        if let Some(extension) = &extension {
            bytes.extend_from_slice(extension.as_slice());
        }
        modes::plan_modeset(&bytes, &Constraints::unlimited())
    });
    let mut hotplug = Vec::new();
    let mut hotplug_errors = Vec::new();
    for ddi in Ddi::ALL {
        match hpd::enable_and_read(regs, ddi) {
            Ok(status) => {
                status.log();
                hotplug.push(status);
            }
            Err(error) => {
                warn!("intel-sink: {}", error.describe());
                hotplug_errors.push((ddi, error));
            }
        }
    }
    DeviceSink {
        bdf,
        pins,
        monitor,
        edid,
        extension,
        plan,
        hotplug,
        hotplug_errors,
    }
}

#[cfg(test)]
mod tests;
