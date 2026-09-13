//! The probe: find the device, identify it, read what it says about itself.
//!
//! The probe answers four questions, in order, and stops at the first one it
//! cannot answer:
//!
//! 1. What PCI functions does this platform have?
//! 2. Is one of them an Intel display device?
//! 3. Does this kernel have a model for it, and therefore a right to interpret
//!    its registers?
//! 4. What do the registers it is safe to read say?
//!
//! Everything it produces is one text report.  The report is emitted to the
//! console at boot *and* served from the DRM debug filesystem, from the same
//! rendering, because the target machine has no serial port: whatever the
//! probe learns has to be legible on the screen the kernel is already drawing
//! on, and has to still be there afterwards for a person who was not watching.
//!
//! The probe never writes a register, never writes configuration space, and
//! never programs an aperture.  That is the whole of its claim: it proves the
//! device was found, that its registers were mapped, and that they answered.
//! It does not prove the display engine works.

use alloc::{format, string::String, vec::Vec};

use super::{
    id::{self, Aperture, Identity, Policy},
    pci::{self, Bdf, BusScan, ConfigSpace, DeviceInfo},
    regs::{self, Meaning, Register, RegisterWindow, Width},
};

/// What the platform says about where configuration space is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BusFacts {
    /// The physical base of the ECAM aperture, when the build has one.
    pub(crate) ecam_base: Option<u64>,
    /// The highest bus the platform declares.
    pub(crate) bus_end: u8,
}

/// The state of the one aperture the probe maps.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WindowStatus {
    /// Mapped, and here is the window.
    Mapped {
        window: RegisterWindow,
        aperture: &'static Aperture,
        physical: u64,
    },
    /// Not mapped, and this is why.
    Refused(&'static str),
}

/// One named register and what came back.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reading {
    pub(crate) register: Register,
    /// `None` when the register was deliberately not read.
    pub(crate) value: Option<u64>,
    /// Why it was not read, when it was not.
    pub(crate) skipped: Option<&'static str>,
}

/// A display device the probe found, identified, and (when modelled) read.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) info: DeviceInfo,
    pub(crate) identity: Identity,
    pub(crate) status: WindowStatus,
    pub(crate) readings: Vec<Reading>,
}

impl Found {
    /// The register with this meaning, when the probe read one.
    fn reading(&self, meaning: Meaning) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.register.meaning() == meaning && reading.value.is_some())
    }

    /// The one-line statement of where this device ended up.
    pub(crate) fn verdict(&self) -> String {
        match self.status {
            WindowStatus::Mapped { window, .. } => format!(
                "{} identified and read ({} registers over a {} byte window); nothing was written",
                self.info.bdf,
                self.readings.iter().filter(|r| r.value.is_some()).count(),
                window.len(),
            ),
            WindowStatus::Refused(reason) => {
                format!("{} reported but not touched: {reason}", self.info.bdf)
            }
        }
    }

    /// What the device says about itself in two places that must agree.
    ///
    /// The class code in configuration space is not a constant: the graphics
    /// control register's VGA-addressing bit selects between the VGA-compatible
    /// and the plain display-controller subclass.  One value is read over the
    /// PCI bus and the other out of the register aperture, so agreement is
    /// evidence that the aperture being read belongs to the function being
    /// described -- which is the one thing a probe has no other way to
    /// establish.
    fn cross_checks(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(control) = self.reading(Meaning::GraphicsControl) {
            let value = control.value.unwrap_or(0) as u32;
            let vga_addressing = value >> 2 & 1 == 1;
            let expected_subclass = if vga_addressing {
                pci::SUBCLASS_OTHER_DISPLAY
            } else {
                pci::SUBCLASS_VGA
            };
            let agrees =
                self.info.class == pci::CLASS_DISPLAY && self.info.subclass == expected_subclass;
            notes.push(format!(
                "cross-check: GGC VAMEN={} and class {:#04x}:{:#04x}:{:#04x} {}",
                u8::from(vga_addressing),
                self.info.class,
                self.info.subclass,
                self.info.prog_if,
                if agrees {
                    "agree"
                } else {
                    "DISAGREE: the aperture being read may not belong to this function"
                },
            ));
        }
        note_unprogrammed(
            &mut notes,
            self.reading(Meaning::StolenMemoryBase),
            "DSMBASE",
            "no stolen memory window is programmed",
        );
        note_unprogrammed(
            &mut notes,
            self.reading(Meaning::GttBase),
            "GSMBASE",
            "no graphics translation table base is programmed",
        );
        notes
    }
}

