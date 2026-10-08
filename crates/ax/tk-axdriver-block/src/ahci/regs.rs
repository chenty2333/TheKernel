//! FreeBSD AHCI register definitions and hardware layouts.
//!
//! Translated from FreeBSD `sys/dev/ahci/ahci.h` (source snapshot at
//! `/home/ava/.cache/thekernel-targets/ref/freebsd`, BSD-2-Clause).
//! Copyright (c) 1998 - 2008 Søren Schmidt <sos@FreeBSD.org>
//! Copyright (c) 2009-2012 Alexander Motin <mav@FreeBSD.org>
//!
//! This module contains the hardware-facing definitions from the header.
//! FreeBSD bus, CAM, callout, locking, and resource-manager objects are
//! intentionally represented by Rust-side state in the AHCI driver rather
//! than copied as framework types.

pub const ATA_DATA: u32 = 0; // (RW) data
pub const ATA_FEATURE: u32 = 1; // (W) feature
pub const ATA_F_DMA: u32 = 0x01; // enable DMA
pub const ATA_F_OVL: u32 = 0x02; // enable overlap
pub const ATA_COUNT: u32 = 2; // (W) sector count
pub const ATA_SECTOR: u32 = 3; // (RW) sector #
pub const ATA_CYL_LSB: u32 = 4; // (RW) cylinder# LSB
pub const ATA_CYL_MSB: u32 = 5; // (RW) cylinder# MSB
pub const ATA_DRIVE: u32 = 6; // (W) Sector/Drive/Head
pub const ATA_D_LBA: u32 = 0x40; // use LBA addressing
pub const ATA_D_IBM: u32 = 0xa0; // 512 byte sectors, ECC
pub const ATA_COMMAND: u32 = 7; // (W) command
pub const ATA_ERROR: u32 = 8; // (R) error
pub const ATA_E_ILI: u32 = 0x01; // illegal length
pub const ATA_E_NM: u32 = 0x02; // no media
pub const ATA_E_ABORT: u32 = 0x04; // command aborted
pub const ATA_E_MCR: u32 = 0x08; // media change request
pub const ATA_E_IDNF: u32 = 0x10; // ID not found
pub const ATA_E_MC: u32 = 0x20; // media changed
pub const ATA_E_UNC: u32 = 0x40; // uncorrectable data
pub const ATA_E_ICRC: u32 = 0x80; // UDMA crc error
pub const ATA_E_ATAPI_SENSE_MASK: u32 = 0xf0; // ATAPI sense key mask
pub const ATA_IREASON: u32 = 9; // (R) interrupt reason
pub const ATA_I_CMD: u32 = 0x01; // cmd (1) | data (0)
pub const ATA_I_IN: u32 = 0x02; // read (1) | write (0)
pub const ATA_I_RELEASE: u32 = 0x04; // released bus (1)
pub const ATA_I_TAGMASK: u32 = 0xf8; // tag mask
pub const ATA_STATUS: u32 = 10; // (R) status
pub const ATA_ALTSTAT: u32 = 11; // (R) alternate status
pub const ATA_S_ERROR: u32 = 0x01; // error
pub const ATA_S_INDEX: u32 = 0x02; // index
pub const ATA_S_CORR: u32 = 0x04; // data corrected
pub const ATA_S_DRQ: u32 = 0x08; // data request
pub const ATA_S_DSC: u32 = 0x10; // drive seek completed
pub const ATA_S_SERVICE: u32 = 0x10; // drive needs service
pub const ATA_S_DWF: u32 = 0x20; // drive write fault
pub const ATA_S_DMA: u32 = 0x20; // DMA ready
pub const ATA_S_READY: u32 = 0x40; // drive ready
pub const ATA_S_BUSY: u32 = 0x80; // busy
pub const ATA_CONTROL: u32 = 12; // (W) control
pub const ATA_A_IDS: u32 = 0x02; // disable interrupts
pub const ATA_A_RESET: u32 = 0x04; // RESET controller
pub const ATA_A_4BIT: u32 = 0x08; // 4 head bits
pub const ATA_A_HOB: u32 = 0x80; // High Order Byte enable
pub const ATA_SSTATUS: u32 = 13;
pub const ATA_SS_DET_MASK: u32 = 0x0000000f;
pub const ATA_SS_DET_NO_DEVICE: u32 = 0x00000000;
pub const ATA_SS_DET_DEV_PRESENT: u32 = 0x00000001;
pub const ATA_SS_DET_PHY_ONLINE: u32 = 0x00000003;
pub const ATA_SS_DET_PHY_OFFLINE: u32 = 0x00000004;
pub const ATA_SS_SPD_MASK: u32 = 0x000000f0;
pub const ATA_SS_SPD_NO_SPEED: u32 = 0x00000000;
pub const ATA_SS_SPD_GEN1: u32 = 0x00000010;
pub const ATA_SS_SPD_GEN2: u32 = 0x00000020;
pub const ATA_SS_SPD_GEN3: u32 = 0x00000030;
pub const ATA_SS_IPM_MASK: u32 = 0x00000f00;
pub const ATA_SS_IPM_NO_DEVICE: u32 = 0x00000000;
pub const ATA_SS_IPM_ACTIVE: u32 = 0x00000100;
pub const ATA_SS_IPM_PARTIAL: u32 = 0x00000200;
pub const ATA_SS_IPM_SLUMBER: u32 = 0x00000600;
pub const ATA_SS_IPM_DEVSLEEP: u32 = 0x00000800;
pub const ATA_SERROR: u32 = 14;
pub const ATA_SE_DATA_CORRECTED: u32 = 0x00000001;
pub const ATA_SE_COMM_CORRECTED: u32 = 0x00000002;
pub const ATA_SE_DATA_ERR: u32 = 0x00000100;
pub const ATA_SE_COMM_ERR: u32 = 0x00000200;
pub const ATA_SE_PROT_ERR: u32 = 0x00000400;
pub const ATA_SE_HOST_ERR: u32 = 0x00000800;
pub const ATA_SE_PHY_CHANGED: u32 = 0x00010000;
pub const ATA_SE_PHY_IERROR: u32 = 0x00020000;
pub const ATA_SE_COMM_WAKE: u32 = 0x00040000;
pub const ATA_SE_DECODE_ERR: u32 = 0x00080000;
pub const ATA_SE_PARITY_ERR: u32 = 0x00100000;
pub const ATA_SE_CRC_ERR: u32 = 0x00200000;
pub const ATA_SE_HANDSHAKE_ERR: u32 = 0x00400000;
pub const ATA_SE_LINKSEQ_ERR: u32 = 0x00800000;
pub const ATA_SE_TRANSPORT_ERR: u32 = 0x01000000;
pub const ATA_SE_UNKNOWN_FIS: u32 = 0x02000000;
pub const ATA_SE_EXCHANGED: u32 = 0x04000000;
pub const ATA_SCONTROL: u32 = 15;
pub const ATA_SC_DET_MASK: u32 = 0x0000000f;
pub const ATA_SC_DET_IDLE: u32 = 0x00000000;
pub const ATA_SC_DET_RESET: u32 = 0x00000001;
pub const ATA_SC_DET_DISABLE: u32 = 0x00000004;
pub const ATA_SC_SPD_MASK: u32 = 0x000000f0;
pub const ATA_SC_SPD_NO_SPEED: u32 = 0x00000000;
pub const ATA_SC_SPD_SPEED_GEN1: u32 = 0x00000010;
pub const ATA_SC_SPD_SPEED_GEN2: u32 = 0x00000020;
pub const ATA_SC_SPD_SPEED_GEN3: u32 = 0x00000030;
pub const ATA_SC_IPM_MASK: u32 = 0x00000f00;
pub const ATA_SC_IPM_NONE: u32 = 0x00000000;
pub const ATA_SC_IPM_DIS_PARTIAL: u32 = 0x00000100;
pub const ATA_SC_IPM_DIS_SLUMBER: u32 = 0x00000200;
pub const ATA_SC_IPM_DIS_DEVSLEEP: u32 = 0x00000400;
pub const ATA_SACTIVE: u32 = 16;
pub const AHCI_CAP: usize = 0x00;
pub const AHCI_CAP_NPMASK: u32 = 0x0000001f;
pub const AHCI_CAP_SXS: u32 = 0x00000020;
pub const AHCI_CAP_EMS: u32 = 0x00000040;
pub const AHCI_CAP_CCCS: u32 = 0x00000080;
pub const AHCI_CAP_NCS: u32 = 0x00001F00;
pub const AHCI_CAP_NCS_SHIFT: u32 = 8;
pub const AHCI_CAP_PSC: u32 = 0x00002000;
pub const AHCI_CAP_SSC: u32 = 0x00004000;
pub const AHCI_CAP_PMD: u32 = 0x00008000;
pub const AHCI_CAP_FBSS: u32 = 0x00010000;
pub const AHCI_CAP_SPM: u32 = 0x00020000;
pub const AHCI_CAP_SAM: u32 = 0x00080000;
pub const AHCI_CAP_ISS: u32 = 0x00F00000;
pub const AHCI_CAP_ISS_SHIFT: u32 = 20;
pub const AHCI_CAP_SCLO: u32 = 0x01000000;
pub const AHCI_CAP_SAL: u32 = 0x02000000;
pub const AHCI_CAP_SALP: u32 = 0x04000000;
pub const AHCI_CAP_SSS: u32 = 0x08000000;
pub const AHCI_CAP_SMPS: u32 = 0x10000000;
pub const AHCI_CAP_SSNTF: u32 = 0x20000000;
pub const AHCI_CAP_SNCQ: u32 = 0x40000000;
pub const AHCI_CAP_64BIT: u32 = 0x80000000;
pub const AHCI_GHC: usize = 0x04;
pub const AHCI_GHC_AE: u32 = 0x80000000;
pub const AHCI_GHC_MRSM: u32 = 0x00000004;
pub const AHCI_GHC_IE: u32 = 0x00000002;
pub const AHCI_GHC_HR: u32 = 0x00000001;
pub const AHCI_IS: usize = 0x08;
pub const AHCI_PI: usize = 0x0c;
pub const AHCI_VS: usize = 0x10;
pub const AHCI_CCCC: usize = 0x14;
pub const AHCI_CCCC_TV_MASK: u32 = 0xffff0000;
pub const AHCI_CCCC_TV_SHIFT: u32 = 16;
pub const AHCI_CCCC_CC_MASK: u32 = 0x0000ff00;
pub const AHCI_CCCC_CC_SHIFT: u32 = 8;
pub const AHCI_CCCC_INT_MASK: u32 = 0x000000f8;
pub const AHCI_CCCC_INT_SHIFT: u32 = 3;
pub const AHCI_CCCC_EN: u32 = 0x00000001;
pub const AHCI_CCCP: usize = 0x18;
pub const AHCI_EM_LOC: usize = 0x1C;
pub const AHCI_EM_CTL: usize = 0x20;
pub const AHCI_EM_MR: u32 = 0x00000001;
pub const AHCI_EM_TM: u32 = 0x00000100;
pub const AHCI_EM_RST: u32 = 0x00000200;
pub const AHCI_EM_LED: u32 = 0x00010000;
pub const AHCI_EM_SAFTE: u32 = 0x00020000;
pub const AHCI_EM_SES2: u32 = 0x00040000;
pub const AHCI_EM_SGPIO: u32 = 0x00080000;
pub const AHCI_EM_SMB: u32 = 0x01000000;
pub const AHCI_EM_XMT: u32 = 0x02000000;
pub const AHCI_EM_ALHD: u32 = 0x04000000;
pub const AHCI_EM_PM: u32 = 0x08000000;
pub const AHCI_CAP2: usize = 0x24;
pub const AHCI_CAP2_BOH: u32 = 0x00000001;
pub const AHCI_CAP2_NVMP: u32 = 0x00000002;
pub const AHCI_CAP2_APST: u32 = 0x00000004;
pub const AHCI_CAP2_SDS: u32 = 0x00000008;
pub const AHCI_CAP2_SADM: u32 = 0x00000010;
pub const AHCI_CAP2_DESO: u32 = 0x00000020;
pub const AHCI_BOHC: usize = 0x28;
pub const AHCI_BOHC_BOS: u32 = 0x00000001;
pub const AHCI_BOHC_OOS: u32 = 0x00000002;
pub const AHCI_BOHC_SOOE: u32 = 0x00000004;
pub const AHCI_BOHC_OOC: u32 = 0x00000008;
pub const AHCI_BOHC_BB: u32 = 0x00000010;
pub const AHCI_VSCAP: usize = 0xa4;
pub const AHCI_OFFSET: usize = 0x100;
pub const AHCI_STEP: usize = 0x80;
pub const AHCI_P_CLB: usize = 0x00;
pub const AHCI_P_CLBU: usize = 0x04;
pub const AHCI_P_FB: usize = 0x08;
pub const AHCI_P_FBU: usize = 0x0c;
pub const AHCI_P_IS: usize = 0x10;
pub const AHCI_P_IE: usize = 0x14;
pub const AHCI_P_IX_DHR: u32 = 0x00000001;
pub const AHCI_P_IX_PS: u32 = 0x00000002;
pub const AHCI_P_IX_DS: u32 = 0x00000004;
pub const AHCI_P_IX_SDB: u32 = 0x00000008;
pub const AHCI_P_IX_UF: u32 = 0x00000010;
pub const AHCI_P_IX_DP: u32 = 0x00000020;
pub const AHCI_P_IX_PC: u32 = 0x00000040;
pub const AHCI_P_IX_MP: u32 = 0x00000080;
pub const AHCI_P_IX_PRC: u32 = 0x00400000;
pub const AHCI_P_IX_IPM: u32 = 0x00800000;
pub const AHCI_P_IX_OF: u32 = 0x01000000;
pub const AHCI_P_IX_INF: u32 = 0x04000000;
pub const AHCI_P_IX_IF: u32 = 0x08000000;
pub const AHCI_P_IX_HBD: u32 = 0x10000000;
pub const AHCI_P_IX_HBF: u32 = 0x20000000;
pub const AHCI_P_IX_TFE: u32 = 0x40000000;
pub const AHCI_P_IX_CPD: u32 = 0x80000000;
pub const AHCI_P_CMD: usize = 0x18;
pub const AHCI_P_CMD_ST: u32 = 0x00000001;
pub const AHCI_P_CMD_SUD: u32 = 0x00000002;
pub const AHCI_P_CMD_POD: u32 = 0x00000004;
pub const AHCI_P_CMD_CLO: u32 = 0x00000008;
pub const AHCI_P_CMD_FRE: u32 = 0x00000010;
pub const AHCI_P_CMD_CCS_MASK: u32 = 0x00001f00;
pub const AHCI_P_CMD_CCS_SHIFT: u32 = 8;
pub const AHCI_P_CMD_ISS: u32 = 0x00002000;
pub const AHCI_P_CMD_FR: u32 = 0x00004000;
pub const AHCI_P_CMD_CR: u32 = 0x00008000;
pub const AHCI_P_CMD_CPS: u32 = 0x00010000;
pub const AHCI_P_CMD_PMA: u32 = 0x00020000;
pub const AHCI_P_CMD_HPCP: u32 = 0x00040000;
pub const AHCI_P_CMD_MPSP: u32 = 0x00080000;
pub const AHCI_P_CMD_CPD: u32 = 0x00100000;
pub const AHCI_P_CMD_ESP: u32 = 0x00200000;
pub const AHCI_P_CMD_FBSCP: u32 = 0x00400000;
pub const AHCI_P_CMD_APSTE: u32 = 0x00800000;
pub const AHCI_P_CMD_ATAPI: u32 = 0x01000000;
pub const AHCI_P_CMD_DLAE: u32 = 0x02000000;
pub const AHCI_P_CMD_ALPE: u32 = 0x04000000;
pub const AHCI_P_CMD_ASP: u32 = 0x08000000;
pub const AHCI_P_CMD_ICC_MASK: u32 = 0xf0000000;
pub const AHCI_P_CMD_NOOP: u32 = 0x00000000;
pub const AHCI_P_CMD_ACTIVE: u32 = 0x10000000;
pub const AHCI_P_CMD_PARTIAL: u32 = 0x20000000;
pub const AHCI_P_CMD_SLUMBER: u32 = 0x60000000;
pub const AHCI_P_CMD_DEVSLEEP: u32 = 0x80000000;
pub const AHCI_P_TFD: usize = 0x20;
pub const AHCI_P_SIG: usize = 0x24;
pub const AHCI_P_SSTS: usize = 0x28;
pub const AHCI_P_SCTL: usize = 0x2c;
pub const AHCI_P_SERR: usize = 0x30;
pub const AHCI_P_SACT: usize = 0x34;
pub const AHCI_P_CI: usize = 0x38;
pub const AHCI_P_SNTF: usize = 0x3C;
pub const AHCI_P_FBS: usize = 0x40;
pub const AHCI_P_FBS_EN: u32 = 0x00000001;
pub const AHCI_P_FBS_DEC: u32 = 0x00000002;
pub const AHCI_P_FBS_SDE: u32 = 0x00000004;
pub const AHCI_P_FBS_DEV: u32 = 0x00000f00;
pub const AHCI_P_FBS_DEV_SHIFT: u32 = 8;
pub const AHCI_P_FBS_ADO: u32 = 0x0000f000;
pub const AHCI_P_FBS_ADO_SHIFT: u32 = 12;
pub const AHCI_P_FBS_DWE: u32 = 0x000f0000;
pub const AHCI_P_FBS_DWE_SHIFT: u32 = 16;
pub const AHCI_P_DEVSLP: usize = 0x44;
pub const AHCI_P_DEVSLP_ADSE: u32 = 0x00000001;
pub const AHCI_P_DEVSLP_DSP: u32 = 0x00000002;
pub const AHCI_P_DEVSLP_DETO: u32 = 0x000003fc;
pub const AHCI_P_DEVSLP_DETO_SHIFT: u32 = 2;
pub const AHCI_P_DEVSLP_MDAT: u32 = 0x00007c00;
pub const AHCI_P_DEVSLP_MDAT_SHIFT: u32 = 10;
pub const AHCI_P_DEVSLP_DITO: u32 = 0x01ff8000;
pub const AHCI_P_DEVSLP_DITO_SHIFT: u32 = 15;
pub const AHCI_P_DEVSLP_DM: u32 = 0x0e000000;
pub const AHCI_P_DEVSLP_DM_SHIFT: u32 = 25;
pub const AHCI_CL_OFFSET: u32 = 0;
pub const AHCI_CL_SIZE: u32 = 32;
pub const AHCI_UNIT: u32 = 0xff; // Channel number.
pub const AHCI_PRD_MASK: u32 = 0x003fffff; // max 4MB
pub const AHCI_CMD_ATAPI: u32 = 0x0020;
pub const AHCI_CMD_WRITE: u32 = 0x0040;
pub const AHCI_CMD_PREFETCH: u32 = 0x0080;
pub const AHCI_CMD_RESET: u32 = 0x0100;
pub const AHCI_CMD_BIST: u32 = 0x0200;
pub const AHCI_CMD_CLR_BUSY: u32 = 0x0400;
pub const ATA_IRQ_RID: u32 = 0;
pub const AHCI_NUM_LEDS: u32 = 3;
pub const AHCI_IRQ_MODE_ALL: u32 = 0;
pub const AHCI_IRQ_MODE_AFTER: u32 = 1;
pub const AHCI_IRQ_MODE_ONE: u32 = 2;
pub const AHCI_Q_NOFORCE: u32 = 0x00000001;
pub const AHCI_Q_NOPMP: u32 = 0x00000002;
pub const AHCI_Q_NONCQ: u32 = 0x00000004;
pub const AHCI_Q_1CH: u32 = 0x00000008;
pub const AHCI_Q_2CH: u32 = 0x00000010;
pub const AHCI_Q_4CH: u32 = 0x00000020;
pub const AHCI_Q_EDGEIS: u32 = 0x00000040;
pub const AHCI_Q_SATA2: u32 = 0x00000080;
pub const AHCI_Q_NOBSYRES: u32 = 0x00000100;
pub const AHCI_Q_NOAA: u32 = 0x00000200;
pub const AHCI_Q_NOCOUNT: u32 = 0x00000400;
pub const AHCI_Q_ALTSIG: u32 = 0x00000800;
pub const AHCI_Q_NOMSI: u32 = 0x00001000;
pub const AHCI_Q_ATI_PMP_BUG: u32 = 0x00002000;
pub const AHCI_Q_MAXIO_64K: u32 = 0x00004000;
pub const AHCI_Q_SATA1_UNIT0: u32 = 0x00008000; // need better method for this
pub const AHCI_Q_ABAR0: u32 = 0x00010000;
pub const AHCI_Q_1MSI: u32 = 0x00020000;
pub const AHCI_Q_FORCE_PI: u32 = 0x00040000;
pub const AHCI_Q_RESTORE_CAP: u32 = 0x00080000;
pub const AHCI_Q_NOMSIX: u32 = 0x00100000;
pub const AHCI_Q_MRVL_SR_DEL: u32 = 0x00200000;
pub const AHCI_Q_NOCCS: u32 = 0x00400000;
pub const AHCI_Q_NOAUX: u32 = 0x00800000;
pub const AHCI_Q_IOMMU_BUSWIDE: u32 = 0x01000000;
pub const AHCI_Q_SLOWDEV: u32 = 0x02000000;

