//! The device table: which PCI ids this driver binds, and what each one is.
//!
//! # Where the facts come from
//!
//! Every id here is taken from Linux v6.12
//! `drivers/net/ethernet/intel/igc/` (tag `v6.12`, commit
//! `adc218676eef25575469234709c2d87185ca223a`), and cross-checked against this
//! host's `pci.ids` (the `hwdata` package).  The Linux symbols are named so a
//! reader can check each value:
//!
//! * the id constants are `igc_hw.h` `IGC_DEV_ID_*` (`igc_hw.h:19-34`);
//! * the set of ids the driver binds is `igc_main.c` `igc_pci_tbl`
//!   (`igc_main.c:49-68`, `PCI_VDEVICE(INTEL, ...)` with vendor `0x8086`);
//! * the i225/i226 split is `igc_base.c` `igc_is_device_id_i225` and
//!   `igc_is_device_id_i226` (`igc_base.c:397-424`).
//!
//! No code is copied from Linux: this table is a restatement of the facts in
//! this project's own shape.
//!
//! # Two places the sources do not say the same thing
//!
//! Both are recorded rather than resolved, because choosing one silently is
//! how a driver ends up binding a part it does not understand.
//!
//! 1. **The i225/i226 predicates do not cover the table.**  `igc_pci_tbl` has
//!    16 entries; `igc_is_device_id_i225` names 7 ids and `igc_is_device_id_i226`
//!    names 4.  That leaves five bound ids in neither predicate —
//!    `0x15F7` (`IGC_DEV_ID_I220_V`), `0x5503` (`IGC_DEV_ID_I226_LMVP`),
//!    `0x125E` (`IGC_DEV_ID_I221_V`), `0x125F` (`IGC_DEV_ID_I226_BLANK_NVM`)
//!    and `0x15FD` (`IGC_DEV_ID_I225_BLANK_NVM`) — so a family derived from
//!    that pair of functions is [`Family::Unclassified`] for a part whose own
//!    name says otherwise.  Nothing in this driver depends on the family, and
//!    the report prints it as `unclassified` rather than guessing.  What
//!    *does* cover all 16 is `igc_base.c` `igc_get_invariants_base`
//!    (`igc_base.c:192-215`), which maps every one of them to a single MAC
//!    type, `igc_i225`; the speed decode in `igc_mac.c`
//!    `igc_get_speed_and_duplex_copper` (`igc_mac.c:681-712`) is written
//!    against that type, not against the family.
//! 2. **`0x3101` has two names.**  Linux calls it `IGC_DEV_ID_I225_K2`; this
//!    host's `pci.ids` calls it `Killer E3100X 2.5 Gigabit Ethernet
//!    Controller`.  These are consistent — the E3100X is a rebranded
//!    controller of this family — but the table keeps both strings so the
//!    report can show what each source claims.
//!
//! Six ids in `igc_pci_tbl` are absent from `pci.ids` altogether (`0x15F7`,
//! `0x15F8`, `0x15FD`, `0x125E`, `0x125F`, `0x3100`); that is a gap in the
//! local database, not a disagreement, and it is recorded as
//! `pci_ids_name: None`.
//!
//! # What this table is not
//!
//! It is not a measurement.  No PCI configuration-space byte has ever been
//! read from the machine this driver is being written for, so "the machine has
//! one of these" is an assumption, and the probe built on this table exists to
//! make that assumption cheap to confirm or refute.

/// The PCI vendor id of every part in this table.
///
/// `igc_main.c` `igc_pci_tbl` matches with `PCI_VDEVICE(INTEL, ...)`, and
/// `pci.ids` line 31401 gives Intel the vendor id `8086`.
pub const INTEL_VENDOR: u16 = 0x8086;

/// Which family a bound id belongs to.
///
/// The split is `igc_base.c` `igc_is_device_id_i225` / `igc_is_device_id_i226`
/// and nothing else.  See the module documentation: those two predicates do
/// not partition the table, so [`Family::Unclassified`] is a real answer and
/// not an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// `igc_is_device_id_i225` returns true for this id.
    I225,
    /// `igc_is_device_id_i226` returns true for this id.
    I226,
    /// The driver binds this id but neither predicate names it.
    Unclassified,
}

impl Family {
    /// The word the report uses for this family.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::I225 => "i225",
            Self::I226 => "i226",
            Self::Unclassified => "unclassified",
        }
    }
}

/// One PCI device id this driver binds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceId {
    /// The PCI device id (`igc_hw.h` `IGC_DEV_ID_*`).
    pub id: u16,
    /// The Linux symbol that defines it, so the value can be checked.
    pub linux_symbol: &'static str,
    /// The family, from the two `igc_base.c` predicates.
    pub family: Family,
    /// What this host's `pci.ids` calls the id, when it lists it at all.
    pub pci_ids_name: Option<&'static str>,
}