/// Note a 64-bit base register that reads back as zero.
///
/// A zero base is not a failure -- a device the firmware never gave memory to
/// legitimately reads zero -- but it is the difference between "the aperture
/// was read and the firmware has not programmed this" and "the aperture was
/// read and returned a value", and a report that does not say which is
/// useless.
fn note_unprogrammed(notes: &mut Vec<String>, reading: Option<&Reading>, name: &str, text: &str) {
    if let Some(reading) = reading
        && reading.value == Some(0)
    {
        notes.push(format!("{name} = 0: {text}"));
    }
}

/// Everything the probe learned, in the order it learned it.
#[derive(Clone, Debug)]
pub(crate) struct ProbeReport {
    pub(crate) facts: BusFacts,
    /// Why the probe could not run at all, when it could not.
    pub(crate) unavailable: Option<&'static str>,
    /// PCI functions that answered the walk.
    pub(crate) functions: usize,
    /// Every Intel-vendor function the walk saw, display or not.
    pub(crate) intel: Vec<DeviceInfo>,
    /// The Intel display devices, with everything the probe could establish.
    pub(crate) displays: Vec<Found>,
}

impl ProbeReport {
    /// A report for a build or a platform where the probe cannot run.
    pub(crate) fn unavailable(reason: &'static str, facts: BusFacts) -> Self {
        Self {
            facts,
            unavailable: Some(reason),
            functions: 0,
            intel: Vec::new(),
            displays: Vec::new(),
        }
    }

    /// The single sentence a log reader should take away.
    pub(crate) fn verdict(&self) -> String {
        if let Some(reason) = self.unavailable {
            return format!("probe did not run: {reason}");
        }
        if self.displays.is_empty() {
            return format!(
                "no Intel display device present: {} PCI functions answered on buses 0..={}, {} \
                 of them Intel, none of them a display controller. No aperture was mapped and no \
                 register was read.",
                self.functions,
                self.facts.bus_end,
                self.intel.len(),
            );
        }
        let mut verdict = String::new();
        for found in &self.displays {
            verdict.push_str(&found.verdict());
            verdict.push_str("; ");
        }
        verdict.push_str("no register was written and no display mode was set");
        verdict
    }

    /// The report as text: the boot log and the debug file are this string.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("intel-gpu: Intel display probe\n");
        match self.facts.ecam_base {
            Some(base) => out.push_str(&format!(
                "intel-gpu: configuration space: ECAM at {base:#x}, buses 0..={}\n",
                self.facts.bus_end,
            )),
            None => out.push_str(&format!(
                "intel-gpu: configuration space: no ECAM base in this build, buses 0..={}\n",
                self.facts.bus_end,
            )),
        }
        if let Some(reason) = self.unavailable {
            out.push_str(&format!("intel-gpu: {reason}\n"));
            out.push_str(&format!("intel-gpu: verdict: {}\n", self.verdict()));
            return out;
        }
        out.push_str(&format!(
            "intel-gpu: bus walk: {} PCI functions answered, {} from Intel, {} display-class\n",
            self.functions,
            self.intel.len(),
            self.displays.len(),
        ));
        for info in &self.intel {
            out.push_str(&format!(
                "intel-gpu: intel function {} ({}): {}\n",
                info.bdf,
                info.describe_identity(),
                if info.is_intel_display() {
                    "display controller"
                } else {
                    "not a display controller"
                },
            ));
        }
        for found in &self.displays {
            out.push_str(&self.render_device(found));
        }
        out.push_str(&format!("intel-gpu: verdict: {}\n", self.verdict()));
        out.push_str(
            "intel-gpu: this proves the device was enumerated and its registers read back; it \
             does not prove the display engine works, because no mode has been set and no pixel \
             has been scanned out by this kernel\n",
        );
        out
    }

    fn render_device(&self, found: &Found) -> String {
        let info = &found.info;
        let mut out = String::new();
        out.push_str(&format!(
            "intel-gpu: display {}: {}\n",
            info.bdf,
            info.describe_identity(),
        ));
        out.push_str(&format!(
            "intel-gpu:   identity: {}\n",
            found.identity.describe(info.revision, info.device_id),
        ));
        if let Some(device) = found.identity.device() {
            out.push_str(&format!(
                "intel-gpu:   quirks: {}\n",
                describe_quirks(device, info.revision),
            ));
        }
        for bar in info.declared_bars() {
            let aperture = found
                .identity
                .device()
                .and_then(|device| device.apertures.iter().find(|a| a.bar == bar.slot));
            out.push_str(&format!(
                "intel-gpu:   {} ({})\n",
                bar.describe(),
                describe_aperture(aperture, bar),
            ));
        }
        if !info.has_standard_header() {
            out.push_str(&format!(
                "intel-gpu:   header type {:#04x} is not a standard header, so its BAR slots were \
                 not decoded as apertures\n",
                info.header_type,
            ));
        }
        match found.status {
            WindowStatus::Mapped {
                window,
                aperture,
                physical,
            } => out.push_str(&format!(
                "intel-gpu:   register window: mapped {} bytes of {} at {physical:#x} to {:#x}, \
                 uncached, read-only in effect\n",
                window.len(),
                aperture.name,
                window.base(),
            )),
            WindowStatus::Refused(reason) => {
                out.push_str(&format!("intel-gpu:   register window: none ({reason})\n"));
            }
        }
        for reading in &found.readings {
            match reading.value {
                Some(value) => {
                    let digits = if reading.register.width() == Width::Bits64 {
                        16
                    } else {
                        8
                    };
                    out.push_str(&format!(
                        "intel-gpu:   {} ({:#07x}) = {}{}\n",
                        reading.register.name(),
                        reading.register.offset(),
                        crate::drm::intel::hex(value, digits),
                        interpret(reading.register, value),
                    ));
                }
                None => out.push_str(&format!(
                    "intel-gpu:   {} ({:#07x}) = not read: {}\n",
                    reading.register.name(),
                    reading.register.offset(),
                    reading.skipped.unwrap_or("not read"),
                )),
            }
        }
        for note in found.cross_checks() {
            out.push_str(&format!("intel-gpu:   {note}\n"));
        }
        out
    }

    /// Emit the report to the kernel log, one line at a time.
    ///
    /// The log and the debug file carry the same text on purpose: on the
    /// target machine the console is the only thing a person can see, so the
    /// report has to be complete when it is printed, and identical when it is
    /// read back later.
    pub(crate) fn log(&self) {
        for line in self.render().lines() {
            axlog::info!("{line}");
        }
    }
}