/// Number of AHCI ports exposed by the specification.
pub const AHCI_MAX_PORTS: usize = 32;
/// Number of command slots exposed by one AHCI port.
pub const AHCI_MAX_SLOTS: usize = 32;
/// Number of interrupt vectors handled by the upstream controller driver.
pub const AHCI_MAX_IRQS: usize = 16;
/// Maximum bytes in one AHCI PRD entry.
pub const AHCI_PRD_IPC: u32 = 1 << 31;
pub const AHCI_PRD_MAX: usize = 4 * 1024 * 1024;

/// FreeBSD's AHCI controller quirk bits (`AHCI_Q_*`).

/// FreeBSD controller/channel ivar flags and unit sentinels.
pub const AHCI_REMAPPED_UNIT: u32 = 1 << 31;
pub const AHCI_EM_UNIT: u32 = 1 << 30;

/// Command table offset in the shared work area, following the command list.
pub const AHCI_CT_OFFSET: usize =
    (AHCI_CL_OFFSET as usize) + (AHCI_CL_SIZE as usize) * AHCI_MAX_SLOTS;

/// Number of PRD entries chosen for a buffer length and page granularity.
/// The FreeBSD macro pessimistically rounds `btoc(maxphys) + 1` up to a
/// multiple of eight, capped by the 16-bit PRD count.
pub const fn ahci_sg_entries(maxphys: usize, page_size: usize) -> usize {
    let pages = maxphys.saturating_add(page_size - 1) / page_size;
    let rounded = (pages.saturating_add(1).saturating_add(7)) & !7;
    if rounded > 65_528 { 65_528 } else { rounded }
}

