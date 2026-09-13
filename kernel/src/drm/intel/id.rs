//! What an Intel display device *is*, as data.
//!
//! Everything above the probe -- the register window, the modeset path, the
//! display engine, the power wells -- differs between Intel graphics
//! generations in ways that are known before any register is read: which PCI
//! device ids exist, which graphics generation and display version they carry,
//! how a PCI revision maps onto a silicon stepping, which aperture holds the
//! registers, and which of the hardware's known misbehaviours apply.  If those
//! facts live in `if` statements scattered through a driver, then every new
//! part is a code change in five places and a device nobody has added yet is
//! driven incorrectly.  They live here instead, as one table.
//!
//! The consequence is the important part: **an unknown device is a first-class
//! answer**.  [`identify`] returns [`Identity::Unmodelled`] for an Intel
//! display device the table does not describe, and the probe's response to
//! that is to describe the function -- configuration space is a standard and
//! decoding it is not guesswork -- and to refuse to touch its apertures, since
//! interpreting a register without a model of the part is exactly the guessing
//! this layer exists to prevent.
//!
//! ## Provenance
//!
//! Each entry records where its facts come from.  A field that could not be
//! established from public documentation is left as `None` or as an empty
//! table rather than filled with a plausible value, and the probe reports the
//! absence.  The deep register reference -- offsets, bit fields, sequences --
//! is `docs/design/intel-display-registers.md`; this file holds only what
//! identifies a part.

/// The graphics generation a device belongs to.
///
/// This is Intel's *graphics* version, which is what the register model and
/// the execution units follow.  It is not the display version: the display
/// engine has its own version number and advances on its own schedule, which
/// is why both are fields rather than one being derived from the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Generation {
    /// Gen9, the Skylake-through-Coffee-Lake register model.
    Gen9,
    /// Gen11, Ice Lake.
    Gen11,
    /// Gen12, the Xe-LP and Xe-HPG parts: Tiger Lake, Alder Lake, Alder
    /// Lake-N, DG1, DG2.
    Gen12,
    /// Xe2, the Lunar Lake and Battlemage generation.
    Xe2,
}

impl Generation {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Gen9 => "Gen9",
            Self::Gen11 => "Gen11",
            Self::Gen12 => "Gen12",
            Self::Xe2 => "Xe2",
        }
    }
}

/// What a graphics aperture is for.
///
/// A driver asks for an aperture by role rather than by BAR number, because
/// the role is what its code means and the BAR number is what happens to
/// implement it on the parts in the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApertureRole {
    /// The register aperture: the 32-bit MMIO register space, and the only
    /// aperture this probe maps.
    Registers,
    /// The graphics memory aperture: the window through which the display
    /// engine reads scanout surfaces and the CPU reaches stolen/local memory.
    GraphicsMemory,
}

/// One aperture an Intel display device exposes through a PCI BAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Aperture {
    /// The name a human and the Intel documentation use.
    pub(crate) name: &'static str,
    pub(crate) role: ApertureRole,
    /// The BAR slot that implements it.
    pub(crate) bar: u8,
    /// The modelled size in bytes, or `None` when public documentation did not
    /// establish it as a property of the part.  A probe that cannot state an
    /// aperture's size must not map it, and a driver that cannot state it must
    /// not place anything in it; the absence is carried as data rather than
    /// filled in with a guess.
    pub(crate) size: Option<u64>,
    /// The address width the documentation specifies for this BAR.
    pub(crate) width: super::pci::BarWidth,
    /// Whether the documentation specifies this BAR as prefetchable.
    pub(crate) prefetchable: bool,
}

impl Aperture {
    /// Whether a BAR base address could be this aperture, judged only from
    /// facts a read-only probe has.
    ///
    /// A PCI BAR is naturally aligned to its own size, so a modelled size the
    /// observed base is not aligned to is a contradiction: either the model is
    /// wrong or the BAR is not the aperture the model named.  Either way the
    /// probe must say so instead of mapping on top of it.  The check needs no
    /// write to configuration space, which is the only reason a read-only
    /// probe can make it at all.
    pub(crate) fn admits_base(&self, base: u64) -> bool {
        if base == 0 || base & 0xfff != 0 {
            // An unassigned BAR, or one that is not even page aligned, cannot
            // be mapped whatever its size is.
            return false;
        }
        match self.size {
            Some(size) => size.is_power_of_two() && base.is_multiple_of(size),
            // An aperture whose size is not modelled can still be probed, as
            // long as the caller maps only what it can justify: the register
            // window, which is a fixed and documented part of the aperture.
            None => true,
        }
    }