/// The quirks a device has at one revision, for a report line.
fn describe_quirks(device: &'static id::DisplayDevice, revision: u8) -> String {
    let mut names = Vec::new();
    for quirk in [
        id::Quirk::GtRegistersRequireForcewake,
        id::Quirk::GmdIdImplemented,
    ] {
        if device.has_quirk(revision, quirk) {
            names.push(match quirk {
                id::Quirk::GtRegistersRequireForcewake => "gt registers need forcewake",
                id::Quirk::GmdIdImplemented => "GMD_ID implemented",
            });
        }
    }
    if names.is_empty() {
        return String::from("none recorded");
    }
    names.join(", ")
}

/// What an aperture is, and whether the BAR the header declares could be it.
fn describe_aperture(aperture: Option<&'static Aperture>, bar: &pci::Bar) -> String {
    let Some(aperture) = aperture else {
        return String::from("no table entry claims this BAR");
    };
    let size = match aperture.size {
        Some(size) => format!("modelled {size} bytes"),
        None => String::from("size not modelled: firmware programs it"),
    };
    let mut consistency = Vec::new();
    match bar.address() {
        Some(address) if aperture.admits_base(address) => {
            consistency.push("base is consistent with the model");
        }
        Some(_) => consistency.push("BASE CONTRADICTS THE MODEL"),
        None => consistency.push("no address decoded"),
    }
    consistency.push(if aperture.admits_encoding(bar) {
        "encoding matches the documentation"
    } else {
        "ENCODING CONTRADICTS THE DOCUMENTATION"
    });
    format!("{} ({size}; {})", aperture.name, consistency.join("; "))
}