/// Per-command-table allocation size for the supplied PRD bound.
pub const fn command_table_bytes(sg_entries: usize) -> usize {
    128 + sg_entries * core::mem::size_of::<DmaPrd>()
}

/// Aggregate work-area bytes for the given number of command slots.
pub const fn work_area_bytes(num_slots: usize, sg_entries: usize) -> usize {
    AHCI_CT_OFFSET + command_table_bytes(sg_entries) * num_slots
}

/// DMA scatter/gather descriptor (`ahci_dma_prd`).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct DmaPrd {
    pub data_base: u64,
    pub reserved: u32,
    // Zero-based byte count; bit 31 is the interrupt-on-completion flag.
    pub byte_count: u32,
}

/// AHCI command table header and packet command area (`ahci_cmd_tab`).
/// PRD entries immediately follow `prd` in the DMA command table allocation.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct CommandTableHeader {
    pub command_fis: [u8; 64],
    pub atapi_command: [u8; 32],
    pub reserved: [u8; 32],
}

impl Default for CommandTableHeader {
    fn default() -> Self {
        Self {
            command_fis: [0; 64],
            atapi_command: [0; 32],
            reserved: [0; 32],
        }
    }
}

/// One entry in the 32-entry command list (`ahci_cmd_list`).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct CommandHeader {
    pub flags: u16,
    pub prd_length: u16,
    pub byte_count: u32,
    pub command_table: u64,
    /// Reserved bytes complete the AHCI 32-byte command-list stride.
    pub reserved: [u8; 16],
}