    /// Whether the BAR as configuration space presents it matches what the
    /// documentation says the BAR is.
    ///
    /// Width and prefetchability are properties of the part, so a disagreement
    /// is worth reporting: it is either a board whose firmware programmed
    /// something the documentation does not allow, or a model that is wrong
    /// about the silicon.  Both are things a person reading the report needs
    /// to know before trusting anything else it says.
    pub(crate) fn admits_encoding(&self, bar: &super::pci::Bar) -> bool {
        bar.width() == Some(self.width)
            && matches!(
                bar.kind,
                super::pci::BarKind::Memory {
                    prefetchable,
                    ..
                } if prefetchable == self.prefetchable
            )
    }
}

/// A hardware behaviour a driver must look up rather than infer.
///
/// The alternative -- `if generation >= Gen12` at the point of use -- encodes
/// a fact about a part as control flow, which is how a driver ends up wrong
/// about a part nobody tested.  A quirk is data: it is declared by the table
/// entry (or by the stepping within it) that has the behaviour, and asked for
/// by name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Quirk {
    /// Reads of the GT register ranges return zero unless forcewake is held
    /// for the power well that gates them.  The identity window is not one of
    /// those ranges, which is what lets a probe read it before it can safely
    /// touch power management.
    GtRegistersRequireForcewake,
    /// The part implements `GMD_ID`, the read-only register that states which
    /// graphics IP it contains.  A part without it must be identified from the
    /// configuration header and the table alone.
    GmdIdImplemented,
}

/// The silicon stepping a display device is, as far as its PCI revision says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayStepping {
    /// The letter is the metal layer and the digit the revision within it, the
    /// convention every Intel part uses.
    A0,
    A1,
    B0,
    B1,
    C0,
    C1,
    D0,
    E0,
    /// A revision the table does not map.  It is reported with the revision
    /// that produced it rather than rounded to the nearest known stepping:
    /// a driver that treats an unknown stepping as a known one is a driver
    /// that applies the wrong workaround to real hardware.
    Unknown(u8),
}

impl DisplayStepping {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::A0 => "A0",
            Self::A1 => "A1",
            Self::B0 => "B0",
            Self::B1 => "B1",
            Self::C0 => "C0",
            Self::C1 => "C1",
            Self::D0 => "D0",
            Self::E0 => "E0",
            Self::Unknown(_) => "unknown",
        }
    }

    pub(crate) const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

/// One PCI revision of a device, and the stepping it selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SteppingEntry {
    pub(crate) revision: u8,
    pub(crate) stepping: DisplayStepping,
    /// Behaviour that differs within one device id.  A quirk that only some
    /// steppings of a part have is declared here rather than applied to the
    /// whole device, and a quirk that only some revisions *within* a stepping
    /// have gets its own entry.
    pub(crate) quirks: &'static [Quirk],
}

impl SteppingEntry {
    /// The common case: a revision that selects a stepping and carries no
    /// quirk of its own.
    pub(crate) const fn new(revision: u8, stepping: DisplayStepping) -> Self {
        Self {
            revision,
            stepping,
            quirks: &[],
        }
    }
}

/// One known Intel display device.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DisplayDevice {
    /// The PCI device id that selects this entry.
    pub(crate) device_id: u16,
    /// The marketing name a human recognises.
    pub(crate) name: &'static str,
    pub(crate) generation: Generation,
    /// The display engine's own version, which is what the modeset and DDI
    /// register reference is keyed on.  It is not derived from `generation`:
    /// parts of one graphics generation carry different display engines.  It
    /// is `None` when public documentation did not establish it for this part,
    /// which is a fact the report states rather than hides.
    pub(crate) display_version: Option<u8>,
    /// Every aperture the part exposes, in BAR order.
    pub(crate) apertures: &'static [Aperture],
    /// Behaviour the whole device has, whatever its stepping.
    pub(crate) quirks: &'static [Quirk],
    /// PCI revision to stepping, for the revisions the table knows.
    pub(crate) steppings: &'static [SteppingEntry],
}

impl DisplayDevice {
    /// The stepping this device is at the given PCI revision.
    pub(crate) fn stepping(&self, revision: u8) -> DisplayStepping {
        self.steppings
            .iter()
            .find(|entry| entry.revision == revision)
            .map(|entry| entry.stepping)
            .unwrap_or(DisplayStepping::Unknown(revision))
    }