/// A short statement about a register value, when one can be made safely.
///
/// The interpretation is deliberately thin, and every field named here is one
/// whose position and meaning are documented rather than inferred.  This kernel
/// has never read these registers on real silicon, and a wrong confident
/// sentence in a log is worse than a raw value.  The full bit layouts live in
/// `docs/design/intel-display-registers.md`.
fn interpret(register: Register, value: u64) -> String {
    match register.meaning() {
        Meaning::GraphicsIp | Meaning::DisplayIp => {
            if value == 0 || value == u64::from(u32::MAX) {
                format!(
                    " (nothing here: all-{})",
                    if value == 0 { "zero" } else { "ones" }
                )
            } else {
                // Architecture 31:22, release 21:14, stepping 5:0.
                format!(
                    " (ip {}.{}, stepping {})",
                    (value >> 22) & 0x3ff,
                    (value >> 14) & 0xff,
                    value & 0x3f,
                )
            }
        }
        Meaning::InterruptState => interpret_interrupts(value as u32),
        Meaning::GraphicsControl => {
            let value = value as u32;
            format!(
                " (GMS={:#04x}, GGMS={:#04x}, VAMEN={}, IVD={})",
                (value >> 8) & 0xff,
                (value >> 6) & 0x3,
                (value >> 2) & 1,
                (value >> 1) & 1,
            )
        }
        Meaning::StolenMemoryBase => {
            if value == 0 {
                String::from(" (not programmed)")
            } else {
                // The base is held in bits 63:20, so the field is 1 MiB
                // granular and the low twenty bits are not part of it.
                format!(
                    " (stolen memory base {})",
                    crate::drm::intel::hex(value & 0xffff_ffff_fff0_0000, 8)
                )
            }
        }
        Meaning::GttBase => {
            if value == 0 {
                String::from(" (not programmed)")
            } else {
                format!(
                    " (translation table base {})",
                    crate::drm::intel::hex(value, 16)
                )
            }
        }
        // The registers of the DDC/EDID transport, the hotplug block and the
        // power wells are named here so that a register table can declare
        // them, but the probe does not read any of them: what they mean
        // depends on a transaction or on a request this report knows nothing
        // about.  `gmbus` and `hpd` interpret them where they are used, and
        // this arm deliberately says nothing rather than saying something
        // shallow about a value the probe never asked for.
        Meaning::BusController | Meaning::Hotplug | Meaning::PowerWell => String::new(),
    }
}

/// Name the interrupt bits a graphics master interrupt register reports.
///
/// The register is readable at any time and reading it acknowledges nothing.
/// Writing it, however, would mask every graphics interrupt on the device,
/// which is why the register table declares it read-only.
fn interpret_interrupts(value: u32) -> String {
    if value == 0 {
        return String::from(" (no graphics interrupt asserted)");
    }
    let mut names = Vec::new();
    if value >> 31 & 1 == 1 {
        names.push(String::from("master enable set"));
    }
    if value >> 30 & 1 == 1 {
        names.push(String::from("power controller"));
    }
    if value >> 29 & 1 == 1 {
        names.push(String::from("graphics unit misc"));
    }
    if value >> 16 & 1 == 1 {
        names.push(String::from("display"));
    }
    for bit in 0..16 {
        if value >> bit & 1 == 1 {
            names.push(format!("gt interrupt dword {bit}"));
        }
    }
    format!(" ({})", names.join(", "))
}

/// Run the probe.
///
/// `open` is where the aperture becomes mapped memory.  On the target it maps
/// the register aperture uncached; in the host tests it hands back a window
/// over an ordinary buffer, which is what makes the whole path -- enumeration,
/// identification, register read, report -- something that can be tested
/// without a graphics device.
pub(crate) fn run<C, F>(config: &C, facts: BusFacts, mut open: F) -> ProbeReport
where
    C: ConfigSpace + ?Sized,
    F: FnMut(&DeviceInfo, &'static Aperture) -> WindowStatus,
{
    let scan = BusScan::walk(config, facts.bus_end);
    let mut displays = Vec::new();
    for info in scan.intel_displays() {
        let identity = id::identify(info);
        let status = match (
            identity.policy(),
            identity
                .device()
                .and_then(|device| device.register_aperture()),
        ) {
            (Policy::Identify, Some(aperture)) => open(info, aperture),
            (Policy::Identify, None) => WindowStatus::Refused(
                "the device table declares no register aperture for this device",
            ),
            (Policy::ReportOnly, _) => WindowStatus::Refused(
                "this kernel has no register model for the device, so its apertures are reported \
                 and not touched",
            ),
            (Policy::Ignore, _) => continue,
        };
        let readings = match status {
            WindowStatus::Mapped { window, .. } => read_named(&window, identity, info.revision),
            WindowStatus::Refused(_) => Vec::new(),
        };
        displays.push(Found {
            info: *info,
            identity,
            status,
            readings,
        });
    }
    ProbeReport {
        facts,
        unavailable: None,
        functions: scan.functions.len(),
        intel: scan.intel_functions().copied().collect(),
        displays,
    }
}

/// Read every named register the device's model says is safe to read.
///
/// A register whose quirk the device does not have is not read and not
/// silently dropped: it is recorded as skipped, with the reason, because "this
/// part does not implement that register" is a fact a reader of the report
/// needs and a blank line would hide.
fn read_named(window: &RegisterWindow, identity: Identity, revision: u8) -> Vec<Reading> {
    regs::NAMED
        .iter()
        .map(|&register| {
            if let Some(quirk) = register.required_quirk() {
                let supported = identity
                    .device()
                    .is_some_and(|device| device.has_quirk(revision, quirk));
                if !supported {
                    return Reading {
                        register,
                        value: None,
                        skipped: Some(match quirk {
                            id::Quirk::GmdIdImplemented => {
                                "the device table does not record GMD_ID for this part"
                            }
                            id::Quirk::GtRegistersRequireForcewake => {
                                "this register needs forcewake, which this probe does not hold"
                            }
                        }),
                    };
                }
            }
            let value = match register.width() {
                Width::Bits32 => window.read(register).map(u64::from),
                Width::Bits64 => window.read64(register),
            };
            Reading {
                register,
                value,
                skipped: value
                    .is_none()
                    .then_some("the mapped window does not contain this register"),
            }
        })
        .collect()
}

/// The registers the probe reads, in report order.
pub(crate) fn named_registers() -> &'static [Register] {
    regs::NAMED
}

