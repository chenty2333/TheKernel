//! The Intel display driver's foundation: find the device, identify it, read
//! its registers, and say what happened.
//!
//! This is the first Intel-specific code in the kernel.  It exists to answer
//! one question honestly -- *is this kernel talking to a real Intel GPU?* --
//! before anything is built on top of the answer.  It does that without
//! programming the device at all: it reads PCI configuration space, matches
//! what it finds against a table of known parts, maps the register aperture
//! read-only in effect, and reads the handful of registers that are safe to
//! read on a part whose display engine the firmware is still driving.
//!
//! ## Layout
//!
//! | module | what it owns |
//! |---|---|
//! | [`pci`] | BDF arithmetic, ECAM configuration-space reads, header and BAR decoding |
//! | [`id`] | the device table: device ids, generations, display versions, steppings, quirks, apertures |
//! | [`regs`] | the register table, the mapped window, typed 32-bit access and its ordering |
//! | [`probe`] | the walk, the identity decision, the register reads, and the report |
//! | [`debugfs`] | the report, exposed as a file in the DRM debug filesystem |
//!
//! ## What it proves, and what it does not
//!
//! A successful probe proves that the platform's configuration space was read,
//! that a device matching the table is present at a specific bus address, that
//! its register aperture was mapped, and that the registers answered with
//! values -- which is a different claim from "the values are correct", and a
//! much weaker one than "the display engine works".  No mode is set, no plane
//! is enabled, no pixel is scanned out.  On a machine that has never run this
//! code against a real Gen12 part, the register *values* in the report have
//! never been observed: the decoding around them has been tested against
//! synthetic configuration space, and that is the limit of what the tests say.
//!
//! ## Where the display driver plugs in
//!
//! Nothing here precludes a modeset driver, and two decisions are there to
//! make one possible:
//!
//! * The identity layer is the only place that knows what a device *is*
//!   ([`id::DisplayDevice`]), so a mode-setting driver asks it for the
//!   apertures, the stepping and the quirks instead of switching on a device
//!   id of its own.
//! * The register window is mapped once, here, and stays mapped in the kernel
//!   address space.  A driver that later owns the device reuses the same
//!   window through the same [`regs::RegisterWindow`] type.
//!
//! The scanout side is deliberately untouched: `pseudofs::dev::scanout`
//! defines the surface a framebuffer console consumes, and an Intel display
//! driver would implement it once it can program a pipe.  This module supplies
//! the device identity and register access that implementation will need, and
//! nothing else.

mod clk;
pub(crate) mod debugfs;
mod gmbus;
mod hpd;
mod id;
mod pci;
mod phy;
mod pll;
mod power;
mod probe;
mod regs;
mod sink;
mod timing;

#[cfg(test)]
mod testbus;

use alloc::string::String;

use spin::Mutex;

use self::{
    id::Aperture,
    probe::{BusFacts, ProbeReport, WindowStatus},
    regs::{PROBE_WINDOW, RegisterWindow},
};

/// The report of the one probe this kernel runs, kept for the debug file.
///
/// The probe runs once, at boot, and its findings do not change afterwards:
/// the device is either there or it is not, and re-reading registers on every
/// open of a debug file would make a diagnostic tool a source of hardware
/// traffic.
static REPORT: Mutex<Option<ProbeReport>> = Mutex::new(None);

/// What the power step of the bring-up order found, kept for the debug file.
///
/// `None` means either that the step did not run -- there was no identified
/// device, or its register window could not be mapped -- or that it ran and
/// failed, in which case [`POWER_FAILURE`] holds the reason.  The two are kept
/// apart on purpose: "the display was never powered" and "the display could not
/// be powered" are different facts and the log says which one happened.
static POWER: Mutex<Option<power::PowerState>> = Mutex::new(None);

/// Why the power step did not produce a state, when it was attempted and
/// failed.  Reference §11 phase 1: everything after this point is behind the
/// power wells, so a failure here is reported once, in full, rather than
/// re-derived by every later step that then fails for an unrelated-looking
/// reason.
static POWER_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// What the sink step of the bring-up order found, kept for the debug file.
///
/// Separate from [`REPORT`] because it is produced by a different step, and
/// kept for the same reason: the EDID is the first fact in this kernel that the
/// firmware did not supply, and on a machine whose only console is the screen
/// it is worth being able to read twice.
static SINK: Mutex<Option<sink::SinkReport>> = Mutex::new(None);

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
/// The console is the only output device on the target machine, so the report
/// goes to the log as it is produced: a person who was not watching the boot
/// can read the same text back from [`debugfs`] afterwards.
///
/// The probe runs first and alone; the bring-up order's later steps are run
/// from [`bring_up_at_boot`], which needs the window this produces.
pub(crate) fn probe_at_boot() {
    let report = platform_probe();
    report.log();
    *REPORT.lock() = Some(report);
}

