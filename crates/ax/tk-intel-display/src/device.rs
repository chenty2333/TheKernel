// SPDX-License-Identifier: MIT
// Linux v7.2.3 drivers/gpu/drm/i915/display/intel_display_device.c:
// TGL/RKL/ADL-S/ADL-P/ADL-N display descriptors and pre-GMD_ID stepping maps.
// Copyright © 2023 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// PCI ID facts: include/drm/intel/pciids.h (Copyright 2013 Intel Corporation).
// Raptor Lake subplatforms, GMD_ID and runtime fuse reads are outside display 12/13.
use crate::{Error, dmc::DmcPlatform};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    TigerLake,
    RocketLake,
    AlderLakeS,
    AlderLakeP,
    AlderLakeN,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    A0,
    A2,
    B0,
    C0,
    D0,
    Future,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Device {
    pub platform: Platform,
    pub step: Step,
    /// True only for an exact revision mapping, not i915's next/future fallback.
    /// Unknown revision cannot grant permission for stepping-sensitive writes.
    pub exact_step: bool,
}
impl Device {
    /// Select display-12/13 family from Linux's PCI ID groups and revision maps.
    // upstream: intel_display_device.c find_platform_desc(), get_pre_gmdid_step()
    pub fn identify(vendor: u16, id: u16, revision: u8) -> Result<Self, Error> {
        if vendor != 0x8086 {
            return Err(Error::UnsupportedDevice);
        }
        let platform = match id {
            0x9a40 | 0x9a49 | 0x9a59 | 0x9a78 | 0x9ac0 | 0x9ac9 | 0x9ad9 | 0x9af8 => {
                Platform::TigerLake
            }
            0x9a60 | 0x9a68 | 0x9a70 => Platform::TigerLake,
            0x4c80 | 0x4c8a | 0x4c8b | 0x4c8c | 0x4c90 | 0x4c9a => Platform::RocketLake,
            0x4680 | 0x4682 | 0x4688 | 0x468a | 0x468b | 0x4690 | 0x4692 | 0x4693 => {
                Platform::AlderLakeS
            }
            0x46d0..=0x46d4 => Platform::AlderLakeN,
            0x46a0..=0x46a3
            | 0x46a6
            | 0x46a8
            | 0x46aa
            | 0x462a
            | 0x4626
            | 0x4628
            | 0x46b0..=0x46b3
            | 0x46c0..=0x46c3 => Platform::AlderLakeP,
            _ => return Err(Error::UnsupportedDevice),
        };
        let map: &[(u8, Step)] = match (platform, id) {
            (
                Platform::TigerLake,
                0x9a40 | 0x9a49 | 0x9a59 | 0x9a78 | 0x9ac0 | 0x9ac9 | 0x9ad9 | 0x9af8,
            ) => &[(0, Step::A0), (1, Step::C0), (2, Step::C0), (3, Step::D0)],
            (Platform::TigerLake, _) => &[(0, Step::B0), (1, Step::D0)],
            (Platform::RocketLake, _) => &[(0, Step::A0), (1, Step::B0), (4, Step::C0)],
            (Platform::AlderLakeS, _) => &[
                (0, Step::A0),
                (1, Step::A2),
                (4, Step::B0),
                (8, Step::B0),
                (12, Step::C0),
            ],
            (Platform::AlderLakeP, _) => {
                &[(0, Step::A0), (4, Step::B0), (8, Step::C0), (12, Step::D0)]
            }
            (Platform::AlderLakeN, _) => &[(0, Step::D0)],
        };
        let (step, exact_step) = map
            .iter()
            .find(|(r, _)| *r >= revision)
            .map(|(r, s)| (*s, *r == revision))
            .unwrap_or((Step::Future, false));
        Ok(Self {
            platform,
            step,
            exact_step,
        })
    }

    pub const fn display_version(self) -> u8 {
        match self.platform {
            Platform::TigerLake | Platform::RocketLake | Platform::AlderLakeS => 12,
            Platform::AlderLakeP | Platform::AlderLakeN => 13,
        }
    }

    pub const fn dmc_platform(self) -> DmcPlatform {
        match self.platform {
            Platform::TigerLake => DmcPlatform::TigerLake,
            Platform::RocketLake => DmcPlatform::RocketLake,
            Platform::AlderLakeS => DmcPlatform::AlderLakeS,
            Platform::AlderLakeP => DmcPlatform::AlderLakeP,
            Platform::AlderLakeN => DmcPlatform::AlderLakeN,
        }
    }