/// Per-command DMA mapping facts from `struct ata_dmaslot`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AtaDmaSlot {
    pub segment_count: u32,
}

/// User-selected or currently negotiated ATA device settings from
/// `struct ahci_device`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceSettings {
    pub revision: i32,
    pub mode: i32,
    pub byte_count: u32,
    pub atapi: bool,
    pub tags: u32,
    pub capabilities: u32,
}

/// One software-owned command slot, corresponding to FreeBSD `ahci_slot`.
/// Framework-owned CCB, DMA-map, and callout pointers are replaced by the
/// block request handle and DMA ownership kept by the Rust controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommandSlotState {
    pub slot: u8,
    pub state: SlotState,
    pub command_table_offset: u32,
    pub request_id: Option<u64>,
    pub dma: AtaDmaSlot,
}

/// AHCI channel state corresponding to `struct ahci_channel`.
#[derive(Clone, Debug)]
pub struct ChannelState {
    pub unit: u8,
    pub capabilities: u32,
    pub capabilities2: u32,
    pub channel_capabilities: u32,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub quirks: u32,
    pub num_slots: u8,
    pub devices: u32,
    pub port_multiplier_present: bool,
    pub fis_switching_enabled: bool,
    pub occupied_slots: u32,
    pub running_slots: u32,
    pub atomic_slots: u32,
    pub error_slots: u32,
    pub timeout_slots: u32,
    pub last_slot: i32,
    pub running_slot_count: u32,
    pub tagged_slot_count: u32,
    pub held_slot_count: u32,
    pub fatal_error: bool,
    pub resetting: bool,
    pub slots: [CommandSlotState; AHCI_MAX_SLOTS],
    pub user_settings: [DeviceSettings; 16],
    pub current_settings: [DeviceSettings; 16],
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            unit: 0,
            capabilities: 0,
            capabilities2: 0,
            channel_capabilities: 0,
            vendor_id: 0,
            device_id: 0,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
            quirks: 0,
            num_slots: 0,
            devices: 0,
            port_multiplier_present: false,
            fis_switching_enabled: false,
            occupied_slots: 0,
            running_slots: 0,
            atomic_slots: 0,
            error_slots: 0,
            timeout_slots: 0,
            last_slot: -1,
            running_slot_count: 0,
            tagged_slot_count: 0,
            held_slot_count: 0,
            fatal_error: false,
            resetting: false,
            slots: [CommandSlotState::default(); AHCI_MAX_SLOTS],
            user_settings: [DeviceSettings::default(); 16],
            current_settings: [DeviceSettings::default(); 16],
        }
    }
}