    /// How to write the display version in a report.
    pub(crate) fn display_version_text(&self) -> alloc::string::String {
        use alloc::{format, string::String};
        match self.display_version {
            Some(version) => format!("display v{version}"),
            None => String::from("display version not established"),
        }
    }

    /// Whether this device has `quirk` at `revision`.
    ///
    /// Device-wide and stepping-specific quirks are one question to a caller:
    /// the answer is assembled here so that no call site has to know which
    /// table a behaviour was recorded in.
    pub(crate) fn has_quirk(&self, revision: u8, quirk: Quirk) -> bool {
        if self.quirks.contains(&quirk) {
            return true;
        }
        self.steppings
            .iter()
            .find(|entry| entry.revision == revision)
            .is_some_and(|entry| entry.quirks.contains(&quirk))
    }

    /// The aperture that role is implemented by.
    pub(crate) fn aperture(&self, role: ApertureRole) -> Option<&'static Aperture> {
        self.apertures.iter().find(|aperture| aperture.role == role)
    }

    /// The register aperture, which is the only one a probe maps.
    pub(crate) fn register_aperture(&self) -> Option<&'static Aperture> {
        self.aperture(ApertureRole::Registers)
    }
}

/// The apertures of an Intel integrated graphics device.
///
/// `GTTMMADR` is BAR0 and `GMADR` is BAR2 on every generation this kernel could
/// run on.  The two differ in a way worth recording: `GTTMMADR` is a hardware
/// constant of exactly 16 MiB -- of which the first 2 MiB is the register
/// window, the next 6 MiB reserved and the last 8 MiB the global page table --
/// while `GMADR`'s size is chosen by firmware through the multi-size aperture
/// control register in configuration space, so no size can be stated for it
/// here.  A driver that needs it reads that register; this probe reports the
/// aperture, its address and its BAR encoding, and says that its size is not
/// modelled.
const INTEGRATED_APERTURES: &[Aperture] = &[
    Aperture {
        name: "GTTMMADR",
        role: ApertureRole::Registers,
        bar: 0,
        size: Some(16 * 1024 * 1024),
        width: super::pci::BarWidth::Bits64,
        // The datasheet marks BAR0's prefetchable bit as hardwired to zero.
        prefetchable: false,
    },
    Aperture {
        name: "GMADR",
        role: ApertureRole::GraphicsMemory,
        bar: 2,
        size: None,
        width: super::pci::BarWidth::Bits64,
        prefetchable: true,
    },
];

/// One Alder Lake-N integrated graphics device.
///
/// `0x46d0` through `0x46d4` are one family: the kernel's own PCI id table
/// groups them as the ADL-N ids, and everything in this table entry is a
/// property of the silicon family rather than of one SKU, so they share it.
///
/// Two fields are deliberately empty.  There is no published PCI
/// revision-to-stepping mapping for this part, so `steppings` is empty and a
/// revision it has not been told about is reported as an unknown stepping
/// rather than rounded to a known one.  And no public document states this
/// part's display engine version, so `display_version` is `None`: a modeset
/// driver must read that from the register reference, not from a number this
/// table invented.
const fn alder_lake_n(device_id: u16) -> DisplayDevice {
    DisplayDevice {
        device_id,
        name: "Alder Lake-N integrated graphics",
        generation: Generation::Gen12,
        display_version: None,
        apertures: INTEGRATED_APERTURES,
        // Gen12 gates most of the register aperture behind forcewake, which is
        // why the probe restricts itself to the bands that do not need it.
        // ADL-N predates the GMD_ID registers, so those registers are not read
        // on this part even though the table knows where they live.
        quirks: &[Quirk::GtRegistersRequireForcewake],
        steppings: &[],
    }
}

/// Every Intel display device this kernel has a model for.
///
/// The table is keyed on the PCI device id alone: the subsystem id identifies
/// the board an OEM built, not the silicon the driver talks to.
pub(crate) const DEVICES: &[DisplayDevice] = &[
    alder_lake_n(0x46d0),
    alder_lake_n(0x46d1),
    alder_lake_n(0x46d2),
    alder_lake_n(0x46d3),
    alder_lake_n(0x46d4),
];

/// What the table says about a PCI function.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Identity {
    /// An Intel display device with a table entry.
    Known(&'static DisplayDevice),
    /// An Intel display device the table does not describe.
    Unmodelled,
    /// Not an Intel display device at all.
    NotDisplay,
}

