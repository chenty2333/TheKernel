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

pub(crate) mod debugfs;
mod gmbus;
mod hpd;
mod id;
mod pci;
mod probe;
mod regs;

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
pub(crate) fn probe_at_boot() {
    let report = platform_probe();
    report.log();
    *REPORT.lock() = Some(report);
}

/// The report of the boot probe as text.
pub(crate) fn report_text() -> String {
    match &*REPORT.lock() {
        Some(report) => report.render(),
        None => String::from(
            "intel-gpu: the Intel display probe has not run in this boot; it runs from the DRM \
             initialization path, so this file is empty only if that path never executed\n",
        ),
    }
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
