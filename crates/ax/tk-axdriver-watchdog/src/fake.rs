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
}
impl Bus for Fake {
    fn read16(&mut self, o: u16) -> u16 {
        self.words[o as usize / 2]
    }
    fn write16(&mut self, o: u16, v: u16) {
        self.writes.push((o, v));
        if !self.locked {
            self.words[o as usize / 2] = v;
        }
    }
    fn read_no_reboot(&mut self) -> u32 {
        self.gcs
    }
    fn write_no_reboot(&mut self, v: u32) {
        if !self.locked {
            self.gcs = v;
        }
    }
    fn read_smi_enable(&mut self) -> u32 {
        self.smi
    }
    fn write_smi_enable(&mut self, v: u32) {
        self.smi = v;
    }
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
            smi: 0xffff_ffff,
            ..Fake::default()
        };
        bus.words[4] = NMI_NOW | 1 | HALT;
        bus.words[9] = 0xa800;
        let mut tco = Tco::start(bus, version, 60).unwrap();
        assert_eq!(tco.bus.words[9], 0xa864);
        assert!(tco.running);
        assert!(
            tco.bus
                .writes
                .iter()
                .all(|(o, v)| *o != CONTROL1 || v & NMI_NOW == 0)
        );
        if version == Version::CnlV6 {
            assert_eq!(tco.bus.gcs, 0x1234_0020);
            assert_eq!(tco.bus.smi, u32::MAX);
        } else {
            assert_eq!(tco.bus.gcs, 0x1234_0000);
            assert_eq!(tco.bus.smi, u32::MAX & !(1 << 13));
        }
        assert!(tco.set_timeout(2).is_err());
        tco.stop().unwrap();
        assert!(!tco.running);
        assert!(tco.bus.words[4] & HALT != 0);
    }
}
#[test]
fn locked_control_refuses_to_claim_a_working_watchdog() {
    assert!(matches!(
        Tco::start(
            Fake {
                locked: true,
                ..Fake::default()
            },
            Version::CnlV6,
            60
        ),
        Err(Error::Locked)
    ));
}