impl PartialEq for Identity {
    /// Two known identities are the same identity when they are the same
    /// device: the table is `'static`, so comparing the entries themselves
    /// would compare borrowed data rather than the question a caller is
    /// actually asking.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Known(left), Self::Known(right)) => left.device_id == right.device_id,
            (Self::Unmodelled, Self::Unmodelled) | (Self::NotDisplay, Self::NotDisplay) => true,
            _ => false,
        }
    }
}

impl Eq for Identity {}

impl Identity {
    pub(crate) const fn device(self) -> Option<&'static DisplayDevice> {
        match self {
            Self::Known(device) => Some(device),
            Self::Unmodelled | Self::NotDisplay => None,
        }
    }

    /// What the probe may do with this function.
    pub(crate) const fn policy(self) -> Policy {
        match self {
            Self::Known(_) => Policy::Identify,
            Self::Unmodelled => Policy::ReportOnly,
            Self::NotDisplay => Policy::Ignore,
        }
    }

    /// A line for a log or a debug file.
    pub(crate) fn describe(self, revision: u8, device_id: u16) -> alloc::string::String {
        use alloc::format;
        match self {
            Self::Known(device) => format!(
                "{} ({} graphics, {}, stepping {}, {:#06x})",
                device.name,
                device.generation.name(),
                device.display_version_text(),
                device.stepping(revision).name(),
                device_id,
            ),
            Self::Unmodelled => format!(
                "unmodelled Intel display device {device_id:#06x} revision {revision:#04x}: this \
                 kernel has no register model for it, so its apertures will be reported and not \
                 touched"
            ),
            Self::NotDisplay => alloc::string::String::from("not a display device"),
        }
    }
}

/// What a probe is allowed to do once it has identified a function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Policy {
    /// Read the identity window and report.  The device is modelled, which is
    /// what makes reading registers meaningful; nothing is programmed, because
    /// owning a display device is a later commit's job.
    Identify,
    /// Decode configuration space and stop.  Registers of an unmodelled part
    /// cannot be interpreted, and a driver that reads them anyway is guessing
    /// about hardware it has never seen.
    ReportOnly,
    /// Not a display device: nothing to say about it beyond the bus walk.
    Ignore,
}

