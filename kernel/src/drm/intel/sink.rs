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
//! the power workstream needs.  [`super::connect`] is the composition that puts
//! step 1 in front of this module: it requests the well for each candidate pin
//! and then calls [`probe_one`] here, so the boot path runs the reference's
//! order with the power register written by the module that owns it.
//!
//! The result is a fact the firmware did not give this kernel: what monitor is
//! attached, on which pin, and what timing the mode layer would program for it.
//! It goes to the kernel log *and* into the probe's debug file, because the
//! target machine has no serial port: a boot log scrolls away, and an EDID is
//! worth reading twice.
//!
//! Nothing here has run on the target hardware.

use alloc::{format, string::String, vec::Vec};

use axlog::warn;

use super::{
    gmbus::{self, EdidBytes, Pin, PollTimer, SinkProbe},
    hpd::{self, Ddi, HpdError, HpdStatus},
    pci::Bdf,
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

impl DeviceSink {
    /// What phase 2 found on this device, as the lines a person reads off the
    /// screen or out of `/sys/kernel/debug/dri/0/intel_gpu`.
    ///
    /// The rendering lives on the value rather than on a report type that
    /// aggregates devices: the aggregation is the connector step's
    /// (`connect::ConnectReport`), and what is left here is one display's own
    /// facts.  Every line is prefixed, so a reader who pipes the file into a log
    /// can grep one word to find all of it, exactly as the probe's report does.
    pub(crate) fn render(&self) -> String {
        let mut out = format!("display {}:\n", self.bdf);
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
        out
    }
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