impl DeviceId {
    /// Whether this part's NVM is expected to be blank.
    ///
    /// `IGC_DEV_ID_I225_BLANK_NVM`/`IGC_DEV_ID_I226_BLANK_NVM` are real
    /// orderable parts whose flash has not been programmed: `igc_main.c`
    /// binds them like any other, and the MAC address comes from whatever the
    /// NVM auto-read leaves in the receive-address registers.  This driver
    /// does not validate the NVM checksum (`igc_nvm.c`
    /// `igc_validate_nvm_checksum`) and says so, which means a blank-NVM part
    /// is reported rather than driven if its receive address is not a valid
    /// unicast address.
    pub const fn is_blank_nvm(self) -> bool {
        matches!(self.id, 0x15FD | 0x125F)
    }
}

/// Every PCI device id this driver binds, in `igc_pci_tbl` order.
pub const DEVICES: &[DeviceId] = &[
    DeviceId {
        id: 0x15F2,
        linux_symbol: "IGC_DEV_ID_I225_LM",
        family: Family::I225,
        pci_ids_name: Some("Ethernet Controller I225-LM"),
    },
    DeviceId {
        id: 0x15F3,
        linux_symbol: "IGC_DEV_ID_I225_V",
        family: Family::I225,
        pci_ids_name: Some("Ethernet Controller I225-V"),
    },
    DeviceId {
        id: 0x15F8,
        linux_symbol: "IGC_DEV_ID_I225_I",
        family: Family::I225,
        pci_ids_name: None,
    },
    DeviceId {
        id: 0x15F7,
        linux_symbol: "IGC_DEV_ID_I220_V",
        family: Family::Unclassified,
        pci_ids_name: None,
    },
    DeviceId {
        id: 0x3100,
        linux_symbol: "IGC_DEV_ID_I225_K",
        family: Family::I225,
        pci_ids_name: None,
    },
    DeviceId {
        id: 0x3101,
        linux_symbol: "IGC_DEV_ID_I225_K2",
        family: Family::I225,
        pci_ids_name: Some("Killer E3100X 2.5 Gigabit Ethernet Controller"),
    },
    DeviceId {
        id: 0x3102,
        linux_symbol: "IGC_DEV_ID_I226_K",
        family: Family::I226,
        pci_ids_name: Some("Ethernet Controller I226-K"),
    },
    DeviceId {
        id: 0x5502,
        linux_symbol: "IGC_DEV_ID_I225_LMVP",
        family: Family::I225,
        pci_ids_name: Some("Ethernet Controller (2) I225-LMvP"),
    },
    DeviceId {
        id: 0x5503,
        linux_symbol: "IGC_DEV_ID_I226_LMVP",
        family: Family::Unclassified,
        pci_ids_name: Some("Ethernet Controller I226-LMvP"),
    },
    DeviceId {
        id: 0x0D9F,
        linux_symbol: "IGC_DEV_ID_I225_IT",
        family: Family::I225,
        pci_ids_name: Some("Ethernet Controller I225-IT"),
    },
    DeviceId {
        id: 0x125B,
        linux_symbol: "IGC_DEV_ID_I226_LM",
        family: Family::I226,
        pci_ids_name: Some("Ethernet Controller I226-LM"),
    },
    DeviceId {
        id: 0x125C,
        linux_symbol: "IGC_DEV_ID_I226_V",
        family: Family::I226,
        pci_ids_name: Some("Ethernet Controller I226-V"),
    },
    DeviceId {
        id: 0x125D,
        linux_symbol: "IGC_DEV_ID_I226_IT",
        family: Family::I226,
        pci_ids_name: Some("Ethernet Controller I226-IT"),
    },
    DeviceId {
        id: 0x125E,
        linux_symbol: "IGC_DEV_ID_I221_V",
        family: Family::Unclassified,
        pci_ids_name: None,
    },
    DeviceId {
        id: 0x125F,
        linux_symbol: "IGC_DEV_ID_I226_BLANK_NVM",
        family: Family::Unclassified,
        pci_ids_name: None,
    },
    DeviceId {
        id: 0x15FD,
        linux_symbol: "IGC_DEV_ID_I225_BLANK_NVM",
        family: Family::Unclassified,
        pci_ids_name: None,
    },
];

/// The [`DeviceId`] for a `(vendor, device)` pair, or `None`.
///
/// The vendor must be [`INTEL_VENDOR`]: a device id is only meaningful with
/// its vendor, and `0x15F3` under another vendor is another company's part.
pub fn identify(vendor: u16, device: u16) -> Option<&'static DeviceId> {
    if vendor != INTEL_VENDOR {
        return None;
    }
    DEVICES.iter().find(|entry| entry.id == device)
}

