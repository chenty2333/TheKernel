use crate::{
    bringup::Tco,
    ids::{Version, identify},
    probe,
    regs::*,
    *,
};

#[derive(Default)]
struct Fake {
    words: [u16; 16],
    gcs: u32,
    smi: u32,
    writes: std::vec::Vec<(u16, u16)>,
    locked: bool,
    gcs_locked: bool,
    smi_locked: bool,
}

impl Bus for Fake {
    fn read16(&mut self, offset: u16) -> u16 {
        self.words[offset as usize / 2]
    }

    fn write16(&mut self, offset: u16, value: u16) {
        self.writes.push((offset, value));
        if self.locked {
            return;
        }
        let word = &mut self.words[offset as usize / 2];
        if matches!(offset, STATUS1 | STATUS2) {
            // Both status registers are write-one-to-clear.
            *word &= !value;
        } else {
            *word = value;
        }
    }

    fn read_no_reboot(&mut self) -> u32 {
        self.gcs
    }

    fn write_no_reboot(&mut self, value: u32) {
        if !self.gcs_locked {
            self.gcs = value;
        }
    }

    fn read_smi_enable(&mut self) -> u32 {
        self.smi
    }

    fn write_smi_enable(&mut self, value: u32) {
        if !self.smi_locked {
            self.smi = value;
        }
    }
}

fn halted_tco(version: Version, bus: Fake) -> Tco<Fake> {
    let mut bus = bus;
    bus.words[CONTROL1 as usize / 2] |= HALT;
    Tco::new(bus, version)
}

#[test]
fn explicit_platforms_and_resources_only() {
    assert_eq!(identify(0x8086, 0x54a3), Some(Version::CnlV6));
    assert_eq!(identify(0x8086, 0x2918), Some(Version::Ich9V2));
    assert_eq!(identify(0x8086, 0x1234), None);
    assert!(probe::resources(Version::CnlV6, 0x400, 0).is_err());
    assert_eq!(
        probe::resources(Version::CnlV6, 0x401, 0x100).unwrap().tco,
        0x400
    );
    assert_eq!(
        probe::resources(Version::Ich9V2, 0x601, 0xfed1c001)
            .unwrap()
            .gcs,
        Some(0xfed1f410)
    );
    assert_eq!(probe::timeout("quiet").unwrap(), None);
    assert_eq!(probe::timeout("watchdog.timeout=60").unwrap(), Some(60));
    assert!(probe::timeout("watchdog.timeout=0").is_err());
}

#[test]
fn layouts_preserve_fields_and_v6_never_uses_pmc_or_nmi_now() {
    for version in [Version::Ich9V2, Version::CnlV6] {
        let mut bus = Fake {
            gcs: 0x1234_0020,
            smi: u32::MAX,
            ..Fake::default()
        };
        bus.words[CONTROL1 as usize / 2] = NMI_NOW | 1 | HALT;
        bus.words[TIMER as usize / 2] = 0xa800;
        bus.words[STATUS2 as usize / 2] = BOOT_STATUS;
        let mut tco = Tco::new(bus, version);
        assert_eq!(tco.start(60), Ok(()));
        assert_eq!(tco.bus.words[TIMER as usize / 2], 0xa864);
        assert!(tco.running);
        assert_eq!(tco.boot_status, version == Version::Ich9V2);
        assert!(
            tco.bus
                .writes
                .iter()
                .all(|(offset, value)| *offset != CONTROL1 || value & NMI_NOW == 0)
        );
        if version == Version::CnlV6 {
            assert_eq!(tco.bus.gcs, 0x1234_0020);
            assert_eq!(tco.bus.smi, u32::MAX);
        } else {
            assert_eq!(tco.bus.gcs, 0x1234_0000);
            assert_eq!(tco.bus.smi, !(1u32 << 13));
        }
        assert_eq!(tco.set_timeout(2), Err(Error::Invalid));
        assert_eq!(tco.stop(), Ok(()));
        assert!(!tco.running);
        assert_ne!(tco.bus.words[CONTROL1 as usize / 2] & HALT, 0);
    }
}

#[test]
fn ignored_smi_clear_is_detected_before_timer_reconfiguration() {
    let mut bus = Fake {
        smi: 1 << 13,
        smi_locked: true,
        ..Fake::default()
    };
    bus.words[TIMER as usize / 2] = 0x400;
    let mut tco = halted_tco(Version::Ich9V2, bus);

    assert_eq!(tco.start(60), Err(Error::Locked));
    assert!(!tco.running);
    assert!(!tco.available());
    assert_eq!(tco.bus.words[TIMER as usize / 2], 0x400);
    assert_ne!(tco.bus.smi & (1 << 13), 0);
}

#[test]
fn passive_controller_rejects_control_writes_until_explicit_start() {
    let mut tco = Tco::new(Fake::default(), Version::Ich9V2);
    assert_eq!(tco.enable(), Err(Error::BadState));
    assert_eq!(tco.stop(), Err(Error::BadState));
    assert_eq!(tco.ping(), Err(Error::BadState));
    assert_eq!(tco.set_timeout(60), Err(Error::BadState));
    assert!(tco.bus.writes.is_empty());
}

#[test]
fn failed_adoption_retains_the_live_timer_for_ping_and_stop() {
    let mut bus = Fake {
        smi: 1 << 13,
        smi_locked: true,
        ..Fake::default()
    };
    bus.words[CONTROL1 as usize / 2] = 0;
    let mut tco = Tco::new(bus, Version::Ich9V2);

    assert_eq!(tco.adopt_running(60), Err(Error::Locked));
    assert!(tco.running);
    assert!(tco.available());
    // /dev/watchdog open calls enable first; it must not block access to an
    // already-running timer retained after a failed initialization.
    assert_eq!(tco.enable(), Ok(()));
    assert_eq!(tco.ping(), Ok(()));
    assert_eq!(tco.set_timeout(60), Err(Error::BadState));
    assert_eq!(tco.stop(), Ok(()));
    assert!(!tco.running);
    assert_eq!(tco.enable(), Err(Error::BadState));
}

#[test]
fn stop_retry_preserves_ownership_after_halt_succeeds() {
    let mut tco = halted_tco(Version::Ich9V2, Fake::default());
    assert_eq!(tco.start(60), Ok(()));

    assert_eq!(tco.stop(), Ok(()));
    assert_eq!(tco.set_timeout(120), Ok(()));
    assert_eq!(tco.timeout, 120);
    assert_eq!(tco.enable(), Ok(()));
    assert!(tco.running);

    tco.bus.gcs_locked = true;
    assert_eq!(tco.stop(), Err(Error::Locked));
    assert!(!tco.running);
    assert!(tco.available());

    tco.bus.gcs_locked = false;
    assert_eq!(tco.stop(), Ok(()));
    assert!(!tco.running);
}

#[test]
fn failed_takeover_retains_a_verified_running_watchdog() {
    let mut bus = Fake {
        locked: true,
        ..Fake::default()
    };
    bus.words[CONTROL1 as usize / 2] = 0;
    let mut tco = Tco::new(bus, Version::CnlV6);

    assert_eq!(tco.start(60), Err(Error::Locked));
    assert!(tco.running);
    assert!(tco.available());
    assert_eq!(tco.enable(), Ok(()));
    assert_eq!(tco.ping(), Ok(()));
    assert_eq!(tco.stop(), Err(Error::Locked));
}