    /// Default display ports before runtime fuse removal; not a present-port claim.
    pub const fn ports(self) -> &'static [Port] {
        match self.platform {
            Platform::TigerLake => &TGL_PORTS,
            Platform::RocketLake => &RKL_PORTS,
            Platform::AlderLakeS => &ADLS_PORTS,
            Platform::AlderLakeP | Platform::AlderLakeN => &ADLP_PORTS,
        }
    }

    pub const DISPLAY_VERSION: u8 = 13;
    /// The XELPD defaults retained for existing ADL-P/N callers.
    pub const PORTS: [Port; 6] = ADLP_PORTS;
}

/// XELPD VBT names C/D/E are not present in the ADL-P/N runtime port mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Port {
    A,
    B,
    C,
    D,
    E,
    F,
    Tc1,
    Tc2,
    Tc3,
    Tc4,
    Tc5,
    Tc6,
}
impl Port {
    pub const fn is_tc(self) -> bool {
        matches!(
            self,
            Self::Tc1 | Self::Tc2 | Self::Tc3 | Self::Tc4 | Self::Tc5 | Self::Tc6
        )
    }
    pub const fn supported_on_adlp(self) -> bool {
        matches!(
            self,
            Self::A | Self::B | Self::Tc1 | Self::Tc2 | Self::Tc3 | Self::Tc4
        )
    }
}

const TGL_PORTS: [Port; 8] = [
    Port::A,
    Port::B,
    Port::Tc1,
    Port::Tc2,
    Port::Tc3,
    Port::Tc4,
    Port::Tc5,
    Port::Tc6,
];
const RKL_PORTS: [Port; 4] = [Port::A, Port::B, Port::Tc1, Port::Tc2];
const ADLS_PORTS: [Port; 5] = [Port::A, Port::Tc1, Port::Tc2, Port::Tc3, Port::Tc4];
const ADLP_PORTS: [Port; 6] = [Port::A, Port::B, Port::Tc1, Port::Tc2, Port::Tc3, Port::Tc4];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn n305_is_d0_not_adlp_a0() {
        let d = Device::identify(0x8086, 0x46d0, 0).unwrap();
        assert_eq!(
            (d.platform, d.step, d.exact_step),
            (Platform::AlderLakeN, Step::D0, true)
        );
        assert_eq!(
            Device::identify(0x8086, 0x46d0, 1).unwrap().step,
            Step::Future
        );
        assert!(!Device::identify(0x8086, 0x46d0, 1).unwrap().exact_step);
    }
    #[test]
    fn gaps_use_next_step_but_do_not_admit_writes() {
        assert_eq!(
            Device::identify(0x8086, 0x46a0, 1).unwrap(),
            Device {
                platform: Platform::AlderLakeP,
                step: Step::B0,
                exact_step: false
            }
        );
        assert!(Device::identify(0x8086, 0x46a0, 4).unwrap().exact_step);
        assert!(Device::identify(0x8086, 0xa7a0, 0).is_err());
        assert!(Device::identify(0x1234, 0x46d0, 0).is_err());
    }

    #[test]
    fn display12_platform_ids_select_their_version_dmc_and_ports() {
        let tgl = Device::identify(0x8086, 0x9a40, 0).unwrap();
        assert_eq!(tgl.platform, Platform::TigerLake);
        assert_eq!(tgl.step, Step::A0);
        assert_eq!(tgl.display_version(), 12);
        assert_eq!(tgl.dmc_platform(), DmcPlatform::TigerLake);
        assert!(tgl.ports().contains(&Port::Tc6));

        let rkl = Device::identify(0x8086, 0x4c80, 4).unwrap();
        assert_eq!(rkl.step, Step::C0);
        assert_eq!(rkl.dmc_platform(), DmcPlatform::RocketLake);
        assert_eq!(rkl.ports(), &RKL_PORTS);

        let adls = Device::identify(0x8086, 0x4680, 1).unwrap();
        assert_eq!(adls.step, Step::A2);
        assert_eq!(adls.dmc_platform(), DmcPlatform::AlderLakeS);
        assert_eq!(adls.display_version(), 12);
    }
}
