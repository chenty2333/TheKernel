// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_display_device.c:
// adl_p_steppings, adl_p_adl_n_steppings, get_pre_gmdid_step, xe_lpd_display.
// Copyright © 2023 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// PCI ID facts: include/drm/intel/pciids.h (Copyright 2013 Intel Corporation).
// Other platforms, Raptor Lake subplatforms, GMD_ID and runtime fuse reads omitted.
use crate::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    AlderLakeP,
    AlderLakeN,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    A0,
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
    pub fn identify(vendor: u16, id: u16, revision: u8) -> Result<Self, Error> {
        if vendor != 0x8086 {
            return Err(Error::UnsupportedDevice);
        }
        let platform = match id {
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
        let map: &[(u8, Step)] = match platform {
            Platform::AlderLakeP => &[(0, Step::A0), (4, Step::B0), (8, Step::C0), (12, Step::D0)],
            Platform::AlderLakeN => &[(0, Step::D0)],
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
    pub const DISPLAY_VERSION: u8 = 13;
    /// Default capabilities before runtime fuse removal. Not a present-port claim.
    pub const PORTS: [Port; 6] = [Port::A, Port::B, Port::Tc1, Port::Tc2, Port::Tc3, Port::Tc4];
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
}
impl Port {
    pub const fn is_tc(self) -> bool {
        matches!(self, Self::Tc1 | Self::Tc2 | Self::Tc3 | Self::Tc4)
    }
    pub const fn supported_on_adlp(self) -> bool {
        matches!(
            self,
            Self::A | Self::B | Self::Tc1 | Self::Tc2 | Self::Tc3 | Self::Tc4
        )
    }
}

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
}
