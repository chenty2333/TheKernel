//! Intel VT-d register encodings translated from FreeBSD `sys/x86/iommu/intel_reg.h`.
//! FreeBSD source snapshot 2026-10-08; BSD-2-Clause.
//! Copyright (c) 2013-2015 The FreeBSD Foundation; developed by Konstantin Belousov
//! under sponsorship from the FreeBSD Foundation. See `LICENSES/BSD-2-Clause.txt`.

#![allow(non_snake_case, non_upper_case_globals, unused_parens)]

pub const PAGE_SIZE: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RootEntry {
    pub r1: u64,
    pub r2: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextEntry {
    pub ctx1: u64,
    pub ctx2: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Irte {
    pub irte1: u64,
    pub irte2: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QiDescriptor {
    pub low: u64,
    pub high: u64,
}

// upstream: intel_reg.h DMAR_ROOT_R1_P
pub const DMAR_ROOT_R1_P: u64 = 1;
// upstream: intel_reg.h DMAR_ROOT_R1_CTP_MASK
pub const DMAR_ROOT_R1_CTP_MASK: u64 = 0xfffffffffffff000;
// upstream: intel_reg.h DMAR_CTX_CNT
pub const DMAR_CTX_CNT: usize = PAGE_SIZE / core::mem::size_of::<RootEntry>();
// upstream: intel_reg.h DMAR_CTX1_P
pub const DMAR_CTX1_P: u64 = 1;
// upstream: intel_reg.h DMAR_CTX1_FPD
pub const DMAR_CTX1_FPD: u64 = 2;
// upstream: intel_reg.h DMAR_CTX1_T_UNTR
pub const DMAR_CTX1_T_UNTR: u64 = 0;
// upstream: intel_reg.h DMAR_CTX1_T_TR
pub const DMAR_CTX1_T_TR: u64 = 4;
// upstream: intel_reg.h DMAR_CTX1_T_PASS
pub const DMAR_CTX1_T_PASS: u64 = 8;
// upstream: intel_reg.h DMAR_CTX1_ASR_MASK
pub const DMAR_CTX1_ASR_MASK: u64 = 0xfffffffffffff000;
// upstream: intel_reg.h DMAR_CTX2_AW_2LVL
pub const DMAR_CTX2_AW_2LVL: u64 = 0;
// upstream: intel_reg.h DMAR_CTX2_AW_3LVL
pub const DMAR_CTX2_AW_3LVL: u64 = 1;
// upstream: intel_reg.h DMAR_CTX2_AW_4LVL
pub const DMAR_CTX2_AW_4LVL: u64 = 2;
// upstream: intel_reg.h DMAR_CTX2_AW_5LVL
pub const DMAR_CTX2_AW_5LVL: u64 = 3;
// upstream: intel_reg.h DMAR_CTX2_AW_6LVL
pub const DMAR_CTX2_AW_6LVL: u64 = 4;
// upstream: intel_reg.h DMAR_CTX2_DID_MASK
pub const DMAR_CTX2_DID_MASK: u64 = 0xffff0;
// upstream: intel_reg.h DMAR_CTX2_DID
pub const fn DMAR_CTX2_DID(x: u64) -> u64 { ((x) << 8) }
// upstream: intel_reg.h DMAR_CTX2_GET_DID
pub const fn DMAR_CTX2_GET_DID(ctx2: u64) -> u64 { (((ctx2) & DMAR_CTX2_DID_MASK) >> 8) }
// upstream: intel_reg.h DMAR_PTE_R
pub const DMAR_PTE_R: u64 = 1;
// upstream: intel_reg.h DMAR_PTE_W
pub const DMAR_PTE_W: u64 = (1 << 1);
// upstream: intel_reg.h DMAR_PTE_SP
pub const DMAR_PTE_SP: u64 = (1 << 7);
// upstream: intel_reg.h DMAR_PTE_SNP
pub const DMAR_PTE_SNP: u64 = (1 << 11);
// upstream: intel_reg.h DMAR_PTE_ADDR_MASK
pub const DMAR_PTE_ADDR_MASK: u64 = 0xffffffffff000;
// upstream: intel_reg.h DMAR_PTE_TM
pub const DMAR_PTE_TM: u64 = (1 << 62);
// upstream: intel_reg.h DMAR_IRTE2_SVT_NONE
pub const DMAR_IRTE2_SVT_NONE: u64 = (0 << (82 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SVT_RID
pub const DMAR_IRTE2_SVT_RID: u64 = (1 << (82 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SVT_BUS
pub const DMAR_IRTE2_SVT_BUS: u64 = (2 << (82 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SQ_RID
pub const DMAR_IRTE2_SQ_RID: u64 = (0 << (80 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SQ_RID_N2
pub const DMAR_IRTE2_SQ_RID_N2: u64 = (1 << (80 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SQ_RID_N21
pub const DMAR_IRTE2_SQ_RID_N21: u64 = (2 << (80 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SQ_RID_N210
pub const DMAR_IRTE2_SQ_RID_N210: u64 = (3 << (80 - 64));
// upstream: intel_reg.h DMAR_IRTE2_SID_RID
pub const fn DMAR_IRTE2_SID_RID(x: u64) -> u64 { ((x)) }
// upstream: intel_reg.h DMAR_IRTE2_SID_BUS
pub const fn DMAR_IRTE2_SID_BUS(start: u64, end: u64) -> u64 { ((((start)) << 8) | (end)) }
// upstream: intel_reg.h DMAR_IRTE1_DST_xAPIC
pub const fn DMAR_IRTE1_DST_xAPIC(x: u64) -> u64 { (((x)) << 40) }
// upstream: intel_reg.h DMAR_IRTE1_DST_x2APIC
pub const fn DMAR_IRTE1_DST_x2APIC(x: u64) -> u64 { (((x)) << 32) }
// upstream: intel_reg.h DMAR_IRTE1_V
pub const fn DMAR_IRTE1_V(x: u64) -> u64 { ((x) << 16) }
// upstream: intel_reg.h DMAR_IRTE1_IM_POSTED
pub const DMAR_IRTE1_IM_POSTED: u64 = (1 << 15);
// upstream: intel_reg.h DMAR_IRTE1_DLM_FM
pub const DMAR_IRTE1_DLM_FM: u64 = (0 << 5);
// upstream: intel_reg.h DMAR_IRTE1_DLM_LP
pub const DMAR_IRTE1_DLM_LP: u64 = (1 << 5);
// upstream: intel_reg.h DMAR_IRTE1_DLM_SMI
pub const DMAR_IRTE1_DLM_SMI: u64 = (2 << 5);
// upstream: intel_reg.h DMAR_IRTE1_DLM_NMI
pub const DMAR_IRTE1_DLM_NMI: u64 = (4 << 5);
// upstream: intel_reg.h DMAR_IRTE1_DLM_INIT
pub const DMAR_IRTE1_DLM_INIT: u64 = (5 << 5);
// upstream: intel_reg.h DMAR_IRTE1_DLM_ExtINT
pub const DMAR_IRTE1_DLM_ExtINT: u64 = (7 << 5);
// upstream: intel_reg.h DMAR_IRTE1_TM_EDGE
pub const DMAR_IRTE1_TM_EDGE: u64 = (0 << 4);
// upstream: intel_reg.h DMAR_IRTE1_TM_LEVEL
pub const DMAR_IRTE1_TM_LEVEL: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_IRTE1_RH_DIRECT
pub const DMAR_IRTE1_RH_DIRECT: u64 = (0 << 3);
// upstream: intel_reg.h DMAR_IRTE1_RH_SELECT
pub const DMAR_IRTE1_RH_SELECT: u64 = (1 << 3);
// upstream: intel_reg.h DMAR_IRTE1_DM_PHYSICAL
pub const DMAR_IRTE1_DM_PHYSICAL: u64 = (0 << 2);
// upstream: intel_reg.h DMAR_IRTE1_DM_LOGICAL
pub const DMAR_IRTE1_DM_LOGICAL: u64 = (1 << 2);
// upstream: intel_reg.h DMAR_IRTE1_FPD
pub const DMAR_IRTE1_FPD: u64 = (1 << 1);
// upstream: intel_reg.h DMAR_IRTE1_P
pub const DMAR_IRTE1_P: u64 = (1);
// upstream: intel_reg.h DMAR_VER_REG
pub const DMAR_VER_REG: u64 = 0;
// upstream: intel_reg.h DMAR_MAJOR_VER
pub const fn DMAR_MAJOR_VER(x: u64) -> u64 { (((x) >> 4) & 0xf) }
// upstream: intel_reg.h DMAR_MINOR_VER
pub const fn DMAR_MINOR_VER(x: u64) -> u64 { ((x) & 0xf) }
// upstream: intel_reg.h DMAR_CAP_REG
pub const DMAR_CAP_REG: u64 = 0x8;
// upstream: intel_reg.h DMAR_CAP_PI
pub const DMAR_CAP_PI: u64 = (1 << 59);
// upstream: intel_reg.h DMAR_CAP_FL1GP
pub const DMAR_CAP_FL1GP: u64 = (1 << 56);
// upstream: intel_reg.h DMAR_CAP_DRD
pub const DMAR_CAP_DRD: u64 = (1 << 55);
// upstream: intel_reg.h DMAR_CAP_DWD
pub const DMAR_CAP_DWD: u64 = (1 << 54);
// upstream: intel_reg.h DMAR_CAP_MAMV
pub const fn DMAR_CAP_MAMV(x: u64) -> u64 { ((((x) >> 48) & 0x3f)) }
// upstream: intel_reg.h DMAR_CAP_NFR
pub const fn DMAR_CAP_NFR(x: u64) -> u64 { ((((x) >> 40) & 0xff) + 1) }
// upstream: intel_reg.h DMAR_CAP_PSI
pub const DMAR_CAP_PSI: u64 = (1 << 39);
// upstream: intel_reg.h DMAR_CAP_SPS
pub const fn DMAR_CAP_SPS(x: u64) -> u64 { ((((x) >> 34) & 0xf)) }
// upstream: intel_reg.h DMAR_CAP_SPS_2M
pub const DMAR_CAP_SPS_2M: u64 = 0x1;
// upstream: intel_reg.h DMAR_CAP_SPS_1G
pub const DMAR_CAP_SPS_1G: u64 = 0x2;
// upstream: intel_reg.h DMAR_CAP_SPS_512G
pub const DMAR_CAP_SPS_512G: u64 = 0x4;
// upstream: intel_reg.h DMAR_CAP_SPS_1T
pub const DMAR_CAP_SPS_1T: u64 = 0x8;
// upstream: intel_reg.h DMAR_CAP_FRO
pub const fn DMAR_CAP_FRO(x: u64) -> u64 { ((((x) >> 24) & 0x1ff)) }
// upstream: intel_reg.h DMAR_CAP_ISOCH
pub const DMAR_CAP_ISOCH: u64 = (1 << 23);
// upstream: intel_reg.h DMAR_CAP_ZLR
pub const DMAR_CAP_ZLR: u64 = (1 << 22);
// upstream: intel_reg.h DMAR_CAP_MGAW
pub const fn DMAR_CAP_MGAW(x: u64) -> u64 { ((((x) >> 16) & 0x3f)) }
// upstream: intel_reg.h DMAR_CAP_SAGAW
pub const fn DMAR_CAP_SAGAW(x: u64) -> u64 { ((((x) >> 8) & 0x1f)) }
// upstream: intel_reg.h DMAR_CAP_SAGAW_2LVL
pub const DMAR_CAP_SAGAW_2LVL: u64 = 0x01;
// upstream: intel_reg.h DMAR_CAP_SAGAW_3LVL
pub const DMAR_CAP_SAGAW_3LVL: u64 = 0x02;
// upstream: intel_reg.h DMAR_CAP_SAGAW_4LVL
pub const DMAR_CAP_SAGAW_4LVL: u64 = 0x04;
// upstream: intel_reg.h DMAR_CAP_SAGAW_5LVL
pub const DMAR_CAP_SAGAW_5LVL: u64 = 0x08;
// upstream: intel_reg.h DMAR_CAP_SAGAW_6LVL
pub const DMAR_CAP_SAGAW_6LVL: u64 = 0x10;
// upstream: intel_reg.h DMAR_CAP_CM
pub const DMAR_CAP_CM: u64 = (1 << 7);
// upstream: intel_reg.h DMAR_CAP_PHMR
pub const DMAR_CAP_PHMR: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_CAP_PLMR
pub const DMAR_CAP_PLMR: u64 = (1 << 5);
// upstream: intel_reg.h DMAR_CAP_RWBF
pub const DMAR_CAP_RWBF: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_CAP_AFL
pub const DMAR_CAP_AFL: u64 = (1 << 3);
// upstream: intel_reg.h DMAR_CAP_ND
pub const fn DMAR_CAP_ND(x: u64) -> u64 { (((x) & 0x3)) }
// upstream: intel_reg.h DMAR_ECAP_REG
pub const DMAR_ECAP_REG: u64 = 0x10;
// upstream: intel_reg.h DMAR_ECAP_PSS
pub const fn DMAR_ECAP_PSS(x: u64) -> u64 { (((x) >> 35) & 0xf) }
// upstream: intel_reg.h DMAR_ECAP_EAFS
pub const DMAR_ECAP_EAFS: u64 = (1 << 34);
// upstream: intel_reg.h DMAR_ECAP_NWFS
pub const DMAR_ECAP_NWFS: u64 = (1 << 33);
// upstream: intel_reg.h DMAR_ECAP_SRS
pub const DMAR_ECAP_SRS: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_ECAP_ERS
pub const DMAR_ECAP_ERS: u64 = (1 << 30);
// upstream: intel_reg.h DMAR_ECAP_PRS
pub const DMAR_ECAP_PRS: u64 = (1 << 29);
// upstream: intel_reg.h DMAR_ECAP_PASID
pub const DMAR_ECAP_PASID: u64 = (1 << 28);
// upstream: intel_reg.h DMAR_ECAP_DIS
pub const DMAR_ECAP_DIS: u64 = (1 << 27);
// upstream: intel_reg.h DMAR_ECAP_NEST
pub const DMAR_ECAP_NEST: u64 = (1 << 26);
// upstream: intel_reg.h DMAR_ECAP_MTS
pub const DMAR_ECAP_MTS: u64 = (1 << 25);
// upstream: intel_reg.h DMAR_ECAP_ECS
pub const DMAR_ECAP_ECS: u64 = (1 << 24);
// upstream: intel_reg.h DMAR_ECAP_MHMV
pub const fn DMAR_ECAP_MHMV(x: u64) -> u64 { ((((x) >> 20) & 0xf)) }
// upstream: intel_reg.h DMAR_ECAP_IRO
pub const fn DMAR_ECAP_IRO(x: u64) -> u64 { ((((x) >> 8) & 0x3ff)) }
// upstream: intel_reg.h DMAR_ECAP_SC
pub const DMAR_ECAP_SC: u64 = (1 << 7);
// upstream: intel_reg.h DMAR_ECAP_PT
pub const DMAR_ECAP_PT: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_ECAP_EIM
pub const DMAR_ECAP_EIM: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_ECAP_IR
pub const DMAR_ECAP_IR: u64 = (1 << 3);
// upstream: intel_reg.h DMAR_ECAP_DI
pub const DMAR_ECAP_DI: u64 = (1 << 2);
// upstream: intel_reg.h DMAR_ECAP_QI
pub const DMAR_ECAP_QI: u64 = (1 << 1);
// upstream: intel_reg.h DMAR_ECAP_C
pub const DMAR_ECAP_C: u64 = (1 << 0);
// upstream: intel_reg.h DMAR_GCMD_REG
pub const DMAR_GCMD_REG: u64 = 0x18;
// upstream: intel_reg.h DMAR_GCMD_TE
pub const DMAR_GCMD_TE: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_GCMD_SRTP
pub const DMAR_GCMD_SRTP: u64 = (1 << 30);
// upstream: intel_reg.h DMAR_GCMD_SFL
pub const DMAR_GCMD_SFL: u64 = (1 << 29);
// upstream: intel_reg.h DMAR_GCMD_EAFL
pub const DMAR_GCMD_EAFL: u64 = (1 << 28);
// upstream: intel_reg.h DMAR_GCMD_WBF
pub const DMAR_GCMD_WBF: u64 = (1 << 27);
// upstream: intel_reg.h DMAR_GCMD_QIE
pub const DMAR_GCMD_QIE: u64 = (1 << 26);
// upstream: intel_reg.h DMAR_GCMD_IRE
pub const DMAR_GCMD_IRE: u64 = (1 << 25);
// upstream: intel_reg.h DMAR_GCMD_SIRTP
pub const DMAR_GCMD_SIRTP: u64 = (1 << 24);
// upstream: intel_reg.h DMAR_GCMD_CFI
pub const DMAR_GCMD_CFI: u64 = (1 << 23);
// upstream: intel_reg.h DMAR_GSTS_REG
pub const DMAR_GSTS_REG: u64 = 0x1c;
// upstream: intel_reg.h DMAR_GSTS_TES
pub const DMAR_GSTS_TES: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_GSTS_RTPS
pub const DMAR_GSTS_RTPS: u64 = (1 << 30);
// upstream: intel_reg.h DMAR_GSTS_FLS
pub const DMAR_GSTS_FLS: u64 = (1 << 29);
// upstream: intel_reg.h DMAR_GSTS_AFLS
pub const DMAR_GSTS_AFLS: u64 = (1 << 28);
// upstream: intel_reg.h DMAR_GSTS_WBFS
pub const DMAR_GSTS_WBFS: u64 = (1 << 27);
// upstream: intel_reg.h DMAR_GSTS_QIES
pub const DMAR_GSTS_QIES: u64 = (1 << 26);
// upstream: intel_reg.h DMAR_GSTS_IRES
pub const DMAR_GSTS_IRES: u64 = (1 << 25);
// upstream: intel_reg.h DMAR_GSTS_IRTPS
pub const DMAR_GSTS_IRTPS: u64 = (1 << 24);
// upstream: intel_reg.h DMAR_GSTS_CFIS
pub const DMAR_GSTS_CFIS: u64 = (1 << 23);
// upstream: intel_reg.h DMAR_RTADDR_REG
pub const DMAR_RTADDR_REG: u64 = 0x20;
// upstream: intel_reg.h DMAR_RTADDR_RTT
pub const DMAR_RTADDR_RTT: u64 = (1 << 11);
// upstream: intel_reg.h DMAR_RTADDR_RTA_MASK
pub const DMAR_RTADDR_RTA_MASK: u64 = 0xfffffffffffff000;
// upstream: intel_reg.h DMAR_CCMD_REG
pub const DMAR_CCMD_REG: u64 = 0x28;
// upstream: intel_reg.h DMAR_CCMD_ICC
pub const DMAR_CCMD_ICC: u64 = (1 << 63);
// upstream: intel_reg.h DMAR_CCMD_ICC32
pub const DMAR_CCMD_ICC32: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_CCMD_CIRG_MASK
pub const DMAR_CCMD_CIRG_MASK: u64 = (0x3 << 61);
// upstream: intel_reg.h DMAR_CCMD_CIRG_GLOB
pub const DMAR_CCMD_CIRG_GLOB: u64 = (0x1 << 61);
// upstream: intel_reg.h DMAR_CCMD_CIRG_DOM
pub const DMAR_CCMD_CIRG_DOM: u64 = (0x2 << 61);
// upstream: intel_reg.h DMAR_CCMD_CIRG_DEV
pub const DMAR_CCMD_CIRG_DEV: u64 = (0x3 << 61);
// upstream: intel_reg.h DMAR_CCMD_CAIG
pub const fn DMAR_CCMD_CAIG(x: u64) -> u64 { (((x) >> 59) & 0x3) }
// upstream: intel_reg.h DMAR_CCMD_CAIG_GLOB
pub const DMAR_CCMD_CAIG_GLOB: u64 = 0x1;
// upstream: intel_reg.h DMAR_CCMD_CAIG_DOM
pub const DMAR_CCMD_CAIG_DOM: u64 = 0x2;
// upstream: intel_reg.h DMAR_CCMD_CAIG_DEV
pub const DMAR_CCMD_CAIG_DEV: u64 = 0x3;
// upstream: intel_reg.h DMAR_CCMD_FM
pub const DMAR_CCMD_FM: u64 = (0x3 << 32);
// upstream: intel_reg.h DMAR_CCMD_SID
pub const fn DMAR_CCMD_SID(x: u64) -> u64 { (((x) & 0xffff) << 16) }
// upstream: intel_reg.h DMAR_CCMD_DID
pub const fn DMAR_CCMD_DID(x: u64) -> u64 { ((x) & 0xffff) }
// upstream: intel_reg.h DMAR_IVA_REG_OFF
pub const DMAR_IVA_REG_OFF: u64 = 0;
// upstream: intel_reg.h DMAR_IVA_IH
pub const DMAR_IVA_IH: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_IVA_AM
pub const fn DMAR_IVA_AM(x: u64) -> u64 { ((x) & 0x1f) }
// upstream: intel_reg.h DMAR_IVA_ADDR
pub const fn DMAR_IVA_ADDR(x: u64) -> u64 { ((x) & !0xfff) }
// upstream: intel_reg.h DMAR_IOTLB_REG_OFF
pub const DMAR_IOTLB_REG_OFF: u64 = 0x8;
// upstream: intel_reg.h DMAR_IOTLB_IVT
pub const DMAR_IOTLB_IVT: u64 = (1 << 63);
// upstream: intel_reg.h DMAR_IOTLB_IVT32
pub const DMAR_IOTLB_IVT32: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_IOTLB_IIRG_MASK
pub const DMAR_IOTLB_IIRG_MASK: u64 = (0x3 << 60);
// upstream: intel_reg.h DMAR_IOTLB_IIRG_GLB
pub const DMAR_IOTLB_IIRG_GLB: u64 = (0x1 << 60);
// upstream: intel_reg.h DMAR_IOTLB_IIRG_DOM
pub const DMAR_IOTLB_IIRG_DOM: u64 = (0x2 << 60);
// upstream: intel_reg.h DMAR_IOTLB_IIRG_PAGE
pub const DMAR_IOTLB_IIRG_PAGE: u64 = (0x3 << 60);
// upstream: intel_reg.h DMAR_IOTLB_IAIG_MASK
pub const DMAR_IOTLB_IAIG_MASK: u64 = (0x3 << 57);
// upstream: intel_reg.h DMAR_IOTLB_IAIG_INVLD
pub const DMAR_IOTLB_IAIG_INVLD: u64 = 0;
// upstream: intel_reg.h DMAR_IOTLB_IAIG_GLB
pub const DMAR_IOTLB_IAIG_GLB: u64 = (0x1 << 57);
// upstream: intel_reg.h DMAR_IOTLB_IAIG_DOM
pub const DMAR_IOTLB_IAIG_DOM: u64 = (0x2 << 57);
// upstream: intel_reg.h DMAR_IOTLB_IAIG_PAGE
pub const DMAR_IOTLB_IAIG_PAGE: u64 = (0x3 << 57);
// upstream: intel_reg.h DMAR_IOTLB_DR
pub const DMAR_IOTLB_DR: u64 = (0x1 << 49);
// upstream: intel_reg.h DMAR_IOTLB_DW
pub const DMAR_IOTLB_DW: u64 = (0x1 << 48);
// upstream: intel_reg.h DMAR_IOTLB_DID
pub const fn DMAR_IOTLB_DID(x: u64) -> u64 { (((x) & 0xffff) << 32) }
// upstream: intel_reg.h DMAR_FSTS_REG
pub const DMAR_FSTS_REG: u64 = 0x34;
// upstream: intel_reg.h DMAR_FSTS_FRI
pub const fn DMAR_FSTS_FRI(x: u64) -> u64 { (((x) >> 8) & 0xff) }
// upstream: intel_reg.h DMAR_FSTS_ITE
pub const DMAR_FSTS_ITE: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_FSTS_ICE
pub const DMAR_FSTS_ICE: u64 = (1 << 5);
// upstream: intel_reg.h DMAR_FSTS_IQE
pub const DMAR_FSTS_IQE: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_FSTS_APF
pub const DMAR_FSTS_APF: u64 = (1 << 3);
// upstream: intel_reg.h DMAR_FSTS_AFO
pub const DMAR_FSTS_AFO: u64 = (1 << 2);
// upstream: intel_reg.h DMAR_FSTS_PPF
pub const DMAR_FSTS_PPF: u64 = (1 << 1);
// upstream: intel_reg.h DMAR_FSTS_PFO
pub const DMAR_FSTS_PFO: u64 = 1;
// upstream: intel_reg.h DMAR_FECTL_REG
pub const DMAR_FECTL_REG: u64 = 0x38;
// upstream: intel_reg.h DMAR_FECTL_IM
pub const DMAR_FECTL_IM: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_FECTL_IP
pub const DMAR_FECTL_IP: u64 = (1 << 30);
// upstream: intel_reg.h DMAR_FEDATA_REG
pub const DMAR_FEDATA_REG: u64 = 0x3c;
// upstream: intel_reg.h DMAR_FEADDR_REG
pub const DMAR_FEADDR_REG: u64 = 0x40;
// upstream: intel_reg.h DMAR_FEUADDR_REG
pub const DMAR_FEUADDR_REG: u64 = 0x44;
// upstream: intel_reg.h DMAR_AFLOG_REG
pub const DMAR_AFLOG_REG: u64 = 0x58;
// upstream: intel_reg.h DMAR_FRCD2_F
pub const DMAR_FRCD2_F: u64 = (1 << 63);
// upstream: intel_reg.h DMAR_FRCD2_F32
pub const DMAR_FRCD2_F32: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_FRCD2_T
pub const fn DMAR_FRCD2_T(x: u64) -> u64 { (((x >> 62) & 1)) }
// upstream: intel_reg.h DMAR_FRCD2_T_W
pub const DMAR_FRCD2_T_W: u64 = 0;
// upstream: intel_reg.h DMAR_FRCD2_T_R
pub const DMAR_FRCD2_T_R: u64 = 1;
// upstream: intel_reg.h DMAR_FRCD2_AT
pub const fn DMAR_FRCD2_AT(x: u64) -> u64 { (((x >> 60) & 0x3)) }
// upstream: intel_reg.h DMAR_FRCD2_FR
pub const fn DMAR_FRCD2_FR(x: u64) -> u64 { (((x >> 32) & 0xff)) }
// upstream: intel_reg.h DMAR_FRCD2_SID
pub const fn DMAR_FRCD2_SID(x: u64) -> u64 { ((x & 0xffff)) }
// upstream: intel_reg.h DMAR_FRCS1_FI_MASK
pub const DMAR_FRCS1_FI_MASK: u64 = 0xffffffffff000;
// upstream: intel_reg.h DMAR_PMEN_REG
pub const DMAR_PMEN_REG: u64 = 0x64;
// upstream: intel_reg.h DMAR_PMEN_EPM
pub const DMAR_PMEN_EPM: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_PMEN_PRS
pub const DMAR_PMEN_PRS: u64 = 1;
// upstream: intel_reg.h DMAR_PLMBASE_REG
pub const DMAR_PLMBASE_REG: u64 = 0x68;
// upstream: intel_reg.h DMAR_PLMLIMIT_REG
pub const DMAR_PLMLIMIT_REG: u64 = 0x6c;
// upstream: intel_reg.h DMAR_PHMBASE_REG
pub const DMAR_PHMBASE_REG: u64 = 0x70;
// upstream: intel_reg.h DMAR_PHMLIMIT_REG
pub const DMAR_PHMLIMIT_REG: u64 = 0x78;
// upstream: intel_reg.h DMAR_IQ_DESCR_SZ_SHIFT
pub const DMAR_IQ_DESCR_SZ_SHIFT: u64 = 4;
// upstream: intel_reg.h DMAR_IQ_DESCR_SZ
pub const DMAR_IQ_DESCR_SZ: u64 = (1 << DMAR_IQ_DESCR_SZ_SHIFT);
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_INV
pub const DMAR_IQ_DESCR_CTX_INV: u64 = 0x1;
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_GLOB
pub const DMAR_IQ_DESCR_CTX_GLOB: u64 = (0x1 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_DOM
pub const DMAR_IQ_DESCR_CTX_DOM: u64 = (0x2 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_DEV
pub const DMAR_IQ_DESCR_CTX_DEV: u64 = (0x3 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_DID
pub const fn DMAR_IQ_DESCR_CTX_DID(x: u64) -> u64 { (((x)) << 16) }
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_SRC
pub const fn DMAR_IQ_DESCR_CTX_SRC(x: u64) -> u64 { (((x)) << 32) }
// upstream: intel_reg.h DMAR_IQ_DESCR_CTX_FM
pub const fn DMAR_IQ_DESCR_CTX_FM(x: u64) -> u64 { (((x)) << 48) }
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_INV
pub const DMAR_IQ_DESCR_IOTLB_INV: u64 = 0x2;
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_GLOB
pub const DMAR_IQ_DESCR_IOTLB_GLOB: u64 = (0x1 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_DOM
pub const DMAR_IQ_DESCR_IOTLB_DOM: u64 = (0x2 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_PAGE
pub const DMAR_IQ_DESCR_IOTLB_PAGE: u64 = (0x3 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_DW
pub const DMAR_IQ_DESCR_IOTLB_DW: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_DR
pub const DMAR_IQ_DESCR_IOTLB_DR: u64 = (1 << 7);
// upstream: intel_reg.h DMAR_IQ_DESCR_IOTLB_DID
pub const fn DMAR_IQ_DESCR_IOTLB_DID(x: u64) -> u64 { (((x)) << 16) }
// upstream: intel_reg.h DMAR_IQ_DESCR_DTLB_INV
pub const DMAR_IQ_DESCR_DTLB_INV: u64 = 0x3;
// upstream: intel_reg.h DMAR_IQ_DESCR_IEC_INV
pub const DMAR_IQ_DESCR_IEC_INV: u64 = 0x4;
// upstream: intel_reg.h DMAR_IQ_DESCR_IEC_IDX
pub const DMAR_IQ_DESCR_IEC_IDX: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_IEC_IIDX
pub const fn DMAR_IQ_DESCR_IEC_IIDX(x: u64) -> u64 { ((x) << 32) }
// upstream: intel_reg.h DMAR_IQ_DESCR_IEC_IM
pub const fn DMAR_IQ_DESCR_IEC_IM(x: u64) -> u64 { ((x) << 27) }
// upstream: intel_reg.h DMAR_IQ_DESCR_WAIT_ID
pub const DMAR_IQ_DESCR_WAIT_ID: u64 = 0x5;
// upstream: intel_reg.h DMAR_IQ_DESCR_WAIT_IF
pub const DMAR_IQ_DESCR_WAIT_IF: u64 = (1 << 4);
// upstream: intel_reg.h DMAR_IQ_DESCR_WAIT_SW
pub const DMAR_IQ_DESCR_WAIT_SW: u64 = (1 << 5);
// upstream: intel_reg.h DMAR_IQ_DESCR_WAIT_FN
pub const DMAR_IQ_DESCR_WAIT_FN: u64 = (1 << 6);
// upstream: intel_reg.h DMAR_IQ_DESCR_WAIT_SD
pub const fn DMAR_IQ_DESCR_WAIT_SD(x: u64) -> u64 { (((x)) << 32) }
// upstream: intel_reg.h DMAR_IQ_DESCR_EIOTLB_INV
pub const DMAR_IQ_DESCR_EIOTLB_INV: u64 = 0x6;
// upstream: intel_reg.h DMAR_IQ_DESCR_PASIDC_INV
pub const DMAR_IQ_DESCR_PASIDC_INV: u64 = 0x7;
// upstream: intel_reg.h DMAR_IQ_DESCR_EDTLB_INV
pub const DMAR_IQ_DESCR_EDTLB_INV: u64 = 0x8;
// upstream: intel_reg.h DMAR_IQH_REG
pub const DMAR_IQH_REG: u64 = 0x80;
// upstream: intel_reg.h DMAR_IQH_MASK
pub const DMAR_IQH_MASK: u64 = 0x7fff0;
// upstream: intel_reg.h DMAR_IQT_REG
pub const DMAR_IQT_REG: u64 = 0x88;
// upstream: intel_reg.h DMAR_IQT_MASK
pub const DMAR_IQT_MASK: u64 = 0x7fff0;
// upstream: intel_reg.h DMAR_IQA_REG
pub const DMAR_IQA_REG: u64 = 0x90;
// upstream: intel_reg.h DMAR_IQA_IQA_MASK
pub const DMAR_IQA_IQA_MASK: u64 = 0xfffffffffffff000;
// upstream: intel_reg.h DMAR_IQA_QS_MASK
pub const DMAR_IQA_QS_MASK: u64 = 0x7;
// upstream: intel_reg.h DMAR_IQA_QS_MAX
pub const DMAR_IQA_QS_MAX: u64 = 0x7;
// upstream: intel_reg.h DMAR_IQA_QS_DEF
pub const DMAR_IQA_QS_DEF: u64 = 3;
// upstream: intel_reg.h DMAR_ICS_REG
pub const DMAR_ICS_REG: u64 = 0x9c;
// upstream: intel_reg.h DMAR_ICS_IWC
pub const DMAR_ICS_IWC: u64 = 1;
// upstream: intel_reg.h DMAR_IECTL_REG
pub const DMAR_IECTL_REG: u64 = 0xa0;
// upstream: intel_reg.h DMAR_IECTL_IM
pub const DMAR_IECTL_IM: u64 = (1 << 31);
// upstream: intel_reg.h DMAR_IECTL_IP
pub const DMAR_IECTL_IP: u64 = (1 << 30);
// upstream: intel_reg.h DMAR_IEDATA_REG
pub const DMAR_IEDATA_REG: u64 = 0xa4;
// upstream: intel_reg.h DMAR_IEADDR_REG
pub const DMAR_IEADDR_REG: u64 = 0xa8;
// upstream: intel_reg.h DMAR_IEUADDR_REG
pub const DMAR_IEUADDR_REG: u64 = 0xac;
// upstream: intel_reg.h DMAR_IRTA_REG
pub const DMAR_IRTA_REG: u64 = 0xb8;
// upstream: intel_reg.h DMAR_IRTA_EIME
pub const DMAR_IRTA_EIME: u64 = (1 << 11);
// upstream: intel_reg.h DMAR_IRTA_S_MASK
pub const DMAR_IRTA_S_MASK: u64 = 0xf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intel_hardware_entry_layouts_match_the_128_bit_definitions() {
        assert_eq!(core::mem::size_of::<RootEntry>(), 16);
        assert_eq!(core::mem::size_of::<ContextEntry>(), 16);
        assert_eq!(core::mem::size_of::<Irte>(), 16);
        assert_eq!(core::mem::size_of::<QiDescriptor>(), 16);
        assert_eq!(DMAR_CTX_CNT, 256);
    }

    #[test]
    fn register_field_encodings_preserve_upstream_bit_positions() {
        assert_eq!(DMAR_GCMD_TE, 1 << 31);
        assert_eq!(DMAR_GSTS_QIES, 1 << 26);
        assert_eq!(DMAR_CTX2_DID(0x1234), 0x123400);
        assert_eq!(DMAR_IQ_DESCR_IOTLB_DID(0x1234), 0x1234_0000);
        assert_eq!(DMAR_IQ_DESCR_WAIT_SD(1), 1 << 32);
    }
}