/// Controller state corresponding to `struct ahci_controller`.
#[derive(Clone, Debug)]
pub struct ControllerState {
    pub capabilities: u32,
    pub capabilities2: u32,
    pub enclosure_capabilities: u32,
    pub quirks: u32,
    pub num_interrupts: u8,
    pub num_channels: u8,
    pub command_completion_coalescing_ms: u16,
    pub direct_completion: bool,
    pub msi_enabled: bool,
    pub dma_coherent: bool,
    pub ports: [Option<ChannelState>; AHCI_MAX_PORTS],
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            capabilities: 0,
            capabilities2: 0,
            enclosure_capabilities: 0,
            quirks: 0,
            num_interrupts: 0,
            num_channels: 0,
            command_completion_coalescing_ms: 0,
            direct_completion: false,
            msi_enabled: false,
            dma_coherent: false,
            ports: core::array::from_fn(|_| None),
        }
    }
}

/// The high-level state values used by FreeBSD's `enum ahci_slot_states`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SlotState {
    #[default]
    Empty     = 0,
    Loading   = 1,
    Running   = 2,
    Executing = 3,
}

/// Error classes used by the upstream transaction recovery state machine.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ErrorType {
    #[default]
    None     = 0,
    Invalid  = 1,
    Innocent = 2,
    TaskFile = 3,
    Sata     = 4,
    Timeout  = 5,
    Ncq      = 6,
}

/// AHCI port register offset for port `index` and register `offset`.
#[inline]
pub const fn port_register(index: usize, offset: usize) -> usize {
    AHCI_OFFSET + index * AHCI_STEP + offset
}

/// Number of command headers that fit in the AHCI command list.
#[inline]
pub const fn command_list_bytes() -> usize {
    AHCI_MAX_SLOTS * core::mem::size_of::<CommandHeader>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_layout_matches_ahci_specification() {
        assert_eq!(core::mem::size_of::<DmaPrd>(), 16);
        assert_eq!(core::mem::size_of::<CommandTableHeader>(), 128);
        assert_eq!(core::mem::size_of::<CommandHeader>(), 32);
        assert_eq!(command_list_bytes(), 1024);
    }

    #[test]
    fn port_registers_follow_the_upstream_stride() {
        assert_eq!(port_register(0, AHCI_P_CMD), 0x118);
        assert_eq!(port_register(1, AHCI_P_CMD), 0x198);
        assert_eq!(port_register(31, AHCI_P_CI), 0x10b8);
    }
}
