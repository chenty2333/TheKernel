//! CNVi CRF/CNV identity and BZ stepping detection from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230,
//! `sys/dev/pci/if_iwxreg.h`, and `if_iwxvar.h` (ISC). Copyright (c) 2014,
//! 2016 genua gmbh <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.;
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CsrAccess, DeviceFamily, IwxRegisters};

const WFPM_CTRL_REG: u32 = 0x00a0_3030;
const WFPM_AUX_IF_MAC_OWNER: u32 = 0x0800_0000;
const CNVI_AUX_MISC_CHIP: u32 = 0x00a2_00b0;
const CNVI_AUX_MISC_CHIP_MAC_STEP_MASK: u32 = 0x0f00_0000;
const CNVI_AUX_MISC_CHIP_PRODUCT_MASK: u32 = 0x0000_0fff;
const CNVI_PRODUCT_BZ_U: u32 = 0x930;
const SD_REG_VER: u32 = 0x00a2_9600;
const SD_REG_VER_GEN2: u32 = 0x00a2_b800;
const CSR_HW_REV_TYPE_MASK: u32 = 0x0000_fff0;
const MAC_TYPE_BZ: u16 = 0x46;
const MAC_TYPE_BZ_W: u16 = 0x4b;
const SILICON_A_STEP: u8 = 0;
const SILICON_B_STEP: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrfIdentity {
    pub crf_id: u32,
    pub cnv_id: u32,
    pub hardware_revision: u32,
}

/// Read CRF/CNV PRPH IDs and apply BZ-family stepping rules.
// upstream: if_iwx.c iwx_get_crf_id()
pub fn read_crf_identity<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    hardware_revision: u32,
) -> CrfIdentity {
    let owner = registers.read_umac_prph_unlocked(WFPM_CTRL_REG) | WFPM_AUX_IF_MAC_OWNER;
    registers.write_umac_prph_unlocked(WFPM_CTRL_REG, owner);
    let crf_id = registers.read_prph_unlocked(if registers.family() >= DeviceFamily::Ax210 {
        SD_REG_VER_GEN2
    } else {
        SD_REG_VER
    });
    let cnv_id = registers.read_prph_unlocked(CNVI_AUX_MISC_CHIP);
    CrfIdentity {
        crf_id,
        cnv_id,
        hardware_revision: apply_bz_stepping(hardware_revision, cnv_id),
    }
}

fn apply_bz_stepping(hardware_revision: u32, cnv_id: u32) -> u32 {
    let mac_type = ((hardware_revision & CSR_HW_REV_TYPE_MASK) >> 4) as u16;
    let mut step = None;
    if mac_type == MAC_TYPE_BZ_W {
        step = Some(SILICON_B_STEP);
    }
    if mac_type == MAC_TYPE_BZ {
        let cnv_step = ((cnv_id & CNVI_AUX_MISC_CHIP_MAC_STEP_MASK) >> 24) as u8;
        step = Some(
            if cnv_id & CNVI_AUX_MISC_CHIP_PRODUCT_MASK == CNVI_PRODUCT_BZ_U
                && cnv_step == SILICON_A_STEP
            {
                SILICON_B_STEP
            } else {
                cnv_step
            },
        );
    }
    hardware_revision | u32::from(step.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bz_stepping_comes_from_cnv_and_bz_u_promotes_a_to_b() {
        let bz_type = u32::from(MAC_TYPE_BZ) << 4;
        assert_eq!(apply_bz_stepping(bz_type, (2 << 24) | 0x900), bz_type | 2);
        assert_eq!(apply_bz_stepping(bz_type, 0x930), bz_type | 1);
        let bzwa_type = u32::from(MAC_TYPE_BZ_W) << 4;
        assert_eq!(apply_bz_stepping(bzwa_type, 0), bzwa_type | 1);
        let ax211_type = u32::from(0x44u16) << 4;
        assert_eq!(apply_bz_stepping(ax211_type, 0), ax211_type);
    }
}