/// The BDF of the first display device the report describes.
pub(crate) fn first_display(report: &ProbeReport) -> Option<Bdf> {
    report.displays.first().map(|found| found.info.bdf)
}

/// The PCI functions the report saw from Intel, by BDF.
pub(crate) fn intel_bdfs(report: &ProbeReport) -> Vec<Bdf> {
    report.intel.iter().map(|info| info.bdf).collect()
}

/// A one-line description of configuration space, for a report header.
pub(crate) fn describe_facts(facts: &BusFacts) -> String {
    match facts.ecam_base {
        Some(base) => format!("ECAM at {base:#x}, buses 0..={}", facts.bus_end),
        None => format!("no ECAM base, buses 0..={}", facts.bus_end),
    }
}

/// The probe's own name, so every line it prints can be found with one search.
pub(crate) const PREFIX: &str = "intel-gpu";

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::{
        drm::intel::{
            pci::{Bdf, CLASS_DISPLAY, SUBCLASS_VGA, VENDOR_INTEL},
            testbus::{FakeBus, Header},
        },
        test_support::scheduler_test_context,
    };

    /// A bus shaped like the target machine's: an Intel host bridge, an Intel
    /// SATA controller, and the integrated graphics device at 00:02.0 with the
    /// BARs a Gen12 part has -- a 64-bit non-prefetchable 16 MiB BAR0 at a
    /// 16 MiB-aligned address, and a 64-bit prefetchable BAR2.
    fn target_like_bus() -> FakeBus {
        FakeBus::new(vec![
            Header::new(Bdf::new(0, 0, 0), VENDOR_INTEL, 0x4641)
                .class(0x06, 0x00)
                .multifunction(),
            Header::new(Bdf::new(0, 0, 1), VENDOR_INTEL, 0x4641).class(0x06, 0x00),
            Header::new(Bdf::new(0, 0x1f, 0), VENDOR_INTEL, 0x5480)
                .class(0x06, 0x01)
                .multifunction(),
            Header::new(Bdf::new(0, 0x1f, 2), VENDOR_INTEL, 0x54d3).class(0x01, 0x06),
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .revision(0x04)
                .subsystem(0x1025, 0x161c)
                .bars([
                    0x0000_0004,
                    0x0000_6000,
                    0x9000_000c,
                    0x0000_0000,
                    0x0000_c001,
                    0x0000_0000,
                ]),
        ])
    }

    /// The register aperture of the target device, for tests that need to name
    /// one without asking the table.
    fn register_aperture() -> &'static Aperture {
        id::DEVICES
            .iter()
            .find(|device| device.device_id == 0x46d0)
            .and_then(|device| device.register_aperture())
            .unwrap()
    }

    /// A window over ordinary memory, standing in for the mapped aperture.
    ///
    /// The buffer is heap allocated: it is two megabytes, which is more than a
    /// test thread's stack can hold.
    struct FakeAperture {
        words: Vec<u32>,
    }

    impl FakeAperture {
        fn new() -> Self {
            Self {
                words: vec![0; regs::PROBE_WINDOW / 4],
            }
        }

        fn window(&mut self) -> RegisterWindow {
            // SAFETY: `words` is a live, 4-byte aligned buffer of exactly
            // `PROBE_WINDOW` bytes, and every access goes through the window
            // built here while this borrow is alive.
            unsafe {
                RegisterWindow::from_mapped(self.words.as_mut_ptr() as usize, regs::PROBE_WINDOW)
            }
        }

        /// Set a named register to the value a plausible part would report.
        fn set(&mut self, name: &str, value: u64) {
            let register = named_registers()
                .iter()
                .find(|register| register.name() == name)
                .unwrap_or_else(|| panic!("{name} is not in the register table"));
            let offset = register.offset() as usize / 4;
            self.words[offset] = value as u32;
            if register.width() == Width::Bits64 {
                self.words[offset + 1] = (value >> 32) as u32;
            }
        }
    }

    fn probe_with_fake_window(bus: &FakeBus, aperture: &mut FakeAperture) -> ProbeReport {
        let facts = BusFacts {
            ecam_base: Some(0xe000_0000),
            bus_end: bus.bus_end,
        };
        run(bus, facts, |_info, spec| WindowStatus::Mapped {
            window: aperture.window(),
            aperture: spec,
            physical: 0x6000_0000,
        })
    }

    /// Whether the report says the probe ran.
    fn unavailable_none(report: &ProbeReport) -> bool {
        report.unavailable.is_none()
    }

    /// A device that exists, with a firmware that has programmed its memory.
    fn program_fake_aperture(aperture: &mut FakeAperture) {
        // The graphics control register: VAMEN clear, which is what makes the
        // function report the VGA-compatible subclass.
        aperture.set("GGC", 0x0000_0300);
        aperture.set("GFX_MSTR_IRQ", 0x8000_0000);
        aperture.set("DSMBASE", 0x0000_0000_8000_0000);
        aperture.set("GSMBASE", 0x0000_0000_7000_0000);
        aperture.set("GMD_ID", 0x0000_0000_0000_0c13);
        aperture.set("GMD_ID_DISPLAY", 0x0000_0000_0000_0e00);
    }

    #[test]
    fn a_shaped_bus_produces_a_complete_report() {
        let _guard = scheduler_test_context();
        let bus = target_like_bus();
        let mut aperture = FakeAperture::new();
        program_fake_aperture(&mut aperture);
        let report = probe_with_fake_window(&bus, &mut aperture);

        assert!(unavailable_none(&report));
        assert_eq!(report.functions, 5);
        // Five Intel functions: a host bridge with two, a multifunction LPC
        // and SATA pair, and the display device itself.
        assert_eq!(report.intel.len(), 5);
        assert_eq!(report.displays.len(), 1);
        let found = &report.displays[0];
        assert_eq!(found.info.bdf, Bdf::new(0, 2, 0));
        assert_eq!(found.info.device_id, 0x46d0);
        assert_eq!(found.info.subsystem_id, 0x161c);
        let device = found.identity.device().unwrap();
        assert_eq!(device.name, "Alder Lake-N integrated graphics");
        assert!(matches!(found.status, WindowStatus::Mapped { .. }));
        // The two GMD_ID registers are skipped on this part, and the four the
        // part does implement are read.
        let read: Vec<&str> = found
            .readings
            .iter()
            .filter(|reading| reading.value.is_some())
            .map(|reading| reading.register.name())
            .collect();
        assert_eq!(
            read,
            vec!["GFX_MSTR_IRQ", "GGC", "DSMBASE", "GSMBASE"],
            "the register set a part is read with is its table entry's decision"
        );

        let text = report.render();
        // The report states identity, every BAR, the window, and the verdict.
        assert!(text.contains("0000:00:02.0"), "{text}");
        assert!(text.contains("device 0x46d0"), "{text}");
        assert!(text.contains("revision 0x04"), "{text}");
        assert!(text.contains("subsystem 0x1025:0x161c"), "{text}");
        assert!(text.contains("class 0x03:0x00:0x00"), "{text}");
        assert!(
            text.contains("BAR0 memory 64-bit at 0x0000_6000_0000_0000"),
            "{text}"
        );
        assert!(text.contains("high 0x0000_6000"), "{text}");
        assert!(text.contains("GTTMMADR"), "{text}");
        assert!(text.contains("base is consistent with the model"), "{text}");
        assert!(
            text.contains("encoding matches the documentation"),
            "{text}"
        );
        assert!(
            text.contains("BAR2 memory 64-bit prefetchable at 0x0000_0000_9000_0000"),
            "{text}"
        );
        assert!(text.contains("GMADR"), "{text}");
        assert!(text.contains("BAR4 i/o port"), "{text}");
        assert!(text.contains("no table entry claims this BAR"), "{text}");
        assert!(text.contains("register window: mapped"), "{text}");
        assert!(
            text.contains(
                "DSMBASE (0x1080c0) = 0x0000_0000_8000_0000 (stolen memory base 0x8000_0000)"
            ),
            "{text}"
        );
        assert!(text.contains("cross-check: GGC VAMEN=0"), "{text}");
        assert!(text.contains("agree"), "{text}");
        assert!(
            text.contains("verdict: 0000:00:02.0 identified and read"),
            "{text}"
        );
        // The honest label travels with the report, not only with the commit
        // message.
        assert!(
            text.contains("does not prove the display engine works"),
            "{text}"
        );
    }

    #[test]
    fn every_line_of_the_report_is_labelled() {
        let _guard = scheduler_test_context();
        let bus = target_like_bus();
        let mut aperture = FakeAperture::new();
        let report = probe_with_fake_window(&bus, &mut aperture);
        for line in report.render().lines() {
            assert!(
                line.starts_with("intel-gpu: "),
                "unlabelled report line: {line}"
            );
        }
    }

    #[test]
    fn a_bus_without_the_device_says_so_and_reads_nothing() {
        let _guard = scheduler_test_context();
        // A machine with Intel devices but no display controller: the shape of
        // every QEMU profile this kernel boots on.
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 0, 0), VENDOR_INTEL, 0x29c0)
                .class(0x06, 0x00)
                .multifunction(),
            Header::new(Bdf::new(0, 0x1f, 0), VENDOR_INTEL, 0x2918)
                .class(0x06, 0x01)
                .multifunction(),
            Header::new(Bdf::new(0, 0x1f, 2), VENDOR_INTEL, 0x2922).class(0x01, 0x06),
            Header::new(Bdf::new(0, 3, 0), 0x1af4, 0x1042).class(0x02, 0x00),
        ]);
        let facts = BusFacts {
            ecam_base: Some(0xe000_0000),
            bus_end: 0xff,
        };
        let mut window_opened = false;
        let report = run(&bus, facts, |_, _| {
            window_opened = true;
            WindowStatus::Refused("must not be called")
        });
        assert!(!window_opened, "no aperture may be opened without a device");
        assert_eq!(report.functions, 4);
        assert_eq!(report.intel.len(), 3);
        // One function whose device has three, and the display device is not
        // among them.
        assert!(report.displays.is_empty());
        let verdict = report.verdict();
        assert!(
            verdict.contains("no Intel display device present"),
            "{verdict}"
        );
        assert!(verdict.contains("No aperture was mapped"), "{verdict}");
        assert!(verdict.contains("3 of them Intel"), "{verdict}");
        // The walk found the display-capable device's siblings but not a
        // display controller, which is what "no device" means here.
        // Every Intel function is still named, so a reader can see what the
        // walk actually saw rather than an unexplained silence.
        let text = report.render();
        assert!(text.contains("0000:00:1f.2"), "{text}");
        assert!(text.contains("device 0x2922"), "{text}");
        assert!(text.contains("device 0x2918"), "{text}");
        assert!(text.contains("not a display controller"), "{text}");
    }

    #[test]
    fn an_unmodelled_display_device_is_reported_and_left_alone() {
        let _guard = scheduler_test_context();
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x9abc)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .revision(0x01)
                .bars([0x0000_0004, 0x0000_6000, 0, 0, 0, 0]),
        ]);
        let facts = BusFacts {
            ecam_base: Some(0xe000_0000),
            bus_end: 0xff,
        };
        let mut window_opened = false;
        let report = run(&bus, facts, |_, _| {
            window_opened = true;
            WindowStatus::Refused("must not be called")
        });
        // The probe knows the device is a display controller and still refuses
        // to open its aperture, because it has no model to read it with.
        assert!(!window_opened);
        assert_eq!(report.displays.len(), 1);
        let found = &report.displays[0];
        assert_eq!(found.identity, Identity::Unmodelled);
        assert!(found.readings.is_empty());
        let text = report.render();
        assert!(
            text.contains("unmodelled Intel display device 0x9abc"),
            "{text}"
        );
        assert!(text.contains("no register model for the device"), "{text}");
        // Its BARs are still reported, because configuration space is a
        // standard this code knows how to decode.
        assert!(
            text.contains("BAR0 memory 64-bit at 0x0000_6000_0000_0000"),
            "{text}"
        );
        assert!(text.contains("no table entry claims this BAR"), "{text}");
        assert!(text.contains("reported but not touched"), "{text}");
    }

    #[test]
    fn an_aperture_that_contradicts_the_model_is_called_out() {
        let _guard = scheduler_test_context();
        // A 64-bit memory BAR0 whose base is not aligned to the aperture the
        // table models: the probe must report the contradiction rather than
        // map on top of it.
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .bars([0x0000_1004, 0x0000_0000, 0, 0, 0, 0]),
        ]);
        let facts = BusFacts {
            ecam_base: Some(0xe000_0000),
            bus_end: 0xff,
        };
        let mut aperture = FakeAperture::new();
        let report = run(&bus, facts, |_, spec| {
            // The real mapping path refuses here before it maps anything; the
            // report is what is under test, so hand back a window anyway and
            // check that the report still says what is wrong.
            WindowStatus::Mapped {
                window: aperture.window(),
                aperture: spec,
                physical: 0x1000,
            }
        });
        let text = report.render();
        assert!(text.contains("BASE CONTRADICTS THE MODEL"), "{text}");
    }

    #[test]
    fn a_part_without_gmd_id_reports_it_as_skipped_with_a_reason() {
        let _guard = scheduler_test_context();
        // The target part has no GMD_ID.  The registers are still in the table,
        // and the report says why they were not read.
        let bus = target_like_bus();
        let mut aperture = FakeAperture::new();
        let report = probe_with_fake_window(&bus, &mut aperture);
        let found = &report.displays[0];
        for name in ["GMD_ID", "GMD_ID_DISPLAY"] {
            let reading = found
                .readings
                .iter()
                .find(|reading| reading.register.name() == name)
                .unwrap_or_else(|| panic!("{name} is in the register table"));
            assert_eq!(reading.value, None);
            assert!(reading.skipped.unwrap().contains("does not record GMD_ID"));
        }
        let text = report.render();
        assert!(text.contains("GMD_ID (0x00d8c) = not read"), "{text}");
        assert!(
            text.contains("GMD_ID_DISPLAY (0x510a0) = not read"),
            "{text}"
        );
    }

    #[test]
    fn zero_base_registers_are_called_out_rather_than_left_ambiguous() {
        let _guard = scheduler_test_context();
        let bus = target_like_bus();
        // A firmware that reserved no graphics memory: both base registers
        // read zero, which the report states instead of printing two zeros and
        // letting the reader guess.
        let mut aperture = FakeAperture::new();
        let report = probe_with_fake_window(&bus, &mut aperture);
        let text = report.render();
        assert!(
            text.contains("DSMBASE = 0: no stolen memory window is programmed"),
            "{text}"
        );
        assert!(
            text.contains("GSMBASE = 0: no graphics translation table base is programmed"),
            "{text}"
        );
    }

    #[test]
    fn a_disagreement_between_the_register_and_the_header_is_reported() {
        let _guard = scheduler_test_context();
        // The class code says VGA-compatible (subclass 0), and the graphics
        // control register says VGA addressing is enabled (VAMEN=1), which
        // would make the class 0x0380.  Two reads of the same device disagree,
        // and that is exactly the case a probe exists to surface.
        let bus = FakeBus::new(vec![
            Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
                .class(CLASS_DISPLAY, SUBCLASS_VGA)
                .bars([0x0000_0004, 0x0000_6000, 0, 0, 0, 0]),
        ]);
        let facts = BusFacts {
            ecam_base: Some(0xe000_0000),
            bus_end: 0xff,
        };
        let mut aperture = FakeAperture::new();
        aperture.set("GGC", 0x0000_0304);
        let report = run(&bus, facts, |_, spec| WindowStatus::Mapped {
            window: aperture.window(),
            aperture: spec,
            physical: 0x6000_0000,
        });
        let text = report.render();
        assert!(text.contains("VAMEN=1"), "{text}");
        assert!(text.contains("DISAGREE"), "{text}");
    }

    #[test]
    fn an_unavailable_probe_says_why_instead_of_pretending() {
        let _guard = scheduler_test_context();
        let report = ProbeReport::unavailable(
            "hosted test build: PCI configuration space is not reachable",
            BusFacts {
                ecam_base: None,
                bus_end: 0,
            },
        );
        let text = report.render();
        assert!(text.contains("hosted test build"), "{text}");
        assert!(text.contains("verdict: probe did not run"), "{text}");
    }

    #[test]
    fn facts_and_helpers_agree_with_the_report() {
        let _guard = scheduler_test_context();
        let bus = target_like_bus();
        let mut aperture = FakeAperture::new();
        let report = probe_with_fake_window(&bus, &mut aperture);
        assert_eq!(first_display(&report), Some(Bdf::new(0, 2, 0)));
        assert_eq!(
            intel_bdfs(&report),
            vec![
                Bdf::new(0, 0, 0),
                Bdf::new(0, 0, 1),
                Bdf::new(0, 2, 0),
                Bdf::new(0, 0x1f, 0),
                Bdf::new(0, 0x1f, 2)
            ]
        );
        assert_eq!(
            describe_facts(&report.facts),
            "ECAM at 0xe0000000, buses 0..=255"
        );
        assert_eq!(PREFIX, "intel-gpu");
        assert_eq!(named_registers().len(), 6);
        assert_eq!(register_aperture().name, "GTTMMADR");
    }
}