/// Run the rest of the bring-up order against the device the probe found.
///
/// This is reference §11 phases 1 and 2, in the order the reference gives them,
/// and each step is attempted only on a device whose register window the probe
/// mapped -- a device this kernel has a model for.  On a machine with no Intel
/// display, or one whose window could not be mapped, nothing here runs and it
/// says so rather than reporting a half-run sequence as a result.
///
/// The order is the point.  Power comes before the sink because GMBUS needs the
/// AUX/DDC power well and a well cannot be requested before `PW_1` is up; the
/// sink comes before any mode because a timing computed from a guessed EDID is
/// worse than no timing at all.  Each step logs as it goes, for the same reason
/// the probe does.
pub(crate) fn bring_up_at_boot() {
    let windows = mapped_windows();
    if windows.is_empty() {
        axlog::info!(
            "intel-gpu: no device with a mapped register window, so the power and sink steps of \
             the bring-up order did not run"
        );
        return;
    }

    for (bdf, window) in &windows {
        axlog::info!("intel-gpu: powering up {bdf} (reference section 11 phase 1)");
        match power::bring_up(window) {
            Ok(state) => {
                state.log();
                *POWER.lock() = Some(state);
            }
            Err(error) => {
                // One line, once, and the sequence stops here: §11 phase 1 is
                // the gate everything else is behind, and a later step that
                // fails because the display is unpowered would be reported as
                // its own fault rather than this one's.
                let text = alloc::format!("intel-gpu: power: {} ({error:?})", error.describe());
                axlog::warn!("{text}");
                *POWER_FAILURE.lock() = Some(text);
                continue;
            }
        }
    }

    // Phase 2 runs for every mapped device whose power came up, because the
    // sink is per-device and one device's monitor must not be lost to another
    // device's failure.
    let report = { REPORT.lock().clone() };
    if let Some(report) = report {
        let sink = sink::probe_at_boot(&report);
        *SINK.lock() = Some(sink);
    }
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

/// The report of the boot probe as text.
pub(crate) fn report_text() -> String {
    let mut text = match &*REPORT.lock() {
        Some(report) => report.render(),
        None => String::from(
            "intel-gpu: the Intel display probe has not run in this boot; it runs from the DRM \
             initialization path, so this file is empty only if that path never executed\n",
        ),
    };
    // The locks are taken one at a time: nothing holds two at once.
    if let Some(failure) = &*POWER_FAILURE.lock() {
        text.push_str(failure);
        text.push('\n');
    }
    if let Some(state) = &*POWER.lock() {
        text.push_str(&state.render());
    }
    if let Some(sink) = &*SINK.lock() {
        text.push_str(&sink.render());
    }
    text
}

/// Whether the boot probe found a display device it could identify.
pub(crate) fn identified_device() -> Option<&'static id::DisplayDevice> {
    let report = REPORT.lock();
    let report = report.as_ref()?;
    report
        .displays
        .iter()
        .find_map(|found| found.identity.device())
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

/// Map the register aperture of a device the table has a model for.
///
/// This is the only place the probe turns an address from configuration space
/// into memory it can read, so it is where every refusal belongs: a device
/// with no memory BAR where the table puts the registers, a BAR that decodes
/// no address, and a BAR whose address contradicts the modelled aperture size
/// are all refused rather than mapped and interpreted.
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

#[cfg(test)]
mod tests {
    use super::hex;

    #[test]
    fn hexadecimal_is_grouped_the_way_register_dumps_are() {
        assert_eq!(hex(0x0000_000c, 8), "0x0000_000c");
        assert_eq!(hex(0x6000_0000_0000, 16), "0x0000_6000_0000_0000");
        assert_eq!(hex(0xffff_ffff, 8), "0xffff_ffff");
        assert_eq!(hex(0, 8), "0x0000_0000");
        assert_eq!(hex(0, 16), "0x0000_0000_0000_0000");
        assert_eq!(hex(0x300, 4), "0x0300");
        assert_eq!(hex(0x46d0, 4), "0x46d0");
        // A request for more digits than a value has still produces a
        // fixed-width field, which is what makes a column of them line up.
        assert_eq!(hex(0x46d0, 8), "0x0000_46d0");
        // And a request for fewer digits than the value needs does not
        // truncate it.
        assert_eq!(hex(0xdead_beef, 4), "0xdead_beef");
    }
}