/// The two ids this workstream's target machine is *assumed* to carry.
///
/// These are the ids the assumption in the brief names — an i225-V or i226-V
/// on an Alder Lake-N mini-PC — and they are here so a test can assert that
/// the assumption is at least expressible in the table.  Nothing in the driver
/// treats them specially.
pub const ASSUMED_TARGET_IDS: [u16; 2] = [0x15F3, 0x125C];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_every_id_igc_pci_tbl_binds() {
        // 16 entries: igc_main.c:49-68 lists 16 ids before the null terminator.
        assert_eq!(DEVICES.len(), 16, "{DEVICES:#?}");
        for (index, entry) in DEVICES.iter().enumerate() {
            for other in &DEVICES[index + 1..] {
                assert_ne!(entry.id, other.id, "duplicate id in the table");
            }
        }
    }

    #[test]
    fn the_table_carries_the_ids_the_brief_assumes() {
        for id in ASSUMED_TARGET_IDS {
            let entry = identify(INTEL_VENDOR, id).expect("assumed id must be in the table");
            assert!(
                matches!(entry.family, Family::I225 | Family::I226),
                "{id:#06x} should be classified by one of the igc_base.c predicates",
            );
        }
        assert_eq!(
            identify(INTEL_VENDOR, 0x15F3).unwrap().linux_symbol,
            "IGC_DEV_ID_I225_V"
        );
        assert_eq!(
            identify(INTEL_VENDOR, 0x125C).unwrap().linux_symbol,
            "IGC_DEV_ID_I226_V"
        );
    }

    #[test]
    fn the_vendor_is_part_of_the_match() {
        assert!(identify(INTEL_VENDOR, 0x15F3).is_some());
        assert!(identify(0x1af4, 0x15F3).is_none(), "virtio vendor, same id");
        assert!(identify(0x0000, 0x15F3).is_none());
        assert!(identify(INTEL_VENDOR, 0x1533).is_none(), "an e1000e id");
        // 0x15F9/0x15FA are the 500 Series PCH GbE controller in this host's
        // pci.ids.  They are NOT in igc_pci_tbl, so this driver must not bind
        // them: another driver owns those parts.
        assert!(identify(INTEL_VENDOR, 0x15F9).is_none());
        assert!(identify(INTEL_VENDOR, 0x15FA).is_none());
    }

    #[test]
    fn the_family_comes_from_the_two_predicates_and_not_from_the_name() {
        // In igc_is_device_id_i225.
        assert_eq!(identify(INTEL_VENDOR, 0x15F2).unwrap().family, Family::I225);
        // In igc_is_device_id_i226.
        assert_eq!(identify(INTEL_VENDOR, 0x125C).unwrap().family, Family::I226);
        // Bound by igc_pci_tbl, named "I226-LMvP", in neither predicate.
        let lmvp = identify(INTEL_VENDOR, 0x5503).unwrap();
        assert_eq!(lmvp.family, Family::Unclassified);
        assert_eq!(lmvp.pci_ids_name, Some("Ethernet Controller I226-LMvP"));
        // The same, for the part pci.ids does not list at all.
        assert_eq!(
            identify(INTEL_VENDOR, 0x15F7).unwrap().family,
            Family::Unclassified
        );
    }

    #[test]
    fn exactly_the_five_uncovered_ids_are_unclassified() {
        // igc_pci_tbl has 16 entries; the predicates name 7 + 4.  If a future
        // Linux revision changes that, this test is where it shows up.
        let unclassified: alloc::vec::Vec<u16> = DEVICES
            .iter()
            .filter(|entry| entry.family == Family::Unclassified)
            .map(|entry| entry.id)
            .collect();
        assert_eq!(unclassified, [0x15F7, 0x5503, 0x125E, 0x125F, 0x15FD]);
    }

    #[test]
    fn the_blank_nvm_parts_are_marked_as_such() {
        assert!(identify(INTEL_VENDOR, 0x15FD).unwrap().is_blank_nvm());
        assert!(identify(INTEL_VENDOR, 0x125F).unwrap().is_blank_nvm());
        assert!(!identify(INTEL_VENDOR, 0x15F3).unwrap().is_blank_nvm());
        assert!(!identify(INTEL_VENDOR, 0x125E).unwrap().is_blank_nvm());
    }

    #[test]
    fn the_pci_ids_cross_check_records_which_ids_the_local_database_lacks() {
        // Six of the sixteen are absent from this host's pci.ids.  Recording
        // the absence is the point: it is why the probe's report prints the
        // Linux symbol as the identity and pci.ids only as corroboration.
        let absent: alloc::vec::Vec<u16> = DEVICES
            .iter()
            .filter(|entry| entry.pci_ids_name.is_none())
            .map(|entry| entry.id)
            .collect();
        assert_eq!(absent, [0x15F8, 0x15F7, 0x3100, 0x125E, 0x125F, 0x15FD]);
        // And the one id whose two sources give different names.
        assert_eq!(
            identify(INTEL_VENDOR, 0x3101).unwrap().pci_ids_name,
            Some("Killer E3100X 2.5 Gigabit Ethernet Controller")
        );
        assert_eq!(
            identify(INTEL_VENDOR, 0x3101).unwrap().linux_symbol,
            "IGC_DEV_ID_I225_K2"
        );
    }
}