/// Identify a PCI function against the table.
pub(crate) fn identify(info: &super::pci::DeviceInfo) -> Identity {
    if !info.is_intel_display() {
        return Identity::NotDisplay;
    }
    match DEVICES
        .iter()
        .find(|device| device.device_id == info.device_id)
    {
        Some(device) => Identity::Known(device),
        None => Identity::Unmodelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device built by the test rather than taken from the table, so that
    /// the *shape* rules -- stepping lookup, quirk assembly, aperture roles --
    /// are tested against data the test controls.
    fn synthetic() -> DisplayDevice {
        const APERTURES: &[Aperture] = &[
            Aperture {
                name: "GTTMMADR",
                role: ApertureRole::Registers,
                bar: 0,
                size: Some(16 * 1024 * 1024),
                width: super::super::pci::BarWidth::Bits64,
                prefetchable: false,
            },
            Aperture {
                name: "GMADR",
                role: ApertureRole::GraphicsMemory,
                bar: 2,
                size: None,
                width: super::super::pci::BarWidth::Bits64,
                prefetchable: true,
            },
        ];
        const STEPPINGS: &[SteppingEntry] = &[
            SteppingEntry::new(0x00, DisplayStepping::A0),
            SteppingEntry {
                revision: 0x04,
                stepping: DisplayStepping::B0,
                quirks: &[Quirk::GmdIdImplemented],
            },
        ];
        DisplayDevice {
            device_id: 0x1234,
            name: "Synthetic",
            generation: Generation::Gen12,
            display_version: Some(13),
            apertures: APERTURES,
            quirks: &[Quirk::GtRegistersRequireForcewake],
            steppings: STEPPINGS,
        }
    }

    #[test]
    fn the_table_has_one_entry_per_device_id() {
        for (index, device) in DEVICES.iter().enumerate() {
            assert!(
                !DEVICES[..index]
                    .iter()
                    .any(|other| other.device_id == device.device_id),
                "duplicate device id {:#06x} in the table",
                device.device_id
            );
            // A device with no register aperture could never be probed, so an
            // entry without one is a table mistake rather than a valid part.
            assert!(
                device.register_aperture().is_some(),
                "{:#06x} has no register aperture",
                device.device_id
            );
            for aperture in device.apertures {
                if let Some(size) = aperture.size {
                    assert!(
                        size.is_power_of_two(),
                        "{} size is not a power of two",
                        aperture.name
                    );
                }
                // Every aperture is a memory aperture: an I/O BAR is not
                // something a driver maps.
                assert!(aperture.bar < 6, "{} has an impossible BAR", aperture.name);
            }
            // Forcewake is a property of the generation the table claims, not
            // of an individual entry, so the two must agree.
            let needs_forcewake = matches!(device.generation, Generation::Gen12 | Generation::Xe2);
            assert_eq!(
                device.quirks.contains(&Quirk::GtRegistersRequireForcewake),
                needs_forcewake,
                "{:#06x}: forcewake is a property of the generation",
                device.device_id
            );
        }
    }

    #[test]
    fn the_target_device_identifies_as_alder_lake_n() {
        let device = DEVICES
            .iter()
            .find(|device| device.device_id == 0x46d0)
            .expect("the target machine's device id must be in the table");
        assert_eq!(device.generation, Generation::Gen12);
        let registers = device.register_aperture().unwrap();
        assert_eq!(registers.bar, 0);
        assert_eq!(registers.name, "GTTMMADR");
        // The size is a hardware constant on this generation, which is what
        // makes the base-alignment check meaningful.
        assert_eq!(registers.size, Some(16 * 1024 * 1024));
        assert!(device.has_quirk(0x04, Quirk::GtRegistersRequireForcewake));
        // ADL-N predates GMD_ID, so the probe must not read those registers
        // here even though the table knows their offsets.
        assert!(!device.has_quirk(0x04, Quirk::GmdIdImplemented));
        // The graphics memory aperture size is firmware's choice, so the table
        // states no size rather than a plausible one.
        let graphics_memory = device.aperture(ApertureRole::GraphicsMemory).unwrap();
        assert_eq!(graphics_memory.name, "GMADR");
        assert_eq!(graphics_memory.size, None);
    }

    #[test]
    fn the_whole_alder_lake_n_id_family_is_in_the_table() {
        for device_id in [0x46d0, 0x46d1, 0x46d2, 0x46d3, 0x46d4] {
            let device = DEVICES
                .iter()
                .find(|device| device.device_id == device_id)
                .unwrap_or_else(|| panic!("{device_id:#06x} is missing from the table"));
            assert_eq!(device.name, "Alder Lake-N integrated graphics");
            assert_eq!(device.display_version, None, "not established publicly");
        }
    }

    #[test]
    fn a_revision_the_table_does_not_map_is_reported_not_rounded() {
        let device = synthetic();
        assert_eq!(device.stepping(0x00), DisplayStepping::A0);
        assert_eq!(device.stepping(0x04), DisplayStepping::B0);
        // Revision 0x07 is not in the table.  Treating it as B0 would apply
        // B0's quirks to silicon nobody has characterised.
        assert_eq!(device.stepping(0x07), DisplayStepping::Unknown(0x07));
        assert!(!device.stepping(0x07).is_known());
        assert_eq!(device.stepping(0x07).name(), "unknown");
        assert!(device.stepping(0x04).is_known());
    }

    #[test]
    fn a_quirk_is_asked_for_by_name_and_answered_from_both_tables() {
        let device = synthetic();
        // Declared for the whole device.
        assert!(device.has_quirk(0x00, Quirk::GtRegistersRequireForcewake));
        assert!(device.has_quirk(0x04, Quirk::GtRegistersRequireForcewake));
        // Declared by one stepping only.
        assert!(!device.has_quirk(0x00, Quirk::GmdIdImplemented));
        assert!(device.has_quirk(0x04, Quirk::GmdIdImplemented));
        // Not declared anywhere, including for a revision with no entry.
        assert!(!device.has_quirk(0x07, Quirk::GmdIdImplemented));
    }

    #[test]
    fn an_aperture_is_asked_for_by_role_and_checked_against_its_size() {
        let device = synthetic();
        assert_eq!(device.register_aperture().unwrap().name, "GTTMMADR");
        assert_eq!(
            device.aperture(ApertureRole::GraphicsMemory).unwrap().bar,
            2
        );
        // A size is a claim a read-only probe can check: a BAR is naturally
        // aligned to its own size.
        let registers = device.register_aperture().unwrap();
        assert!(registers.admits_base(0x6000_0000));
        assert!(!registers.admits_base(0x6000_1000), "not aligned to 16 MiB");
        assert!(!registers.admits_base(0), "unassigned");
        assert!(!registers.admits_base(0x6000_0001), "not page aligned");
        // An aperture whose size is not modelled can still be probed, because
        // the probe maps the register window rather than the whole aperture.
        let graphics_memory = device.aperture(ApertureRole::GraphicsMemory).unwrap();
        assert!(graphics_memory.admits_base(0x8000_0000));
        assert!(!graphics_memory.admits_base(0x1234));
    }

    #[test]
    fn an_aperture_checks_the_bar_encoding_the_documentation_states() {
        use crate::drm::intel::pci::{Bar, BarWidth};
        let device = synthetic();
        let registers = device.register_aperture().unwrap();
        // BAR0: 64-bit memory, not prefetchable, as the datasheet says.
        let good = Bar::decode(0, 0x0000_0004, Some(0x0000_0060));
        assert!(registers.admits_encoding(&good));
        // Prefetchable where the documentation says it is hardwired to zero.
        let prefetchable = Bar::decode(0, 0x0000_000c, Some(0x0000_0060));
        assert!(!registers.admits_encoding(&prefetchable));
        // A 32-bit encoding of a 64-bit aperture.
        let narrow = Bar::decode(0, 0x6000_0000, None);
        assert!(!registers.admits_encoding(&narrow));
        // An I/O BAR in a memory aperture's slot.
        let io = Bar::decode(0, 0x0000_c001, None);
        assert!(!registers.admits_encoding(&io));
        let _ = BarWidth::Bits64;
    }

    #[test]
    fn an_unknown_intel_display_device_is_reported_and_refused() {
        use crate::drm::intel::{
            pci::{Bdf, CLASS_DISPLAY, DeviceInfo, SUBCLASS_VGA, VENDOR_INTEL},
            testbus::{FakeBus, Header},
        };

        // An Intel display device whose id is not in the table.
        let unknown = Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x9abc)
            .class(CLASS_DISPLAY, SUBCLASS_VGA)
            .bars([0x0000_000c, 0x0000_0060, 0x9000_0008, 0, 0, 0]);
        let bus = FakeBus::new(alloc::vec![unknown]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 2, 0)).unwrap();
        assert_eq!(identify(&info), Identity::Unmodelled);
        assert_eq!(identify(&info).policy(), Policy::ReportOnly);

        // The same table entry question for a device it does know.
        let known = Header::new(Bdf::new(0, 2, 0), VENDOR_INTEL, 0x46d0)
            .class(CLASS_DISPLAY, SUBCLASS_VGA)
            .bars([0x0000_000c, 0x0000_0060, 0x9000_0008, 0, 0, 0]);
        let bus = FakeBus::new(alloc::vec![known]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 2, 0)).unwrap();
        assert!(matches!(identify(&info), Identity::Known(_)));
        assert_eq!(identify(&info).policy(), Policy::Identify);

        // An Intel device of another class, and a display device from another
        // vendor, are both simply not the thing being looked for.
        let ethernet = Header::new(Bdf::new(0, 0x1f, 6), VENDOR_INTEL, 0x15f3).class(0x02, 0x00);
        let bus = FakeBus::new(alloc::vec![ethernet]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 0x1f, 6)).unwrap();
        assert_eq!(identify(&info), Identity::NotDisplay);
        assert_eq!(identify(&info).policy(), Policy::Ignore);

        let other_vendor =
            Header::new(Bdf::new(0, 1, 0), 0x1234, 0x1111).class(CLASS_DISPLAY, SUBCLASS_VGA);
        let bus = FakeBus::new(alloc::vec![other_vendor]);
        let info = DeviceInfo::read(&bus, Bdf::new(0, 1, 0)).unwrap();
        assert_eq!(identify(&info), Identity::NotDisplay);
    }

    #[test]
    fn identity_description_states_what_a_log_reader_needs() {
        let known = DEVICES.iter().find(|d| d.device_id == 0x46d0).unwrap();
        let text = Identity::Known(known).describe(0x04, 0x46d0);
        assert!(text.contains("Alder Lake-N"), "{text}");
        assert!(text.contains("Gen12"), "{text}");
        // The display engine version is not something public documentation
        // states for this part, and the report says so rather than guessing.
        assert!(text.contains("display version not established"), "{text}");
        // No stepping table is published for this part, so the revision is
        // reported as an unknown stepping rather than silently called A0.
        assert!(text.contains("stepping unknown"), "{text}");
        let unknown = Identity::Unmodelled.describe(0x01, 0x9abc);
        assert!(unknown.contains("unmodelled"), "{unknown}");
        assert!(unknown.contains("0x9abc"), "{unknown}");
    }
}
