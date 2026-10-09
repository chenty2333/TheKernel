//! Intel VT-d CPU/northbridge quirk matching translated from FreeBSD
//! sys/x86/iommu/intel_quirks.c (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013, 2015 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. PCI config and CPUID discovery are platform inputs;
//! complete license in LICENSES/BSD-2-Clause.txt.
use core::sync::atomic::AtomicU32;

use crate::{
    Error,
    dmar::{DMAR_BARRIER_USEQ, DmarUnit},
    reg::*,
    utils::{dmar_barrier_enter, dmar_barrier_exit},
};

const QUIRK_NB_ALL_REV: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NorthbridgeIdentity {
    pub device_id: u32,
    pub revision: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuIdentity {
    pub eax_leaf1: u32,
}
impl CpuIdentity {
    fn fields(self) -> (u32, u32, u32, u32, u32) {
        let eax = self.eax_leaf1;
        (
            (eax >> 20) & 0xff, // CPUID_EXT_FAMILY
            (eax >> 16) & 0x0f, // CPUID_EXT_MODEL
            (eax >> 8) & 0x0f,  // CPUID_FAMILY
            (eax >> 4) & 0x0f,  // CPUID_MODEL
            eax & 0x0f,         // CPUID_STEPPING
        )
    }
}

#[derive(Clone, Copy)]
struct NorthbridgeQuirk {
    device_id: u32,
    revision: u32,
    apply: fn(&mut DmarUnit, u32),
    description: &'static str,
}
#[derive(Clone, Copy)]
struct CpuQuirk {
    ext_family: u32,
    ext_model: u32,
    family_code: u32,
    model: u32,
    stepping: Option<u32>,
    apply: fn(&mut DmarUnit),
    description: &'static str,
}

// upstream: intel_quirks.c nb_5400_no_low_high_prot_mem()
fn nb_5400_no_low_high_prot_mem(unit: &mut DmarUnit, _: u32) {
    unit.hw_cap &= !(DMAR_CAP_PHMR | DMAR_CAP_PLMR);
}
// upstream: intel_quirks.c nb_no_ir()
fn nb_no_ir(unit: &mut DmarUnit, _: u32) {
    unit.hw_ecap &= !(DMAR_ECAP_IR | DMAR_ECAP_EIM);
}
// upstream: intel_quirks.c nb_5500_no_ir_rev13()
fn nb_5500_no_ir_rev13(unit: &mut DmarUnit, revision: u32) {
    if revision <= 0x13 {
        nb_no_ir(unit, revision);
    }
}
/// upstream: intel_quirks.c cpu_e5_am9()
fn cpu_e5_am9(unit: &mut DmarUnit) {
    unit.hw_cap &= !(0x3f_u64 << 48);
    unit.hw_cap |= 9_u64 << 48;
}

const PRE_USE_NB: [NorthbridgeQuirk; 6] = [
    NorthbridgeQuirk {
        device_id: 0x4001,
        revision: 0x20,
        apply: nb_5400_no_low_high_prot_mem,
        description: "5400 E23",
    },
    NorthbridgeQuirk {
        device_id: 0x4003,
        revision: 0x20,
        apply: nb_5400_no_low_high_prot_mem,
        description: "5400 E23",
    },
    NorthbridgeQuirk {
        device_id: 0x3403,
        revision: QUIRK_NB_ALL_REV,
        apply: nb_5500_no_ir_rev13,
        description: "5500 E47, E53",
    },
    NorthbridgeQuirk {
        device_id: 0x3405,
        revision: QUIRK_NB_ALL_REV,
        apply: nb_5500_no_ir_rev13,
        description: "5500 E47, E53",
    },
    NorthbridgeQuirk {
        device_id: 0x3405,
        revision: 0x22,
        apply: nb_no_ir,
        description: "5500 E47, E53",
    },
    NorthbridgeQuirk {
        device_id: 0x3406,
        revision: QUIRK_NB_ALL_REV,
        apply: nb_5500_no_ir_rev13,
        description: "5500 E47, E53",
    },
];
const POST_IDENT_CPU: [CpuQuirk; 1] = [CpuQuirk {
    ext_family: 0,
    ext_model: 2,
    family_code: 6,
    model: 13,
    stepping: Some(6),
    apply: cpu_e5_am9,
    description: "E5 BT176",
}];

/// Generic exact match pass over the selected northbridge/CPU signatures.
/// upstream: intel_quirks.c dmar_match_quirks()
fn dmar_match_quirks(
    unit: &mut DmarUnit,
    northbridge: Option<NorthbridgeIdentity>,
    cpu: Option<CpuIdentity>,
    northbridge_quirks: &[NorthbridgeQuirk],
    cpu_quirks: &[CpuQuirk],
    mut report: impl FnMut(&'static str),
) {
    if let Some(nb) = northbridge {
        for quirk in northbridge_quirks {
            if quirk.device_id == nb.device_id
                && (quirk.revision == nb.revision || quirk.revision == QUIRK_NB_ALL_REV)
            {
                (quirk.apply)(unit, nb.revision);
                report(quirk.description);
            }
        }
    } else if !northbridge_quirks.is_empty() {
        report("cannot find northbridge");
    }
    if let Some(cpu) = cpu {
        let (ext_family, ext_model, family_code, model, stepping) = cpu.fields();
        for quirk in cpu_quirks {
            if quirk.ext_family == ext_family
                && quirk.ext_model == ext_model
                && quirk.family_code == family_code
                && quirk.model == model
                && quirk.stepping.is_none_or(|expected| expected == stepping)
            {
                (quirk.apply)(unit);
                report(quirk.description);
            }
        }
    }
}

/// One-time pre-use northbridge quirk pass under the RMRR/use-sequence barrier.
/// upstream: intel_quirks.c dmar_quirks_pre_use()
pub fn dmar_quirks_pre_use(
    unit: &mut DmarUnit,
    barrier_flags: &AtomicU32,
    northbridge: Option<NorthbridgeIdentity>,
    mut report: impl FnMut(&'static str),
) -> Result<bool, Error> {
    if !dmar_barrier_enter(barrier_flags, DMAR_BARRIER_USEQ)? {
        return Ok(false);
    }
    dmar_match_quirks(unit, northbridge, None, &PRE_USE_NB, &[], &mut report);
    dmar_barrier_exit(barrier_flags, DMAR_BARRIER_USEQ)?;
    Ok(true)
}

/// Post-identification CPU quirk pass.
/// upstream: intel_quirks.c dmar_quirks_post_ident()
pub fn dmar_quirks_post_ident(
    unit: &mut DmarUnit,
    cpu: CpuIdentity,
    mut report: impl FnMut(&'static str),
) {
    dmar_match_quirks(unit, None, Some(cpu), &[], &POST_IDENT_CPU, &mut report);
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn northbridge_capability_and_revision_workarounds_match_table() {
        let mut unit = DmarUnit {
            hw_cap: DMAR_CAP_PHMR | DMAR_CAP_PLMR,
            hw_ecap: DMAR_ECAP_IR | DMAR_ECAP_EIM,
            ..Default::default()
        };
        let flags = AtomicU32::new(0);
        let mut reports = Vec::new();
        assert!(
            dmar_quirks_pre_use(
                &mut unit,
                &flags,
                Some(NorthbridgeIdentity {
                    device_id: 0x4001,
                    revision: 0x20
                }),
                |d| reports.push(d)
            )
            .unwrap()
        );
        assert_eq!(unit.hw_cap & (DMAR_CAP_PHMR | DMAR_CAP_PLMR), 0);
        assert_eq!(reports, ["5400 E23"]);
        assert!(!dmar_quirks_pre_use(&mut unit, &flags, None, |_| {}).unwrap());

        let flags = AtomicU32::new(0);
        let mut rev22 = DmarUnit {
            hw_ecap: DMAR_ECAP_IR | DMAR_ECAP_EIM,
            ..Default::default()
        };
        dmar_quirks_pre_use(
            &mut rev22,
            &flags,
            Some(NorthbridgeIdentity {
                device_id: 0x3405,
                revision: 0x22,
            }),
            |_| {},
        )
        .unwrap();
        assert_eq!(rev22.hw_ecap & (DMAR_ECAP_IR | DMAR_ECAP_EIM), 0);

        let flags = AtomicU32::new(0);
        let mut rev14 = DmarUnit {
            hw_ecap: DMAR_ECAP_IR | DMAR_ECAP_EIM,
            ..Default::default()
        };
        dmar_quirks_pre_use(
            &mut rev14,
            &flags,
            Some(NorthbridgeIdentity {
                device_id: 0x3403,
                revision: 0x14,
            }),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            rev14.hw_ecap & (DMAR_ECAP_IR | DMAR_ECAP_EIM),
            DMAR_ECAP_IR | DMAR_ECAP_EIM
        );
    }

    #[test]
    fn cpu_signature_quirk_caps_mamv_at_nine() {
        let eax = (2 << 16) | (6 << 8) | (13 << 4) | 6;
        let mut unit = DmarUnit {
            hw_cap: 63 << 48,
            ..Default::default()
        };
        let mut found = false;
        dmar_quirks_post_ident(&mut unit, CpuIdentity { eax_leaf1: eax }, |description| {
            found |= description == "E5 BT176"
        });
        assert_eq!(DMAR_CAP_MAMV(unit.hw_cap), 9);
        assert!(found);
    }
}
