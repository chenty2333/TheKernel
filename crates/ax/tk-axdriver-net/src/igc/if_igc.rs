//! FreeBSD IGC device/netif policy helpers translated to the NetDriver boundary.
//!
//! Translated from FreeBSD `sys/dev/igc/if_igc.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024 Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2021-2024 Rubicon Communications, LLC (Netgate).

use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{
    api::{IgcHardware, igc_set_mac_type},
    mac::MacState,
};

const MAX_MULTICAST: usize = 128;
const VFTA_SIZE: usize = 128;
const RCTL: u32 = 0x00100;
const CTRL: u32 = 0;
const TIPG: u32 = 0x00410;
const EITR_BASE: u32 = 0x01680;
const RCTL_SBP: u32 = 0x4;
const RCTL_UPE: u32 = 0x8;
const RCTL_MPE: u32 = 0x10;
const RCTL_VFE: u32 = 0x0004_0000;
const RCTL_CFIEN: u32 = 0x0008_0000;
const CTRL_VME: u32 = 0x4000_0000;
const TIPG_IPGT_MASK: u32 = 0x3ff;
const DEFAULT_IPGT_COPPER: u32 = 8;
const I225_IPGT_2P5: u32 = 0xb;
const REVISION_2: u8 = 2;
const EITR_CNT_IGNR: u32 = 0x8000_0000;
const EITR_SHIFT: u32 = 2;
const EITR_DIVIDEND: u32 = 1_000_000;
const EITR_QVECTOR_MASK: u32 = 0x7ffc;
const INTS_4K: u32 = 4000;
const INTS_20K: u32 = 20_000;
const INTS_70K: u32 = 70_000;
const IFF_PROMISC: u32 = 0x0100;
const TDLEN: u32 = 0x03808;
const TDBAL: u32 = 0x03800;
const TDBAH: u32 = 0x03804;
const TDT: u32 = 0x03818;
const TDH: u32 = 0x03810;
const TXDCTL: u32 = 0x03828;
const RDLEN: u32 = 0x02808;
const RDBAL: u32 = 0x02800;
const RDBAH: u32 = 0x02804;
const RDT: u32 = 0x02818;
const RDH: u32 = 0x02810;
const RXDCTL: u32 = 0x02828;
const SRRCTL: u32 = 0x0280c;
const RXCSUM: u32 = 0x05000;
const RLPML: u32 = 0x05004;
const VET: u32 = 0x00038;
const TCTL_EN: u32 = 2;
const TCTL_PSP: u32 = 8;
const TCTL_RTLC: u32 = 0x0100_0000;
const TCTL_CT: u32 = 0x0000_0ff0;
const TCTL_CT_SHIFT: u32 = 4;
const COLLISION_THRESHOLD: u32 = 15;
const TXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
const TX_PTHRESH: u32 = 8;
const TX_HTHRESH: u32 = 1;
const RXDCTL_PTHRESH: u32 = 0x1f;
const RXDCTL_HTHRESH: u32 = 0x1f00;
const RXDCTL_WTHRESH: u32 = 0x001f_0000;
const RXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
const RCTL_EN: u32 = 2;
const RCTL_BAM: u32 = 0x8000;
const RCTL_LPE: u32 = 0x20;
const RCTL_SECRC: u32 = 0x0400_0000;
const RCTL_MO_SHIFT: u32 = 12;
const RCTL_SZ_2048: u32 = 0;
const RXCSUM_TUOFL: u32 = 0x200;
const RXCSUM_CRCOFL: u32 = 0x800;
const RXCSUM_IPPCSE: u32 = 0x1000;
const RXCSUM_PCSD: u32 = 0x2000;
const SRRCTL_BSIZEPKT_SHIFT: u32 = 10;
const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 0x0200_0000;
const SRRCTL_DROP_EN: u32 = 0x8000_0000;
const MRQC: u32 = 0x05818;
const RETA: u32 = 0x05c00;
const RSSRK: u32 = 0x05c80;
const MRQC_ENABLE_RSS_4Q: u32 = 2;
const FC_PAUSE_TIME: u16 = 0x0680;
const PBA_34K: u32 = 0x22;
const WUS: u32 = 0x05800;
const WUS_EXT: u32 = 0x05804;
const WUFC: u32 = 0x05808;
const WUFC_EXT: u32 = 0x0580c;
const WUC: u32 = 0x05810;
const PCIEERRSTS: u32 = 0x05ba8;
const PEIND: u32 = 0x01084;
const LANPERRSTS: u32 = 0x05f58;
const STATUS: u32 = 0x8;
const EECD: u32 = 0x10;
const CTRL_DEV_RST: u32 = 0x2000_0000;
const STATUS_RST_DONE: u32 = 0x0020_0000;
const EECD_AUTO_RD: u32 = 0x200;
const PEIND_PCIE_PARITY_FATAL: u32 = 4;
const PCIEERRSTS_FATAL_MASK: u32 = 0x78;
const LANPERRSTS_RETX_BUF: u32 = 0x200;
const PBECCSTS: u32 = 0x0245c;
const PCIEECCSTS: u32 = 0x05bac;
const PBECCSTS_ECC_ENABLE: u32 = 1;
const PBECCSTS_CORR_ERR: u32 = 4;
const PCIEECCSTS_CORR_MASK: u32 = 0x30;
const PCIEECCSTS_TX_WR_DATA: u32 = 0x10;
const PCIEECCSTS_RETRY_BUF: u32 = 0x20;
const MNGPARSTS: u32 = 0x08f24;
const ICR: u32 = 0x01500;
const IMS: u32 = 0x01508;
const IMC: u32 = 0x0150c;
const EIMS: u32 = 0x01524;
const EIMC: u32 = 0x01528;
const EIAC: u32 = 0x0152c;
const EIAM: u32 = 0x01530;
const PEIND_LANPORT_PARITY_FATAL: u32 = 1;
const PEIND_MNG_PARITY_FATAL: u32 = 2;
const PEIND_DMA_PARITY_FATAL: u32 = 8;
const PEIND_FATAL_MASK: u32 = 0xf;
const MNGPARSTS_FATAL_MASK: u32 = 3;
const ICR_LSC: u32 = 4;
const ICR_RXSEQ: u32 = 8;
const ICR_RXO: u32 = 0x40;
const ICR_INT_ASSERTED: u32 = 0x8000_0000;
const ICR_FER: u32 = 0x0040_0000;
const IMS_LSC: u32 = ICR_LSC;
const IMS_FER: u32 = ICR_FER;
const IMS_ENABLE_MASK: u32 = 1 | 4 | 8 | 0x10 | 0x80;
const GPIE: u32 = 0x01514;
const IVAR0: u32 = 0x01700;
const IVAR_MISC: u32 = 0x01740;
const GPIE_MSIX_MODE: u32 = 0x0000_0010;
const GPIE_EIAME: u32 = 0x4000_0000;
const GPIE_PBA: u32 = 0x8000_0000;
const GPIE_NSICR: u32 = 0x0000_0020;
const IVAR_VALID: u32 = 0x80;
const FATAL_NONE: u32 = 0;
const FATAL_CAPTURING: u32 = 1;
const FATAL_DETECTED: u32 = 2;
const FATAL_RESET_REQUESTED: u32 = 3;
const MAX_JUMBO_MTU: u32 = 9234;
const ETHER_HDR_LEN: u32 = 14;
const ETHER_CRC_LEN: u32 = 4;
const DMACR: u32 = 0x02508;
const DMCTXTH: u32 = 0x03550;
const DMCTLX: u32 = 0x02514;
const DMCRTRH: u32 = 0x05dd0;
const FCRTC: u32 = 0x02170;
const PCIEMISC: u32 = 0x05bb8;
const FCRTC_RTH_COAL_MASK: u32 = 0x0003_fff0;
const FCRTC_RTH_COAL_SHIFT: u32 = 4;
const DMACR_DMACTHR_MASK: u32 = 0x00ff_0000;
const DMACR_DMACTHR_SHIFT: u32 = 16;
const DMACR_DMAC_LX_MASK: u32 = 0x3000_0000;
const DMACR_DMAC_EN: u32 = 0x8000_0000;
const DMCTLX_DCFLUSH_DIS: u32 = 0x8000_0000;
const PCIEMISC_LX_DECISION: u32 = 0x80;
const TXPBSIZE: u32 = 20408;
const STATUS_2P5_SKU: u32 = 0x1000;
const STATUS_2P5_SKU_OVER: u32 = 0x2000;
const LEDCTL: u32 = 0x00e00;
const LED1_MODE_MASK: u32 = 0x0000_0f00;
const LED1_MODE_SHIFT: u32 = 8;
const LED1_BLINK: u32 = 0x0000_8000;
const LED_MODE_ON: u32 = 0;
const AUTONEG_ADV_DEFAULT: u16 = 0x002f;
const ADVERTISE_10_HALF: u16 = 1;
const ADVERTISE_10_FULL: u16 = 2;
const ADVERTISE_100_HALF: u16 = 4;
const ADVERTISE_100_FULL: u16 = 8;
const ADVERTISE_1000_FULL: u16 = 0x20;
const ADVERTISE_2500_FULL: u16 = 0x80;
const WUS_WAKE: u32 = 0x05800;
const WUS_EXT_WAKE: u32 = 0x05804;
const WUFC_MAG: u32 = 2;
const WUFC_EX: u32 = 4;
const WUFC_MC: u32 = 8;
const WUC_PME_EN: u32 = 2;
const CTRL_ADVD3WUC: u32 = 0x0010_0000;
const PCI_COMMAND: u16 = 0x04;
const PCI_VENDOR: u16 = 0;
const PCI_DEVICE: u16 = 0x02;
const PCI_REVISION: u16 = 0x08;
const PCI_SUBVENDOR: u16 = 0x2c;
const PCI_SUBDEVICE: u16 = 0x2e;
const PCI_BUSMASTER_ENABLE: u32 = 0x4;
const L1SS_CONTROL1: u16 = 0x08;
const CTRL_EXT: u32 = 0x00018;
const CTRL_EXT_DRV_LOAD: u32 = 0x1000_0000;
const CRCERRS: u32 = 0x04000;
const RXERRC: u32 = 0x0400c;
const MPC: u32 = 0x04010;
const SCC: u32 = 0x04014;
const ECOL: u32 = 0x04018;
const MCC: u32 = 0x0401c;
const LATECOL: u32 = 0x04020;
const COLC: u32 = 0x04028;
const RERC: u32 = 0x0402c;
const DC: u32 = 0x04030;
const RLEC: u32 = 0x04040;
const XONRXC: u32 = 0x04048;
const XONTXC: u32 = 0x0404c;
const XOFFRXC: u32 = 0x04050;
const XOFFTXC: u32 = 0x04054;
const FCRUC: u32 = 0x04058;
const PRC64: u32 = 0x0405c;
const PRC127: u32 = 0x04060;
const PRC255: u32 = 0x04064;
const PRC511: u32 = 0x04068;
const PRC1023: u32 = 0x0406c;
const PRC1522: u32 = 0x04070;
const TLPIC: u32 = 0x04148;
const RLPIC: u32 = 0x0414c;
const GPRC: u32 = 0x04074;
const BPRC: u32 = 0x04078;
const MPRC: u32 = 0x0407c;
const GPTC: u32 = 0x04080;
const GORCL: u32 = 0x04088;
const GORCH: u32 = 0x0408c;
const GOTCL: u32 = 0x04090;
const GOTCH: u32 = 0x04094;
const RNBC: u32 = 0x040a0;
const RUC: u32 = 0x040a4;
const RFC: u32 = 0x040a8;
const ROC: u32 = 0x040ac;
const RJC: u32 = 0x040b0;
const MGTPRC: u32 = 0x040b4;
const MGTPDC: u32 = 0x040b8;
const MGTPTC: u32 = 0x040bc;
const TORL: u32 = 0x040c0;
const TORH: u32 = 0x040c4;
const TOTL: u32 = 0x040c8;
const TOTH: u32 = 0x040cc;
const TPR: u32 = 0x040d0;
const TPT: u32 = 0x040d4;
const PTC64: u32 = 0x040d8;
const PTC127: u32 = 0x040dc;
const PTC255: u32 = 0x040e0;
const PTC511: u32 = 0x040e4;
const PTC1023: u32 = 0x040e8;
const PTC1522: u32 = 0x040ec;
const MPTC: u32 = 0x040f0;
const BPTC: u32 = 0x040f4;
const IAC: u32 = 0x04100;
const RXDMTC: u32 = 0x04120;
const ALGNERRC: u32 = 0x04004;
const TNCRS: u32 = 0x04034;
const HTDPMC: u32 = 0x0403c;
const TSCTC: u32 = 0x040f8;
const DTXTCPFLGL: u32 = 0x0359c;
const DTXTCPFLGH: u32 = 0x035a0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MainError {
    Bounds,
    Io,
}
pub trait IgcMainIo {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn admin_status_deferred(&mut self);
    fn update_mc(&mut self, addresses: &[u8], count: u32) -> Result<(), MainError>;
    fn write_vfta(&mut self, index: u32, value: u32);
}
#[derive(Debug)]
pub struct AimCounters {
    pub snapshot: AtomicU64,
    pub bytes_last: u32,
    pub packets_last: u32,
}
impl Default for AimCounters {
    fn default() -> Self {
        Self {
            snapshot: AtomicU64::new(0),
            bytes_last: 0,
            packets_last: 0,
        }
    }
}
#[derive(Debug)]
pub struct AimRxQueue {
    pub counters: AimCounters,
    pub vector: u16,
    pub eitr_setting: u32,
    pub interrupts: u64,
}
#[derive(Debug)]
pub struct AimTxQueue {
    pub counters: AimCounters,
    pub vector: u16,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AimDevice {
    pub enabled: u8,
    pub max_interrupt_rate: u32,
    pub link_speed_mbps: u32,
    pub max_frame_size: u32,
    pub packet_buffer_kb: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartEvent {
    VlanChange,
    MtuChange,
    CapabilitiesChange,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingDma {
    pub bus_address: u64,
    pub descriptors: usize,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitConfig {
    pub tx_rings: Vec<RingDma>,
    pub rx_rings: Vec<RingDma>,
    pub max_frame_size: u32,
    pub mtu: u32,
    pub rx_buffer_size: u32,
    pub vlan_trunk: bool,
    pub disable_crc_stripping: bool,
    pub rx_checksum: bool,
    pub flow_mode: super::mac::FlowMode,
    pub multicast_filter_type: u8,
    pub low_water: u32,
    pub high_water: u32,
    pub send_xon: bool,
}
pub trait IgcResetIo: IgcMainIo {
    fn restore_led(&mut self);
    fn get_hw_control(&mut self);
    fn reset_hw(&mut self) -> Result<(), MainError>;
    fn init_hw(&mut self) -> Result<(), MainError>;
    fn finish_fatal_error_reset(&mut self);
    fn init_dmac(&mut self, pba: u32, dmac: u32);
    fn get_phy_info(&mut self);
    fn check_for_link(&mut self);
    fn log_reset_error(&mut self, stage: &'static str);
}
pub trait IgcRssIo: IgcMainIo {
    fn rss_bucket(&mut self, bucket: usize, queue_count: usize) -> usize;
    fn rss_key(&mut self) -> [u32; 10];
    fn rss_hash_config(&mut self) -> u32;
}
pub trait IgcLifecycleIo: IgcMainIo {
    fn restore_led_for_stop(&mut self);
    fn prepare_fatal_error_reset(&mut self);
    fn stop_reset_hw(&mut self) -> Result<(), MainError>;
    fn finish_stop_fatal_error_reset(&mut self);
    fn enable_wakeup(&mut self) -> Result<(), MainError>;
    fn release_hw_control(&mut self);
    fn disable_broken_l1_2(&mut self);
    fn log_wakeup_status(&mut self, wus: u32, wus_ext: u32);
    fn clear_pme(&mut self);
    fn log_reset_failure(&mut self);
    fn log_wakeup_failure(&mut self);
}
pub trait IgcIfInitIo: IgcMainIo {
    fn enable_pci_busmaster(&mut self) -> Result<(), MainError>;
    fn suspend_link_powered_down(&self) -> bool;
    fn power_up_wakeup_link(&mut self);
    fn reset_adapter(&mut self) -> Result<(), MainError>;
    fn update_admin_status(&mut self);
    fn init_failed(&mut self);
    fn log_busmaster_failure(&mut self);
    fn set_mac_address(&mut self, address: [u8; 6]);
}
pub trait IgcAdminIo: IgcMainIo {
    fn fatal_error_admin(&mut self) -> bool;
    fn is_copper(&self) -> bool;
    fn is_unknown_media(&self) -> bool;
    fn get_link_status(&self) -> bool;
    fn check_for_link(&mut self);
    fn get_speed_duplex(&mut self) -> (u16, u16);
    fn set_link_state(&mut self, up: bool, speed_mbps: u16);
    fn set_link_fields(&mut self, active: bool, speed: u16, duplex: u16);
    fn apply_i225_ipg_workaround(&mut self);
    fn update_stats_counters(&mut self);
}
#[derive(Debug, Default)]
pub struct FatalErrorState {
    pub state: AtomicU32,
    pub peind: u32,
    pub pcie_error: u32,
    pub lan_error: u32,
    pub mng_error: u32,
    pub lan_parity_count: u64,
    pub mng_parity_count: u64,
    pub pcie_parity_count: u64,
    pub dma_parity_count: u64,
}
pub trait IgcFatalIo: IgcMainIo {
    fn delay_ms(&mut self, ms: u32);
    fn disable_pcie_master(&mut self) -> Result<(), MainError>;
    fn log_parity_reset_timeout(&mut self);
    fn log_master_disable_failure(&mut self);
}
pub trait IgcInterruptIo: IgcMainIo {
    fn disable_interrupts(&mut self);
}
pub trait IgcPciIo {
    fn read_config(&mut self, offset: u16, width: u8) -> u32;
    fn write_config(&mut self, offset: u16, width: u8, value: u32);
    fn enable_busmaster(&mut self) -> Result<(), MainError>;
    fn find_l1ss_capability(&mut self) -> Option<u16>;
    fn l1ss_aspm_l12_mask(&self) -> u32;
    fn l1ss_pcipm_l12_mask(&self) -> u32;
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IfStats {
    pub colc: u64,
    pub rxerrc: u64,
    pub crcerrs: u64,
    pub algnerrc: u64,
    pub ruc: u64,
    pub rfc: u64,
    pub roc: u64,
    pub mpc: u64,
    pub ecol: u64,
    pub latecol: u64,
    pub dropped_pkts: u64,
    pub scc: u64,
    pub mcc: u64,
    pub dc: u64,
    pub rerc: u64,
    pub rlec: u64,
    pub xonrxc: u64,
    pub xontxc: u64,
    pub xoffrxc: u64,
    pub xofftxc: u64,
    pub fcruc: u64,
    pub prc64: u64,
    pub prc127: u64,
    pub prc255: u64,
    pub prc511: u64,
    pub prc1023: u64,
    pub prc1522: u64,
    pub tlpic: u64,
    pub rlpic: u64,
    pub gprc: u64,
    pub bprc: u64,
    pub mprc: u64,
    pub gptc: u64,
    pub gorc: u64,
    pub gotc: u64,
    pub rnbc: u64,
    pub rjc: u64,
    pub mgprc: u64,
    pub mgpdc: u64,
    pub mgptc: u64,
    pub tor: u64,
    pub tot: u64,
    pub tpr: u64,
    pub tpt: u64,
    pub ptc64: u64,
    pub ptc127: u64,
    pub ptc255: u64,
    pub ptc511: u64,
    pub ptc1023: u64,
    pub ptc1522: u64,
    pub mptc: u64,
    pub bptc: u64,
    pub iac: u64,
    pub rxdmtc: u64,
    pub tncrs: u64,
    pub htdpmc: u64,
    pub tsctc: u64,
    pub xoff_pause_observed: bool,
    pub corrected_error_dma_count: u64,
    pub corrected_error_pcie_tx_data_count: u64,
    pub corrected_error_pcie_retry_count: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IfCounter {
    Collisions,
    InputErrors,
    OutputErrors,
    Other(u8),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LedState {
    pub active: bool,
    pub default: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSubtype {
    Auto,
    Speed2500,
    Speed1000,
    Speed100,
    Speed10,
    Other,
}

/// Mutable flow-control policy used by the FreeBSD `igc_set_flowcntl`
/// sysctl callback.  The sysctl registration itself belongs to the host
/// configuration framework, but validation and reinitialization policy do
/// not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowControlSetting {
    pub mode: super::mac::FlowMode,
    pub interface_up: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowControlRequestError {
    InvalidMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmacSetting {
    /// `0` disables coalescing; all nonzero values are the timer in usec.
    pub timer_us: u32,
    pub interface_up: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmacRequestError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EeeSetting {
    pub disabled: bool,
    pub interface_up: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TsoFlagBank {
    LowLower,
    LowUpper,
    HighLower,
}

/// The return value records whether FreeBSD's callback would request an
/// iflib reinitialization.  The local NetDriverOps does not expose a generic
/// deferred-reset scheduler; its owner can apply the new mode on its next
/// initialization instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowControlUpdate {
    pub changed: bool,
    pub request_reinit: bool,
}

// upstream: if_igc.c igc_set_flowcntl()
pub fn igc_set_flowcntl(
    setting: &mut FlowControlSetting,
    input: i32,
) -> Result<FlowControlUpdate, FlowControlRequestError> {
    let mode = match input {
        0 => super::mac::FlowMode::None,
        1 => super::mac::FlowMode::RxPause,
        2 => super::mac::FlowMode::TxPause,
        3 => super::mac::FlowMode::Full,
        _ => return Err(FlowControlRequestError::InvalidMode),
    };

    if mode == setting.mode {
        return Ok(FlowControlUpdate {
            changed: false,
            request_reinit: false,
        });
    }

    setting.mode = mode;
    Ok(FlowControlUpdate {
        changed: true,
        request_reinit: setting.interface_up,
    })
}

// upstream: if_igc.c igc_sysctl_dmac()
pub fn igc_sysctl_dmac(setting: &mut DmacSetting, input: i32) -> Result<bool, DmacRequestError> {
    let requested = match input {
        0 => 0,
        1 => 1000,
        250 | 500 => input as u32,
        1000..=10000 if input % 1000 == 0 => input as u32,
        _ => {
            setting.timer_us = 0;
            return Err(DmacRequestError);
        }
    };
    setting.timer_us = requested;
    Ok(igc_sysctl_request_reinit(setting.interface_up))
}

// upstream: if_igc.c igc_sysctl_eee()
pub fn igc_sysctl_eee(setting: &mut EeeSetting, input: i32) -> bool {
    let disabled = input != 0;
    setting.disabled = disabled;
    igc_sysctl_request_reinit(setting.interface_up)
}

// upstream: if_igc.c igc_sysctl_request_reinit()
pub const fn igc_sysctl_request_reinit(interface_up: bool) -> bool {
    interface_up
}

// upstream: if_igc.c igc_sysctl_interrupt_rate_handler()
pub fn igc_sysctl_interrupt_rate_handler<I: IgcMainIo>(io: &mut I, vector: u16) -> u32 {
    let value = io.read(EITR_BASE + u32::from(vector) * 4) & EITR_QVECTOR_MASK;
    if value == 0 {
        0
    } else {
        (EITR_DIVIDEND << EITR_SHIFT) / value
    }
}

// upstream: if_igc.c igc_sysctl_reg_handler()
pub fn igc_sysctl_reg_handler<I: IgcMainIo>(io: &mut I, register: u32) -> u32 {
    io.read(register)
}

// upstream: if_igc.c igc_sysctl_tso_tcp_flags_mask()
pub fn igc_sysctl_tso_tcp_flags_mask<I: IgcMainIo>(
    io: &mut I,
    bank: TsoFlagBank,
    mask: i32,
) -> Result<(), MainError> {
    if !(0..=0x0fff).contains(&mask) {
        return Err(MainError::Bounds);
    }
    let (register, shift) = match bank {
        TsoFlagBank::LowLower => (DTXTCPFLGL, 0),
        TsoFlagBank::LowUpper => (DTXTCPFLGL, 16),
        TsoFlagBank::HighLower => (DTXTCPFLGH, 0),
    };
    let value = io.read(register);
    io.write(
        register,
        (value & !(0x0fff << shift)) | ((mask as u32) << shift),
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaStatus {
    pub valid: bool,
    pub active: bool,
    pub speed_mbps: u16,
    pub full_duplex: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeFilters {
    pub magic: bool,
    pub unicast: bool,
    pub multicast: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeCapabilities {
    pub wol: bool,
    pub enabled: WakeFilters,
}
pub trait IgcWakeupIo: IgcMainIo {
    fn pme_d3_hot_supported(&mut self) -> bool;
    fn enabled_wake_filters(&self) -> WakeFilters;
    fn set_wake_capabilities(&mut self, caps: WakeCapabilities);
    fn management_passthrough(&mut self) -> bool;
    fn multicast_addresses(&mut self) -> Vec<u8>;
    fn update_multicast(&mut self, addresses: &[u8], count: u32) -> Result<(), MainError>;
    fn current_mac_address(&self) -> [u8; 6];
    fn set_mac_address(&mut self, address: [u8; 6]);
    fn rar_set(&mut self, address: [u8; 6], index: u32) -> Result<(), MainError>;
    fn power_up_phy(&mut self);
    fn power_down_phy(&mut self);
    fn suspend_link_powered_down(&self) -> bool;
    fn set_suspend_link_powered_down(&mut self, value: bool);
    fn enable_pme(&mut self);
    fn clear_pme(&mut self);
    fn disable_pcie_master(&mut self) -> Result<(), MainError>;
    fn disable_busmaster(&mut self) -> Result<(), MainError>;
    fn log_wakeup_error(&mut self);
    fn log_pcie_disable_error(&mut self);
    fn log_busmaster_disable_error(&mut self);
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachPlan {
    pub max_queues: usize,
    pub tx_descriptor_count: usize,
    pub rx_descriptor_count: usize,
    pub descriptor_bytes: usize,
    pub tx_queue_bytes: usize,
    pub rx_queue_bytes: usize,
    pub max_scatter: usize,
    pub max_tso_size: usize,
    pub max_tso_segment_size: usize,
    pub msix_bar: u16,
    pub max_frame_size: u32,
    pub flow_mode: super::mac::FlowMode,
    pub mac_autoneg: bool,
    pub autoneg_advertised: u16,
    pub autoneg_wait: bool,
    pub mtu: u32,
    pub eee_disable: bool,
}
pub trait IgcAttachIo {
    fn sysctl_register(&mut self);
    fn identify_hardware(&mut self) -> Result<(), MainError>;
    fn disable_broken_l1_2(&mut self);
    fn configure_driver_context(&mut self, plan: &AttachPlan);
    fn msix_bar(&mut self) -> u16;
    fn read_config(&mut self, offset: u16, width: u8) -> u32;
    fn allocate_pci_resources(&mut self) -> Result<(), MainError>;
    fn shared_code_init(&mut self) -> Result<(), MainError>;
    fn setup_msix(&mut self);
    fn get_bus_info(&mut self);
    fn allocate_multicast_buffer(&mut self) -> bool;
    fn check_reset_block(&mut self) -> bool;
    fn reset_hardware(&mut self) -> Result<(), MainError>;
    fn validate_nvm_checksum(&mut self) -> Result<(), MainError>;
    fn read_mac_address(&mut self) -> Result<[u8; 6], MainError>;
    fn read_firmware_version(&mut self);
    fn configure_wakeup(&mut self);
    fn set_mac_address(&mut self, address: [u8; 6]);
    fn release_hw_control(&mut self);
    fn free_multicast_buffer(&mut self);
    fn free_pci_resources(&mut self);
    fn setup_interface(&mut self) -> Result<(), MainError>;
    fn reset(&mut self) -> Result<(), MainError>;
    fn update_stats_counters(&mut self);
    fn mark_link_status_stale(&mut self);
    fn update_admin_status(&mut self);
    fn add_hw_stats(&mut self);
    fn reset_phy(&mut self);
}
pub trait IfCounterIo {
    fn default_counter(&mut self, counter: IfCounter) -> u64;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptState {
    pub msix: bool,
    pub queue_mask: u32,
    pub link_mask: u32,
    pub link_interrupts: u64,
    pub rx_overruns: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptResult {
    Stray,
    ScheduleThread,
    Handled,
}

// upstream: if_igc.c igc_if_intr_enable()
pub fn igc_if_intr_enable<I: IgcMainIo>(
    io: &mut I,
    state: &InterruptState,
    fatal: &FatalErrorState,
) {
    let mut mask;
    if state.msix {
        mask = state.queue_mask | state.link_mask;
        io.write(EIAC, mask);
        io.write(EIAM, mask);
        io.write(EIMS, mask);
        mask = IMS_LSC
    } else {
        mask = IMS_ENABLE_MASK
    }
    if fatal.state.load(Ordering::Acquire) == FATAL_NONE {
        mask |= IMS_FER
    }
    io.write(IMS, mask)
}
// upstream: if_igc.c igc_if_intr_disable()
pub fn igc_if_intr_disable<I: IgcMainIo>(io: &mut I, msix: bool) {
    if msix {
        io.write(EIMC, u32::MAX);
        io.write(EIAC, 0)
    }
    io.write(IMC, u32::MAX)
}
// upstream: if_igc.c igc_if_rx_queue_intr_enable()
pub fn igc_if_rx_queue_intr_enable<I: IgcMainIo>(io: &mut I, eims: u32) {
    io.write(EIMS, eims)
}
// upstream: if_igc.c igc_if_tx_queue_intr_enable()
pub fn igc_if_tx_queue_intr_enable<I: IgcMainIo>(io: &mut I, eims: u32) {
    io.write(EIMS, eims)
}
// upstream: if_igc.c igc_handle_link()
pub fn igc_handle_link<I: IgcMainIo>(io: &mut I, mac: &mut MacState) {
    mac.get_link_status = true;
    io.admin_status_deferred()
}

// upstream: if_igc.c igc_handle_fatal_error_intr()
pub fn igc_handle_fatal_error_intr<I: IgcMainIo>(
    io: &mut I,
    fatal: &mut FatalErrorState,
    icr: u32,
) {
    if icr & ICR_FER == 0 {
        return;
    }
    io.write(IMC, IMS_FER);
    if fatal
        .state
        .compare_exchange(
            FATAL_NONE,
            FATAL_CAPTURING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    let mut peind = io.read(PEIND) & PEIND_FATAL_MASK;
    let pcie = io.read(PCIEERRSTS) & PCIEERRSTS_FATAL_MASK;
    let lan = io.read(LANPERRSTS) & LANPERRSTS_RETX_BUF;
    let mng = io.read(MNGPARSTS) & MNGPARSTS_FATAL_MASK;
    if pcie != 0 {
        peind |= PEIND_PCIE_PARITY_FATAL
    }
    if lan != 0 {
        peind |= PEIND_LANPORT_PARITY_FATAL
    }
    fatal.peind = peind;
    fatal.pcie_error = pcie;
    fatal.lan_error = lan;
    fatal.mng_error = mng;
    fatal.state.store(FATAL_DETECTED, Ordering::Release);
    io.admin_status_deferred()
}
// upstream: if_igc.c igc_handle_fatal_error_admin()
pub fn igc_handle_fatal_error_admin(fatal: &mut FatalErrorState) -> bool {
    if fatal
        .state
        .compare_exchange(
            FATAL_DETECTED,
            FATAL_RESET_REQUESTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return fatal.state.load(Ordering::Acquire) != FATAL_NONE;
    }
    if fatal.peind & PEIND_LANPORT_PARITY_FATAL != 0 {
        fatal.lan_parity_count += 1
    }
    if fatal.peind & PEIND_MNG_PARITY_FATAL != 0 {
        fatal.mng_parity_count += 1
    }
    if fatal.peind & PEIND_PCIE_PARITY_FATAL != 0 {
        fatal.pcie_parity_count += 1
    }
    if fatal.peind & PEIND_DMA_PARITY_FATAL != 0 {
        fatal.dma_parity_count += 1
    }
    true
}
// upstream: if_igc.c igc_intr()
pub fn igc_intr<I: IgcInterruptIo>(
    io: &mut I,
    interrupts: &mut InterruptState,
    mac: &mut MacState,
    fatal: &mut FatalErrorState,
    device: &AimDevice,
    rx: &mut AimRxQueue,
    tx: &mut [AimTxQueue],
) -> InterruptResult {
    let icr = io.read(ICR);
    if icr == u32::MAX || icr == 0 || icr & ICR_INT_ASSERTED == 0 {
        return InterruptResult::Stray;
    }
    io.disable_interrupts();
    if icr & (ICR_RXSEQ | ICR_LSC) != 0 {
        igc_handle_link(io, mac)
    }
    if icr & ICR_RXO != 0 {
        interrupts.rx_overruns += 1
    }
    igc_handle_fatal_error_intr(io, fatal, icr);
    igc_neweitr(io, device, rx, tx);
    InterruptResult::ScheduleThread
}
// upstream: if_igc.c igc_msix_que()
pub fn igc_msix_que<I: IgcMainIo>(
    io: &mut I,
    device: &AimDevice,
    rx: &mut AimRxQueue,
    tx: &mut [AimTxQueue],
) -> InterruptResult {
    rx.interrupts += 1;
    igc_neweitr(io, device, rx, tx);
    InterruptResult::ScheduleThread
}
// upstream: if_igc.c igc_msix_link()
pub fn igc_msix_link<I: IgcMainIo>(
    io: &mut I,
    interrupts: &mut InterruptState,
    mac: &mut MacState,
    fatal: &mut FatalErrorState,
) -> InterruptResult {
    interrupts.link_interrupts += 1;
    let icr = io.read(ICR);
    if icr & ICR_RXO != 0 {
        interrupts.rx_overruns += 1
    }
    if icr & (ICR_RXSEQ | ICR_LSC) != 0 {
        igc_handle_link(io, mac)
    }
    igc_handle_fatal_error_intr(io, fatal, icr);
    let mut mask = IMS_LSC;
    if fatal.state.load(Ordering::Acquire) == FATAL_NONE {
        mask |= IMS_FER
    }
    io.write(IMS, mask);
    io.write(EIMS, interrupts.link_mask);
    InterruptResult::Handled
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueInterrupt {
    pub vector: u8,
    pub eims: u32,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterruptTopology {
    pub rx: Vec<QueueInterrupt>,
    pub tx: Vec<QueueInterrupt>,
    pub link_vector: u8,
    pub queue_mask: u32,
    pub link_mask: u32,
}
// upstream: if_igc.c igc_configure_queues()
pub fn igc_configure_queues<I: IgcMainIo>(io: &mut I, topology: &mut InterruptTopology) {
    io.write(GPIE, GPIE_MSIX_MODE | GPIE_EIAME | GPIE_PBA | GPIE_NSICR);
    for (index, q) in topology.rx.iter().enumerate() {
        let reg = IVAR0 + ((index >> 1) as u32) * 4;
        let mut ivar = io.read(reg);
        if index & 1 != 0 {
            ivar &= 0xff00_ffff;
            ivar |= (u32::from(q.vector) | IVAR_VALID) << 16
        } else {
            ivar &= 0xffff_ff00;
            ivar |= u32::from(q.vector) | IVAR_VALID
        }
        io.write(reg, ivar)
    }
    for (index, q) in topology.tx.iter().enumerate() {
        let reg = IVAR0 + ((index >> 1) as u32) * 4;
        let mut ivar = io.read(reg);
        if index & 1 != 0 {
            ivar &= 0x00ff_ffff;
            ivar |= (u32::from(q.vector) | IVAR_VALID) << 24
        } else {
            ivar &= 0xffff_00ff;
            ivar |= (u32::from(q.vector) | IVAR_VALID) << 8
        }
        io.write(reg, ivar);
        topology.queue_mask |= q.eims
    }
    io.write(
        IVAR_MISC,
        (u32::from(topology.link_vector) | IVAR_VALID) << 8,
    );
    topology.link_mask = 1u32 << topology.link_vector;
}
// upstream: if_igc.c igc_initialize_interrupt_rate()
pub fn igc_initialize_interrupt_rate<I: IgcMainIo>(
    io: &mut I,
    rx: &mut [AimRxQueue],
    max_interrupt_rate: u32,
) {
    let rate = if max_interrupt_rate == 0 {
        0
    } else {
        ((EITR_DIVIDEND / max_interrupt_rate) << EITR_SHIFT) & EITR_QVECTOR_MASK
    };
    let value = rate | EITR_CNT_IGNR;
    for q in rx {
        q.eitr_setting = value;
        io.write(EITR_BASE + u32::from(q.vector) * 4, value)
    }
}

// upstream: if_igc.c igc_prepare_fatal_error_reset()
pub fn igc_prepare_fatal_error_reset<I: IgcFatalIo>(io: &mut I, fatal: &FatalErrorState) {
    if fatal.state.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut pcie_error = fatal.pcie_error | (io.read(PCIEERRSTS) & PCIEERRSTS_FATAL_MASK);
    if fatal.peind & PEIND_PCIE_PARITY_FATAL == 0 && pcie_error == 0 {
        return;
    }
    let ctrl = io.read(CTRL);
    io.write(CTRL, ctrl | CTRL_DEV_RST);
    io.delay_ms(3);
    let mut reset_done = false;
    for _ in 0..10 {
        if io.read(EECD) & EECD_AUTO_RD != 0 && io.read(STATUS) & STATUS_RST_DONE != 0 {
            reset_done = true;
            break;
        }
        io.delay_ms(1)
    }
    if !reset_done {
        io.log_parity_reset_timeout()
    }
    if io.disable_pcie_master().is_err() {
        io.log_master_disable_failure()
    }
    pcie_error |= io.read(PCIEERRSTS) & PCIEERRSTS_FATAL_MASK;
    if pcie_error != 0 {
        io.write(PCIEERRSTS, pcie_error)
    }
}

// upstream: if_igc.c igc_finish_fatal_error_reset()
pub fn igc_finish_fatal_error_reset<I: IgcMainIo>(io: &mut I, fatal: &mut FatalErrorState) {
    if fatal.state.load(Ordering::Acquire) == 0 {
        return;
    }
    let pcie_error = fatal.pcie_error | (io.read(PCIEERRSTS) & PCIEERRSTS_FATAL_MASK);
    if pcie_error != 0 {
        io.write(PCIEERRSTS, pcie_error)
    }
    let lan_error = fatal.lan_error | (io.read(LANPERRSTS) & LANPERRSTS_RETX_BUF);
    if lan_error != 0 {
        io.write(LANPERRSTS, lan_error)
    }
    let _ = io.read(PEIND);
    fatal.peind = 0;
    fatal.pcie_error = 0;
    fatal.lan_error = 0;
    fatal.mng_error = 0;
    fatal.state.store(0, Ordering::Release)
}

// upstream: if_igc.c igc_reset()
pub fn igc_reset<I: IgcResetIo>(
    io: &mut I,
    mac: &mut MacState,
    config: &UnitConfig,
    dmac: u32,
) -> Result<u32, MainError> {
    io.restore_led();
    io.get_hw_control();
    let pba = PBA_34K;
    let rx_buffer = (pba & 0xffff) << 10;
    let rounded = (config.max_frame_size + 1023) & !1023;
    let high = rx_buffer.wrapping_sub(rounded);
    mac.flow.high_water = high;
    mac.flow.low_water = high.wrapping_sub(16);
    mac.flow.requested = config.flow_mode;
    mac.flow.current = config.flow_mode;
    mac.flow.pause_time = FC_PAUSE_TIME;
    mac.flow.send_xon = true;
    if let Err(e) = io.reset_hw() {
        io.log_reset_error("reset");
        return Err(e);
    }
    io.write(0x05814, 0);
    if let Err(e) = io.init_hw() {
        io.log_reset_error("init");
        return Err(e);
    }
    io.finish_fatal_error_reset();
    io.init_dmac(pba, dmac);
    io.write(VET, 0x8100);
    io.get_phy_info();
    io.check_for_link();
    Ok(pba)
}

// upstream: if_igc.c igc_initialize_rss_mapping()
pub fn igc_initialize_rss_mapping<I: IgcRssIo>(io: &mut I, queue_count: usize) {
    const HASH_IPV4: u32 = 1 << 0;
    const HASH_TCP_IPV4: u32 = 1 << 1;
    const HASH_IPV6: u32 = 1 << 2;
    const HASH_TCP_IPV6: u32 = 1 << 3;
    const HASH_TCP_IPV6_EX: u32 = 1 << 4;
    const HASH_UDP_IPV4: u32 = 1 << 5;
    const HASH_UDP_IPV6: u32 = 1 << 6;
    const HASH_UDP_IPV6_EX: u32 = 1 << 7;
    const MRQC_IPV4_TCP: u32 = 0x0001_0000;
    const MRQC_IPV4: u32 = 0x0002_0000;
    const MRQC_IPV6_TCP_EX: u32 = 0x0004_0000;
    const MRQC_IPV6: u32 = 0x0010_0000;
    const MRQC_IPV6_TCP: u32 = 0x0020_0000;
    const MRQC_IPV4_UDP: u32 = 0x0040_0000;
    const MRQC_IPV6_UDP: u32 = 0x0080_0000;
    const MRQC_IPV6_UDP_EX: u32 = 0x0100_0000;
    if queue_count == 0 {
        return;
    }
    let mut reta = 0;
    for i in 0..128 {
        let queue = io.rss_bucket(i, queue_count) % queue_count;
        reta >>= 8;
        reta |= (queue as u32) << 24;
        if i & 3 == 3 {
            io.write(RETA + ((i >> 2) as u32) * 4, reta);
            reta = 0
        }
    }
    let key = io.rss_key();
    for (i, word) in key.into_iter().enumerate() {
        io.write(RSSRK + (i as u32) * 4, word)
    }
    let features = io.rss_hash_config();
    let mut mrqc = MRQC_ENABLE_RSS_4Q;
    for (enabled, mask) in [
        (HASH_IPV4, MRQC_IPV4),
        (HASH_TCP_IPV4, MRQC_IPV4_TCP),
        (HASH_IPV6, MRQC_IPV6),
        (HASH_TCP_IPV6, MRQC_IPV6_TCP),
        (HASH_TCP_IPV6_EX, MRQC_IPV6_TCP_EX),
        (HASH_UDP_IPV4, MRQC_IPV4_UDP),
        (HASH_UDP_IPV6, MRQC_IPV6_UDP),
        (HASH_UDP_IPV6_EX, MRQC_IPV6_UDP_EX),
    ] {
        if features & enabled != 0 {
            mrqc |= mask
        }
    }
    io.write(MRQC, mrqc)
}

// upstream: if_igc.c igc_initialize_transmit_unit()
pub fn igc_initialize_transmit_unit<I: IgcMainIo>(
    io: &mut I,
    config: &UnitConfig,
) -> Result<(), MainError> {
    for (index, ring) in config.tx_rings.iter().enumerate() {
        let base = TDBAL + (index as u32) * 0x100;
        io.write(
            TDLEN + (index as u32) * 0x100,
            (ring.descriptors * 16) as u32,
        );
        io.write(
            TDBAH + (index as u32) * 0x100,
            (ring.bus_address >> 32) as u32,
        );
        io.write(base, ring.bus_address as u32);
        io.write(TDT + (index as u32) * 0x100, 0);
        io.write(TDH + (index as u32) * 0x100, 0);
        let txdctl = TX_PTHRESH | (TX_HTHRESH << 8) | TXDCTL_QUEUE_ENABLE;
        io.write(TXDCTL + (index as u32) * 0x100, txdctl)
    }
    let mut tctl = io.read(0x00400);
    tctl &= !TCTL_CT;
    tctl |= TCTL_PSP | TCTL_RTLC | TCTL_EN | (COLLISION_THRESHOLD << TCTL_CT_SHIFT);
    io.write(0x00400, tctl);
    Ok(())
}

// upstream: if_igc.c igc_initialize_receive_unit()
pub fn igc_initialize_receive_unit<I: IgcMainIo + IgcRssIo>(
    io: &mut I,
    config: &UnitConfig,
) -> Result<(), MainError> {
    let mut rctl = io.read(RCTL);
    io.write(RCTL, rctl & !RCTL_EN);
    rctl &= !(3 << RCTL_MO_SHIFT);
    rctl |= RCTL_EN | RCTL_BAM | (u32::from(config.multicast_filter_type) << RCTL_MO_SHIFT);
    rctl &= !RCTL_SBP;
    if config.mtu > 1500 {
        rctl |= RCTL_LPE
    } else {
        rctl &= !RCTL_LPE
    }
    if !config.disable_crc_stripping {
        rctl |= RCTL_SECRC
    }
    let mut rxcsum = io.read(RXCSUM);
    if config.rx_checksum {
        rxcsum |= RXCSUM_CRCOFL;
        if config.tx_rings.len() > 1 {
            rxcsum |= RXCSUM_PCSD
        } else {
            rxcsum |= RXCSUM_IPPCSE
        }
    } else if config.tx_rings.len() > 1 {
        rxcsum |= RXCSUM_PCSD
    } else {
        rxcsum &= !RXCSUM_TUOFL
    }
    io.write(RXCSUM, rxcsum);
    if config.rx_rings.len() > 1 {
        igc_initialize_rss_mapping(io, config.rx_rings.len())
    }
    if config.mtu > 1500 {
        let psize = config.max_frame_size + if config.vlan_trunk { 4 } else { 0 };
        io.write(RLPML, psize)
    }
    let mut srrctl =
        (config.rx_buffer_size + ((1 << SRRCTL_BSIZEPKT_SHIFT) - 1)) >> SRRCTL_BSIZEPKT_SHIFT;
    let _ = RCTL_SZ_2048;
    rctl |= 0;
    if config.rx_rings.len() > 1
        && matches!(
            config.flow_mode,
            super::mac::FlowMode::None | super::mac::FlowMode::RxPause
        )
    {
        srrctl |= SRRCTL_DROP_EN
    }
    srrctl |= SRRCTL_DESCTYPE_ADV_ONEBUF;
    for (index, ring) in config.rx_rings.iter().enumerate() {
        let q = index as u32;
        io.write(RDLEN + q * 0x100, (ring.descriptors * 16) as u32);
        io.write(RDBAH + q * 0x100, (ring.bus_address >> 32) as u32);
        io.write(RDBAL + q * 0x100, ring.bus_address as u32);
        io.write(SRRCTL + q * 0x100, srrctl);
        io.write(RDH + q * 0x100, 0);
        io.write(RDT + q * 0x100, 0);
        let mut rxdctl = io.read(RXDCTL + q * 0x100);
        rxdctl &= !(RXDCTL_PTHRESH | RXDCTL_HTHRESH | RXDCTL_WTHRESH);
        rxdctl |= 8 | (8 << 8) | (4 << 16) | RXDCTL_QUEUE_ENABLE;
        io.write(RXDCTL + q * 0x100, rxdctl)
    }
    rctl &= !RCTL_VFE;
    io.write(RCTL, rctl);
    Ok(())
}

// upstream: if_igc.c igc_init_dmac()
pub fn igc_init_dmac<I: IgcMainIo>(io: &mut I, dmac: u32, pba: u32, max_frame_size: u32) {
    if dmac == 0 {
        io.write(DMACR, !DMACR_DMAC_EN);
        return;
    }
    io.write(DMCTXTH, 0);
    let mut hwm = 64 * pba - max_frame_size / 16;
    if hwm < 64 * (pba - 6) {
        hwm = 64 * (pba - 6)
    }
    let mut reg = io.read(FCRTC);
    reg &= !FCRTC_RTH_COAL_MASK;
    reg |= (hwm << FCRTC_RTH_COAL_SHIFT) & FCRTC_RTH_COAL_MASK;
    io.write(FCRTC, reg);
    let mut threshold = pba - max_frame_size / 512;
    if threshold < pba - 10 {
        threshold = pba - 10
    }
    reg = io.read(DMACR);
    reg &= !DMACR_DMACTHR_MASK;
    reg |= (threshold << DMACR_DMACTHR_SHIFT) & DMACR_DMACTHR_MASK;
    reg |= DMACR_DMAC_EN | DMACR_DMAC_LX_MASK;
    let status = io.read(STATUS);
    if status & STATUS_2P5_SKU != 0 && status & STATUS_2P5_SKU_OVER == 0 {
        reg |= (dmac * 5) >> 6
    } else {
        reg |= dmac >> 5
    }
    io.write(DMACR, reg);
    io.write(DMCRTRH, 0);
    reg = io.read(DMCTLX) | DMCTLX_DCFLUSH_DIS;
    let status = io.read(STATUS);
    reg |= if status & STATUS_2P5_SKU != 0 && status & STATUS_2P5_SKU_OVER == 0 {
        0xa
    } else {
        4
    };
    io.write(DMCTLX, reg);
    io.write(DMCTXTH, (TXPBSIZE - 2 * max_frame_size) >> 6);
    reg = io.read(PCIEMISC) & !PCIEMISC_LX_DECISION;
    io.write(PCIEMISC, reg)
}

// upstream: if_igc.c igc_aim_rx_delta()
pub fn igc_aim_rx_delta(c: &mut AimCounters) -> (u32, u32) {
    let snapshot = c.snapshot.load(Ordering::Acquire);
    let now_bytes = (snapshot >> 32) as u32;
    let now_packets = snapshot as u32;
    let db = now_bytes.wrapping_sub(c.bytes_last);
    let dp = now_packets.wrapping_sub(c.packets_last);
    c.bytes_last = now_bytes;
    c.packets_last = now_packets;
    (db, dp)
}
// upstream: if_igc.c igc_aim_tx_delta()
pub fn igc_aim_tx_delta(c: &mut AimCounters) -> (u32, u32) {
    let snapshot = c.snapshot.load(Ordering::Acquire);
    let now_bytes = (snapshot >> 32) as u32;
    let now_packets = snapshot as u32;
    let db = now_bytes.wrapping_sub(c.bytes_last);
    let dp = now_packets.wrapping_sub(c.packets_last);
    c.bytes_last = now_bytes;
    c.packets_last = now_packets;
    (db, dp)
}
// upstream: if_igc.c igc_ring_itr()
pub fn igc_ring_itr(
    device: &AimDevice,
    rx_bytes: u32,
    rx_packets: u32,
    tx_bytes: u32,
    tx_packets: u32,
) -> u32 {
    let mut value = 0;
    if txbytes_and_packets(tx_bytes, tx_packets) {
        value = tx_bytes / tx_packets
    }
    if rxbytes_and_packets(rx_bytes, rx_packets) {
        value = value.max(rx_bytes / rx_packets)
    }
    if value == 0 {
        return 0;
    }
    value = value.saturating_add(24).min(3000);
    value = if value > 300 && value < 1200 {
        value / 3
    } else {
        value / 2
    };
    value = if value == 0 {
        0
    } else {
        (EITR_DIVIDEND << EITR_SHIFT) / value
    };
    value.min(if device.enabled == 1 {
        INTRS_20K
    } else {
        INTRS_70K
    })
}
const INTRS_20K: u32 = INTS_20K;
const INTRS_70K: u32 = INTS_70K;
fn txbytes_and_packets(bytes: u32, packets: u32) -> bool {
    bytes != 0 && packets != 0
}
fn rxbytes_and_packets(bytes: u32, packets: u32) -> bool {
    bytes != 0 && packets != 0
}
// upstream: if_igc.c igc_neweitr()
pub fn igc_neweitr<I: IgcMainIo>(
    io: &mut I,
    device: &AimDevice,
    rx: &mut AimRxQueue,
    tx: &mut [AimTxQueue],
) {
    let (rx_bytes, rx_packets) = igc_aim_rx_delta(&mut rx.counters);
    let (mut tx_bytes, mut tx_packets) = (0u32, 0u32);
    for q in tx.iter_mut().filter(|q| q.vector == rx.vector) {
        let (b, p) = igc_aim_tx_delta(&mut q.counters);
        tx_bytes = tx_bytes.wrapping_add(b);
        tx_packets = tx_packets.wrapping_add(p)
    }
    if tx_bytes == 0 && rx_bytes == 0 {
        return;
    }
    let mut rate = if device.enabled == 0 {
        device.max_interrupt_rate
    } else if device.link_speed_mbps < 1000 {
        INTS_4K
    } else if device.max_frame_size.saturating_mul(2) > device.packet_buffer_kb << 10 {
        device.max_interrupt_rate
    } else {
        let observation = igc_ring_itr(device, rx_bytes, rx_packets, tx_bytes, tx_packets);
        if observation == 0 {
            return;
        }
        observation
    };
    rate = if rate == 0 {
        0
    } else {
        ((EITR_DIVIDEND / rate) << EITR_SHIFT) & EITR_QVECTOR_MASK
    };
    rate |= EITR_CNT_IGNR;
    if rate != rx.eitr_setting {
        rx.eitr_setting = rate;
        io.write(EITR_BASE + u32::from(rx.vector) * 4, rate)
    }
}
// upstream: if_igc.c igc_if_needs_restart()
pub fn igc_if_needs_restart(_event: RestartEvent) -> bool {
    false
}
// upstream: if_igc.c igc_apply_i225_ipg_workaround()
pub fn igc_apply_i225_ipg_workaround<I: IgcMainIo>(
    io: &mut I,
    is_i225: bool,
    revision: u8,
    link_speed_mbps: u32,
) {
    if !is_i225 || revision >= REVISION_2 {
        return;
    }
    let ipgt = if link_speed_mbps == 2500 {
        I225_IPGT_2P5
    } else {
        DEFAULT_IPGT_COPPER
    };
    let mut tipg = io.read(TIPG);
    if tipg & TIPG_IPGT_MASK == ipgt {
        return;
    }
    tipg &= !TIPG_IPGT_MASK;
    tipg |= ipgt;
    io.write(TIPG, tipg)
}
// upstream: if_igc.c igc_if_set_promisc()
pub fn igc_if_set_promisc<I: IgcMainIo>(
    io: &mut I,
    flags: u32,
    multicast_count: usize,
    allmulti: bool,
    debug_bad_packets: bool,
    vlan_filter_used: bool,
) -> Result<(), MainError> {
    let mut rctl = io.read(RCTL);
    rctl &= !(RCTL_SBP | RCTL_UPE);
    let mcnt = if allmulti {
        MAX_MULTICAST
    } else {
        multicast_count.min(MAX_MULTICAST)
    };
    if mcnt < MAX_MULTICAST {
        rctl &= !RCTL_MPE
    }
    let promisc = flags & IFF_PROMISC != 0;
    if promisc {
        rctl |= RCTL_UPE | RCTL_MPE;
        if debug_bad_packets {
            rctl |= RCTL_SBP
        }
    } else if allmulti {
        rctl |= RCTL_MPE;
        rctl &= !RCTL_UPE
    }
    if promisc || !vlan_filter_used {
        rctl &= !RCTL_VFE
    } else {
        rctl |= RCTL_VFE
    }
    io.write(RCTL, rctl);
    Ok(())
}
// upstream: if_igc.c igc_copy_maddr()
pub fn igc_copy_maddr(
    output: &mut [u8],
    address: [u8; 6],
    index: usize,
) -> Result<bool, MainError> {
    if index == MAX_MULTICAST {
        return Ok(false);
    }
    let start = index * 6;
    if start + 6 > output.len() {
        return Err(MainError::Bounds);
    }
    output[start..start + 6].copy_from_slice(&address);
    Ok(true)
}
// upstream: if_igc.c igc_if_multi_set()
pub fn igc_if_multi_set<I: IgcMainIo>(
    io: &mut I,
    mac: &MacState,
    packed: &[u8],
    count: usize,
    flags: u32,
    allmulti: bool,
    debug_bad_packets: bool,
) -> Result<(), MainError> {
    let mut rctl = io.read(RCTL);
    let mcnt = count.min(MAX_MULTICAST);
    let promisc = flags & IFF_PROMISC != 0;
    if promisc {
        rctl |= RCTL_UPE | RCTL_MPE;
        if debug_bad_packets {
            rctl |= RCTL_SBP
        }
    } else if mcnt >= MAX_MULTICAST || allmulti {
        rctl |= RCTL_MPE;
        rctl &= !RCTL_UPE
    } else {
        rctl &= !(RCTL_UPE | RCTL_MPE)
    }
    if mcnt < MAX_MULTICAST {
        io.update_mc(packed, mcnt as u32)?
    }
    io.write(RCTL, rctl);
    let _ = mac;
    Ok(())
}
// upstream: if_igc.c igc_if_timer()
pub fn igc_if_timer<I: IgcMainIo>(io: &mut I, queue_id: u16) {
    if queue_id == 0 {
        io.admin_status_deferred()
    }
}
// upstream: if_igc.c igc_if_vlan_register()
pub fn igc_if_vlan_register<I: IgcMainIo>(
    io: &mut I,
    shadow: &mut [u32],
    vtag: u16,
) -> Result<(), MainError> {
    let index = usize::from((vtag >> 5) & 0x7f);
    let mask = 1u32 << (vtag & 0x1f);
    let word = shadow.get_mut(index).ok_or(MainError::Bounds)?;
    if *word & mask != 0 {
        return Ok(());
    }
    *word |= mask;
    io.write_vfta(index as u32, *word);
    Ok(())
}
// upstream: if_igc.c igc_if_vlan_unregister()
pub fn igc_if_vlan_unregister<I: IgcMainIo>(
    io: &mut I,
    shadow: &mut [u32],
    vtag: u16,
) -> Result<(), MainError> {
    let index = usize::from((vtag >> 5) & 0x7f);
    let mask = 1u32 << (vtag & 0x1f);
    let word = shadow.get_mut(index).ok_or(MainError::Bounds)?;
    if *word & mask == 0 {
        return Ok(());
    }
    *word &= !mask;
    io.write_vfta(index as u32, *word);
    Ok(())
}
// upstream: if_igc.c igc_if_vlan_filter_capable()
pub fn igc_if_vlan_filter_capable(
    capenable: u32,
    vlan_filter_bit: u32,
    disable_crc_stripping: bool,
) -> bool {
    capenable & vlan_filter_bit != 0 && !disable_crc_stripping
}
// upstream: if_igc.c igc_if_vlan_filter_used()
pub fn igc_if_vlan_filter_used(capable: bool, shadow: &[u32]) -> bool {
    capable
        && shadow[..shadow.len().min(VFTA_SIZE)]
            .iter()
            .any(|v| *v != 0)
}
// upstream: if_igc.c igc_if_vlan_filter_enable()
pub fn igc_if_vlan_filter_enable<I: IgcMainIo>(io: &mut I) {
    let mut reg = io.read(RCTL);
    reg &= !RCTL_CFIEN;
    reg |= RCTL_VFE;
    io.write(RCTL, reg)
}
// upstream: if_igc.c igc_if_vlan_filter_disable()
pub fn igc_if_vlan_filter_disable<I: IgcMainIo>(io: &mut I) {
    let mut reg = io.read(RCTL);
    reg &= !(RCTL_VFE | RCTL_CFIEN);
    io.write(RCTL, reg)
}
// upstream: if_igc.c igc_setup_vlan_hw_support()
pub fn igc_setup_vlan_hw_support<I: IgcMainIo>(
    io: &mut I,
    capenable: u32,
    vlan_tag_bit: u32,
    filter_capable: bool,
    shadow: &mut [u32],
) {
    let mut ctrl = io.read(CTRL);
    if capenable & vlan_tag_bit != 0 {
        ctrl |= CTRL_VME
    } else {
        ctrl &= !CTRL_VME
    }
    io.write(CTRL, ctrl);
    if !filter_capable {
        igc_if_vlan_filter_disable(io);
        return;
    }
    if !shadow.is_empty() {
        shadow[0] |= 1
    }
    for (index, value) in shadow.iter().take(VFTA_SIZE).enumerate() {
        io.write_vfta(index as u32, *value)
    }
    igc_if_vlan_filter_enable(io)
}

// upstream: if_igc.c igc_if_init()
pub fn igc_if_init<I: IgcIfInitIo>(
    io: &mut I,
    mac: &mut MacState,
    user_mac: [u8; 6],
    tx_queues: &mut [super::txrx::TxRingState],
) {
    if io.enable_pci_busmaster().is_err() {
        io.log_busmaster_failure();
        io.init_failed();
        return;
    }
    if io.suspend_link_powered_down() {
        io.power_up_wakeup_link()
    }
    mac.address = user_mac;
    io.set_mac_address(user_mac);
    if io.reset_adapter().is_err() {
        io.init_failed();
        return;
    }
    io.update_admin_status();
    for tx in tx_queues {
        tx.rs_cidx = tx.rs_pidx
    }
}

// upstream: if_igc.c igc_if_stop()
pub fn igc_if_stop<I: IgcLifecycleIo>(io: &mut I) -> Result<(), MainError> {
    io.restore_led_for_stop();
    io.prepare_fatal_error_reset();
    if io.stop_reset_hw().is_err() {
        io.log_reset_failure();
        return Err(MainError::Io);
    }
    io.finish_stop_fatal_error_reset();
    io.write(WUC, 0);
    Ok(())
}

// upstream: if_igc.c igc_if_suspend()
pub fn igc_if_suspend<I: IgcLifecycleIo>(io: &mut I) -> Result<(), MainError> {
    let result = io.enable_wakeup();
    io.release_hw_control();
    result
}

// upstream: if_igc.c igc_if_shutdown()
pub fn igc_if_shutdown<I: IgcLifecycleIo>(io: &mut I) {
    if io.enable_wakeup().is_err() {
        io.log_wakeup_failure()
    }
    io.release_hw_control()
}

// upstream: if_igc.c igc_if_resume()
pub fn igc_if_resume<I: IgcLifecycleIo>(io: &mut I) {
    io.disable_broken_l1_2();
    let wus = io.read(WUS);
    let wus_ext = io.read(WUS_EXT);
    if wus != 0 || wus_ext != 0 {
        io.log_wakeup_status(wus, wus_ext)
    }
    io.write(WUFC, 0);
    io.write(WUFC_EXT, 0);
    io.write(WUC, 0);
    io.write(WUS, u32::MAX);
    io.write(WUS_EXT, u32::MAX);
    io.clear_pme()
}

// upstream: if_igc.c igc_if_mtu_set()
pub fn igc_if_mtu_set(mtu: u32) -> Result<u32, MainError> {
    if mtu > MAX_JUMBO_MTU - ETHER_HDR_LEN - ETHER_CRC_LEN {
        return Err(MainError::Bounds);
    }
    Ok(mtu + ETHER_HDR_LEN + ETHER_CRC_LEN)
}

// upstream: if_igc.c igc_if_media_status()
pub fn igc_if_media_status<I: IgcMainIo>(
    io: &mut I,
    link_active: bool,
    speed: u16,
    duplex: u16,
) -> MediaStatus {
    io.admin_status_deferred();
    MediaStatus {
        valid: true,
        active: link_active,
        speed_mbps: if link_active { speed } else { 0 },
        full_duplex: link_active && duplex == 2,
    }
}

// upstream: if_igc.c igc_if_media_change()
pub fn igc_if_media_change(
    ethernet: bool,
    subtype: MediaSubtype,
    full_duplex: bool,
    phy: &mut super::phy::PhyState,
    mac_autoneg: &mut bool,
) -> Result<(), MainError> {
    if !ethernet {
        return Err(MainError::Bounds);
    }
    *mac_autoneg = true;
    phy.autoneg_advertised = match subtype {
        MediaSubtype::Auto => AUTONEG_ADV_DEFAULT,
        MediaSubtype::Speed2500 => ADVERTISE_2500_FULL,
        MediaSubtype::Speed1000 => ADVERTISE_1000_FULL,
        MediaSubtype::Speed100 => {
            if full_duplex {
                ADVERTISE_100_FULL
            } else {
                ADVERTISE_100_HALF
            }
        }
        MediaSubtype::Speed10 => {
            if full_duplex {
                ADVERTISE_10_FULL
            } else {
                ADVERTISE_10_HALF
            }
        }
        MediaSubtype::Other => phy.autoneg_advertised,
    };
    Ok(())
}

// upstream: if_igc.c igc_if_led_func()
pub fn igc_if_led_func<I: IgcMainIo>(io: &mut I, led: &mut LedState, on: bool) {
    if on {
        if !led.active {
            led.default = io.read(LEDCTL);
            led.active = true
        }
        let mut value = led.default;
        value &= !(LED1_MODE_MASK | LED1_BLINK);
        value |= LED_MODE_ON << LED1_MODE_SHIFT;
        io.write(LEDCTL, value)
    } else {
        igc_led_restore(io, led)
    }
}

// upstream: if_igc.c igc_led_restore()
pub fn igc_led_restore<I: IgcMainIo>(io: &mut I, led: &mut LedState) {
    if !led.active {
        return;
    }
    io.write(LEDCTL, led.default);
    led.active = false
}

// upstream: if_igc.c igc_configure_wakeup()
pub fn igc_configure_wakeup<I: IgcWakeupIo>(io: &mut I) -> WakeCapabilities {
    let wol = io.pme_d3_hot_supported();
    let caps = WakeCapabilities {
        wol,
        enabled: WakeFilters {
            magic: wol,
            unicast: false,
            multicast: false,
        },
    };
    io.set_wake_capabilities(caps);
    caps
}

// upstream: if_igc.c igc_enable_wakeup()
pub fn igc_enable_wakeup<I: IgcWakeupIo>(
    io: &mut I,
    mac_state: &mut MacState,
) -> Result<(), MainError> {
    if !io.pme_d3_hot_supported() {
        return Ok(());
    }
    let requested = io.enabled_wake_filters();
    let manage = io.management_passthrough();
    let mut wufc = 0;
    if requested.magic {
        wufc |= WUFC_MAG
    }
    if requested.unicast {
        wufc |= WUFC_EX
    }
    if requested.multicast {
        wufc |= WUFC_MC;
        let addresses = io.multicast_addresses();
        let count = (addresses.len() / 6).min(MAX_MULTICAST);
        if count < MAX_MULTICAST {
            let _ = io.update_multicast(&addresses[..count * 6], count as u32);
        }
    }
    io.write(WUFC, 0);
    io.write(WUFC_EXT, 0);
    io.write(WUC, 0);
    io.write(WUS_WAKE, u32::MAX);
    io.write(WUS_EXT_WAKE, u32::MAX);
    let mut error = None;
    if wufc == 0 {
        if manage {
            if io.suspend_link_powered_down() {
                igc_power_up_wakeup_link(io)
            }
            io.enable_pme()
        } else {
            io.power_down_phy();
            io.set_suspend_link_powered_down(true);
            io.clear_pme()
        }
    } else {
        let address = io.current_mac_address();
        io.set_mac_address(address);
        mac_state.address = address;
        if io.rar_set(address, 0).is_err() {
            io.log_wakeup_error();
            error = Some(MainError::Io)
        }
        if error.is_none() {
            let mut rctl = io.read(RCTL);
            rctl &= !(RCTL_UPE | RCTL_MPE | (3 << RCTL_MO_SHIFT));
            rctl |= RCTL_EN | RCTL_BAM | (u32::from(mac_state.mc_filter_type) << RCTL_MO_SHIFT);
            if wufc & WUFC_MC != 0 {
                rctl |= RCTL_MPE
            }
            io.write(RCTL, rctl);
            let ctrl = io.read(CTRL) | CTRL_ADVD3WUC;
            io.write(CTRL, ctrl);
            igc_power_up_wakeup_link(io);
            io.write(WUC, WUC_PME_EN);
            io.write(WUFC, wufc)
        }
        if error.is_none() {
            io.enable_pme()
        } else {
            io.write(WUFC, 0);
            io.write(WUFC_EXT, 0);
            io.write(WUC, 0);
            io.clear_pme()
        }
    }
    if io.disable_pcie_master().is_err() {
        io.log_pcie_disable_error()
    }
    if io.disable_busmaster().is_err() {
        io.log_busmaster_disable_error()
    }
    error.map_or(Ok(()), Err)
}

// upstream: if_igc.c igc_power_up_wakeup_link()
pub fn igc_power_up_wakeup_link<I: IgcWakeupIo>(io: &mut I) {
    io.power_up_phy();
    io.set_suspend_link_powered_down(false)
}

// upstream: if_igc.c igc_fw_version()
pub fn igc_fw_version<I: super::nvm::IgcNvmIo>(io: &mut I) -> super::nvm::FirmwareVersion {
    super::nvm::igc_get_fw_version(io)
}

// upstream: if_igc.c igc_sbuf_fw_version()
pub fn igc_sbuf_fw_version(version: &super::nvm::FirmwareVersion) -> String {
    let mut out = String::new();
    let mut push = |s: String| {
        if !out.is_empty() {
            out.push(' ')
        }
        out.push_str(&s)
    };
    if version.eep_major != 0 || version.eep_minor != 0 || version.eep_build != 0 {
        push(format!(
            "EEPROM V{}.{}-{}",
            version.eep_major, version.eep_minor, version.eep_build
        ))
    }
    if version.invm_major != 0 || version.invm_minor != 0 || version.invm_img_type != 0 {
        push(format!(
            "NVM V{}.{} imgtype{}",
            version.invm_major, version.invm_minor, version.invm_img_type
        ))
    }
    if version.or_valid {
        push(format!(
            "Option ROM V{}-b{}-p{}",
            version.or_major, version.or_build, version.or_patch
        ))
    }
    if version.etrack_id != 0 {
        push(format!("eTrack {:#010x}", version.etrack_id))
    }
    out
}

// upstream: if_igc.c igc_print_fw_version()
pub fn igc_print_fw_version(version: &super::nvm::FirmwareVersion) -> Option<String> {
    let text = igc_sbuf_fw_version(version);
    if text.is_empty() { None } else { Some(text) }
}

// upstream: if_igc.c igc_if_attach_pre()
pub fn igc_if_attach_pre<I: IgcAttachIo>(
    io: &mut I,
    tx_descriptors: usize,
    rx_descriptors: usize,
    eee_disable: bool,
) -> Result<AttachPlan, MainError> {
    io.sysctl_register();
    io.identify_hardware()?;
    io.disable_broken_l1_2();
    let mut plan = AttachPlan {
        max_queues: igc_set_num_queues(),
        tx_descriptor_count: tx_descriptors,
        rx_descriptor_count: rx_descriptors,
        descriptor_bytes: 16,
        tx_queue_bytes: ((tx_descriptors * 16 + 127) / 128) * 128,
        rx_queue_bytes: ((rx_descriptors * 16 + 127) / 128) * 128,
        max_scatter: 40,
        max_tso_size: 65_535,
        max_tso_segment_size: 4096,
        msix_bar: io.msix_bar(),
        max_frame_size: 1500 + ETHER_HDR_LEN + ETHER_CRC_LEN,
        flow_mode: super::mac::FlowMode::Full,
        mac_autoneg: true,
        autoneg_advertised: AUTONEG_ADV_DEFAULT,
        autoneg_wait: false,
        mtu: 1500,
        eee_disable,
    };
    if io.read_config(plan.msix_bar, 4) == 0 {
        plan.msix_bar = plan.msix_bar.wrapping_add(4)
    }
    io.configure_driver_context(&plan);
    io.allocate_pci_resources()?;
    if io.shared_code_init().is_err() {
        io.free_pci_resources();
        io.free_multicast_buffer();
        return Err(MainError::Io);
    }
    io.setup_msix();
    io.get_bus_info();
    if !io.allocate_multicast_buffer() {
        io.release_hw_control();
        io.free_pci_resources();
        io.free_multicast_buffer();
        return Err(MainError::Io);
    }
    let _ = io.check_reset_block();
    if io.reset_hardware().is_err() {
        io.release_hw_control();
        io.free_pci_resources();
        io.free_multicast_buffer();
        return Err(MainError::Io);
    }
    if io.validate_nvm_checksum().is_err() && io.validate_nvm_checksum().is_err() {
        io.release_hw_control();
        io.free_pci_resources();
        io.free_multicast_buffer();
        return Err(MainError::Io);
    }
    let mac = match io.read_mac_address() {
        Ok(v) => v,
        Err(e) => {
            io.release_hw_control();
            io.free_pci_resources();
            io.free_multicast_buffer();
            return Err(e);
        }
    };
    if !igc_is_valid_ether_addr(&mac) {
        io.release_hw_control();
        io.free_pci_resources();
        io.free_multicast_buffer();
        return Err(MainError::Io);
    }
    io.read_firmware_version();
    io.configure_wakeup();
    io.set_mac_address(mac);
    Ok(plan)
}

// upstream: if_igc.c igc_if_attach_post()
pub fn igc_if_attach_post<I: IgcAttachIo>(io: &mut I) -> Result<(), MainError> {
    io.setup_interface()?;
    io.reset()?;
    io.update_stats_counters();
    io.mark_link_status_stale();
    io.update_admin_status();
    io.add_hw_stats();
    Ok(())
}

// upstream: if_igc.c igc_if_detach()
pub fn igc_if_detach<I: IgcAttachIo>(io: &mut I) {
    io.reset_phy();
    io.release_hw_control();
    io.free_pci_resources()
}

// upstream: if_igc.c igc_if_update_admin_status()
pub fn igc_if_update_admin_status<I: IgcAdminIo>(
    io: &mut I,
    link_active: &mut bool,
    link_speed: &mut u16,
    link_duplex: &mut u16,
) {
    if io.fatal_error_admin() {
        return;
    }
    let mut link_check = false;
    if io.is_copper() {
        if io.get_link_status() {
            io.check_for_link();
            link_check = !io.get_link_status()
        } else {
            link_check = true
        }
    } else if io.is_unknown_media() {
        io.check_for_link();
        link_check = !io.get_link_status()
    }
    if link_check && !*link_active {
        let (speed, duplex) = io.get_speed_duplex();
        *link_speed = speed;
        *link_duplex = duplex;
        *link_active = true;
        io.set_link_state(true, speed)
    } else if !link_check && *link_active {
        *link_speed = 0;
        *link_duplex = 0;
        *link_active = false;
        io.set_link_fields(false, 0, 0);
        io.set_link_state(false, 0)
    }
    io.apply_i225_ipg_workaround();
    io.update_stats_counters();
}

// upstream: if_igc.c igc_identify_hardware()
pub fn igc_identify_hardware<I: IgcPciIo>(
    io: &mut I,
    hw: &mut IgcHardware,
) -> Result<(), MainError> {
    hw.pci_command = io.read_config(PCI_COMMAND, 2) as u16;
    hw.vendor_id = io.read_config(PCI_VENDOR, 2) as u16;
    hw.device_id = io.read_config(PCI_DEVICE, 2) as u16;
    hw.revision_id = io.read_config(PCI_REVISION, 1) as u8;
    hw.subsystem_vendor_id = io.read_config(PCI_SUBVENDOR, 2) as u16;
    hw.subsystem_device_id = io.read_config(PCI_SUBDEVICE, 2) as u16;
    igc_set_mac_type(hw).map(|_| ()).map_err(|_| MainError::Io)
}
// upstream: if_igc.c igc_disable_broken_l1_2()
pub fn igc_disable_broken_l1_2<I: IgcPciIo>(io: &mut I, is_i225: bool, is_i226: bool) {
    let mask = if is_i225 {
        io.l1ss_aspm_l12_mask() | io.l1ss_pcipm_l12_mask()
    } else if is_i226 {
        io.l1ss_aspm_l12_mask()
    } else {
        return;
    };
    let Some(cap) = io.find_l1ss_capability() else {
        return;
    };
    let offset = cap + L1SS_CONTROL1;
    let ctl1 = io.read_config(offset, 4) & !mask;
    io.write_config(offset, 4, ctl1)
}
// upstream: if_igc.c igc_enable_pci_busmaster()
pub fn igc_enable_pci_busmaster<I: IgcPciIo>(io: &mut I) -> Result<(), MainError> {
    let command = io.read_config(PCI_COMMAND, 2);
    if command == u16::MAX as u32 {
        return Err(MainError::Io);
    }
    if command & PCI_BUSMASTER_ENABLE != 0 {
        return Ok(());
    }
    let enabled = io.enable_busmaster();
    let command = io.read_config(PCI_COMMAND, 2);
    if command == u16::MAX as u32 {
        return Err(MainError::Io);
    }
    if command & PCI_BUSMASTER_ENABLE == 0 {
        return enabled.and(Err(MainError::Io));
    }
    Ok(())
}
// upstream: if_igc.c igc_get_hw_control()
pub fn igc_get_hw_control<I: IgcMainIo>(io: &mut I, is_vf: bool) {
    if is_vf {
        return;
    }
    let ctrl = io.read(CTRL_EXT);
    io.write(CTRL_EXT, ctrl | CTRL_EXT_DRV_LOAD)
}
// upstream: if_igc.c igc_release_hw_control()
pub fn igc_release_hw_control<I: IgcMainIo>(io: &mut I) {
    let ctrl = io.read(CTRL_EXT);
    io.write(CTRL_EXT, ctrl & !CTRL_EXT_DRV_LOAD)
}
// upstream: if_igc.c igc_if_get_counter()
pub fn igc_if_get_counter<I: IfCounterIo>(io: &mut I, stats: &IfStats, counter: IfCounter) -> u64 {
    match counter {
        IfCounter::Collisions => stats.colc,
        IfCounter::InputErrors => {
            stats.dropped_pkts
                + stats.rxerrc
                + stats.crcerrs
                + stats.algnerrc
                + stats.ruc
                + stats.rfc
                + stats.roc
                + stats.mpc
        }
        IfCounter::OutputErrors => io.default_counter(counter) + stats.ecol + stats.latecol,
        other => io.default_counter(other),
    }
}

// upstream: if_igc.c igc_update_ecc_stats()
pub fn igc_update_ecc_stats<I: IgcMainIo>(io: &mut I, stats: &mut IfStats) {
    let pbecc = io.read(PBECCSTS);
    if pbecc & PBECCSTS_CORR_ERR != 0 {
        stats.corrected_error_dma_count += 1;
        io.write(PBECCSTS, pbecc & (PBECCSTS_ECC_ENABLE | PBECCSTS_CORR_ERR))
    }
    let pcie = io.read(PCIEECCSTS) & PCIEECCSTS_CORR_MASK;
    if pcie & PCIEECCSTS_TX_WR_DATA != 0 {
        stats.corrected_error_pcie_tx_data_count += 1
    }
    if pcie & PCIEECCSTS_RETRY_BUF != 0 {
        stats.corrected_error_pcie_retry_count += 1
    }
    if pcie != 0 {
        io.write(PCIEECCSTS, pcie)
    }
}

// upstream: if_igc.c igc_update_stats_counters()
pub fn igc_update_stats_counters<I: IgcMainIo>(io: &mut I, stats: &mut IfStats) {
    let prev_xoffrxc = stats.xoffrxc;
    macro_rules! add {
        ($field:ident, $reg:ident) => {
            stats.$field = stats.$field.wrapping_add(u64::from(io.read($reg)));
        };
    }
    add!(crcerrs, CRCERRS);
    add!(rxerrc, RXERRC);
    add!(mpc, MPC);
    add!(scc, SCC);
    add!(ecol, ECOL);
    add!(mcc, MCC);
    add!(latecol, LATECOL);
    add!(colc, COLC);
    add!(rerc, RERC);
    add!(dc, DC);
    add!(rlec, RLEC);
    add!(xonrxc, XONRXC);
    add!(xontxc, XONTXC);
    add!(xoffrxc, XOFFRXC);
    if stats.xoffrxc != prev_xoffrxc {
        stats.xoff_pause_observed = true
    }
    add!(xofftxc, XOFFTXC);
    add!(fcruc, FCRUC);
    add!(prc64, PRC64);
    add!(prc127, PRC127);
    add!(prc255, PRC255);
    add!(prc511, PRC511);
    add!(prc1023, PRC1023);
    add!(prc1522, PRC1522);
    add!(tlpic, TLPIC);
    add!(rlpic, RLPIC);
    add!(gprc, GPRC);
    add!(bprc, BPRC);
    add!(mprc, MPRC);
    add!(gptc, GPTC);
    let gorcl = io.read(GORCL);
    let gorch = io.read(GORCH);
    stats.gorc = stats
        .gorc
        .wrapping_add(u64::from(gorcl) + (u64::from(gorch) << 32));
    let gotcl = io.read(GOTCL);
    let gotch = io.read(GOTCH);
    stats.gotc = stats
        .gotc
        .wrapping_add(u64::from(gotcl) + (u64::from(gotch) << 32));
    add!(rnbc, RNBC);
    add!(ruc, RUC);
    add!(rfc, RFC);
    add!(roc, ROC);
    add!(rjc, RJC);
    add!(mgprc, MGTPRC);
    add!(mgpdc, MGTPDC);
    add!(mgptc, MGTPTC);
    let torl = io.read(TORL);
    let torh = io.read(TORH);
    stats.tor = stats
        .tor
        .wrapping_add(u64::from(torl) + (u64::from(torh) << 32));
    let totl = io.read(TOTL);
    let toth = io.read(TOTH);
    stats.tot = stats
        .tot
        .wrapping_add(u64::from(totl) + (u64::from(toth) << 32));
    add!(tpr, TPR);
    add!(tpt, TPT);
    add!(ptc64, PTC64);
    add!(ptc127, PTC127);
    add!(ptc255, PTC255);
    add!(ptc511, PTC511);
    add!(ptc1023, PTC1023);
    add!(ptc1522, PTC1522);
    add!(mptc, MPTC);
    add!(bptc, BPTC);
    add!(iac, IAC);
    add!(rxdmtc, RXDMTC);
    add!(algnerrc, ALGNERRC);
    add!(tncrs, TNCRS);
    add!(htdpmc, HTDPMC);
    add!(tsctc, TSCTC);
    igc_update_ecc_stats(io, stats)
}
// upstream: if_igc.c igc_set_num_queues()
pub const fn igc_set_num_queues() -> usize {
    4
}
// upstream: if_igc.c igc_setup_msix()
pub const fn igc_setup_msix() -> Result<(), MainError> {
    Ok(())
}
// upstream: if_igc.c igc_is_valid_ether_addr()
pub fn igc_is_valid_ether_addr(address: &[u8; 6]) -> bool {
    address[0] & 1 == 0 && address.iter().any(|b| *b != 0)
}

#[cfg(test)]
mod tests {

    use alloc::{vec, vec::Vec};

    use super::*;
    #[derive(Default)]
    struct Fake {
        regs: Vec<(u32, u32)>,
        writes: Vec<(u32, u32)>,
        mta: Vec<(Vec<u8>, u32)>,
        vfta: Vec<(u32, u32)>,
        admin: usize,
        events: Vec<&'static str>,
        suspended: bool,
        fatal_admin: bool,
        link_active: bool,
        link_speed: u16,
        link_duplex: u16,
        copper: bool,
        unknown_media: bool,
        get_link_status: bool,
        config: Vec<(u16, u8, u32)>,
        pme_supported: bool,
        wake_filters: WakeFilters,
        wake_caps: Option<WakeCapabilities>,
        manage_passthrough: bool,
        mac_address: [u8; 6],
        rar_failure: bool,
        pme_enabled: bool,
    }
    #[derive(Default)]
    struct AttachFake {
        events: Vec<&'static str>,
        plan: Option<AttachPlan>,
        checksum_calls: u8,
        mac: [u8; 6],
    }
    impl IgcAttachIo for AttachFake {
        fn sysctl_register(&mut self) {
            self.events.push("sysctl")
        }
        fn identify_hardware(&mut self) -> Result<(), MainError> {
            self.events.push("identify");
            Ok(())
        }
        fn disable_broken_l1_2(&mut self) {
            self.events.push("l1.2")
        }
        fn configure_driver_context(&mut self, p: &AttachPlan) {
            self.events.push("context");
            self.plan = Some(p.clone())
        }
        fn msix_bar(&mut self) -> u16 {
            0x1c
        }
        fn read_config(&mut self, _: u16, _: u8) -> u32 {
            0
        }
        fn allocate_pci_resources(&mut self) -> Result<(), MainError> {
            self.events.push("pci");
            Ok(())
        }
        fn shared_code_init(&mut self) -> Result<(), MainError> {
            self.events.push("shared");
            Ok(())
        }
        fn setup_msix(&mut self) {
            self.events.push("msix")
        }
        fn get_bus_info(&mut self) {
            self.events.push("bus")
        }
        fn allocate_multicast_buffer(&mut self) -> bool {
            self.events.push("mta");
            true
        }
        fn check_reset_block(&mut self) -> bool {
            self.events.push("reset-block");
            false
        }
        fn reset_hardware(&mut self) -> Result<(), MainError> {
            self.events.push("reset");
            Ok(())
        }
        fn validate_nvm_checksum(&mut self) -> Result<(), MainError> {
            self.checksum_calls += 1;
            self.events.push("checksum");
            if self.checksum_calls == 1 {
                Err(MainError::Io)
            } else {
                Ok(())
            }
        }
        fn read_mac_address(&mut self) -> Result<[u8; 6], MainError> {
            self.events.push("mac-read");
            Ok([2, 1, 2, 3, 4, 5])
        }
        fn read_firmware_version(&mut self) {
            self.events.push("firmware")
        }
        fn configure_wakeup(&mut self) {
            self.events.push("wakeup")
        }
        fn set_mac_address(&mut self, a: [u8; 6]) {
            self.mac = a;
            self.events.push("set-mac")
        }
        fn release_hw_control(&mut self) {
            self.events.push("release")
        }
        fn free_multicast_buffer(&mut self) {
            self.events.push("free-mta")
        }
        fn free_pci_resources(&mut self) {
            self.events.push("free-pci")
        }
        fn setup_interface(&mut self) -> Result<(), MainError> {
            self.events.push("interface");
            Ok(())
        }
        fn reset(&mut self) -> Result<(), MainError> {
            self.events.push("post-reset");
            Ok(())
        }
        fn update_stats_counters(&mut self) {
            self.events.push("stats")
        }
        fn mark_link_status_stale(&mut self) {
            self.events.push("stale")
        }
        fn update_admin_status(&mut self) {
            self.events.push("admin")
        }
        fn add_hw_stats(&mut self) {
            self.events.push("hwstats")
        }
        fn reset_phy(&mut self) {
            self.events.push("phy-reset")
        }
    }
    impl Fake {
        fn get(&self, r: u32) -> u32 {
            self.regs.iter().rev().find(|x| x.0 == r).map_or(0, |x| x.1)
        }
        fn set(&mut self, r: u32, v: u32) {
            if let Some(x) = self.regs.iter_mut().find(|x| x.0 == r) {
                x.1 = v
            } else {
                self.regs.push((r, v))
            }
        }
    }
    impl IgcMainIo for Fake {
        fn read(&mut self, r: u32) -> u32 {
            self.get(r)
        }
        fn write(&mut self, r: u32, v: u32) {
            self.writes.push((r, v));
            self.set(r, v)
        }
        fn admin_status_deferred(&mut self) {
            self.admin += 1
        }
        fn update_mc(&mut self, a: &[u8], c: u32) -> Result<(), MainError> {
            self.mta.push((a.to_vec(), c));
            Ok(())
        }
        fn write_vfta(&mut self, i: u32, v: u32) {
            self.vfta.push((i, v))
        }
    }
    impl IgcRssIo for Fake {
        fn rss_bucket(&mut self, b: usize, n: usize) -> usize {
            b % n
        }
        fn rss_key(&mut self) -> [u32; 10] {
            [0x1234; 10]
        }
        fn rss_hash_config(&mut self) -> u32 {
            0xff
        }
    }
    impl IgcResetIo for Fake {
        fn restore_led(&mut self) {}
        fn get_hw_control(&mut self) {}
        fn reset_hw(&mut self) -> Result<(), MainError> {
            Ok(())
        }
        fn init_hw(&mut self) -> Result<(), MainError> {
            Ok(())
        }
        fn finish_fatal_error_reset(&mut self) {}
        fn init_dmac(&mut self, _: u32, _: u32) {}
        fn get_phy_info(&mut self) {}
        fn check_for_link(&mut self) {}
        fn log_reset_error(&mut self, _: &'static str) {}
    }
    impl IgcIfInitIo for Fake {
        fn enable_pci_busmaster(&mut self) -> Result<(), MainError> {
            self.events.push("busmaster");
            Ok(())
        }
        fn suspend_link_powered_down(&self) -> bool {
            self.suspended
        }
        fn power_up_wakeup_link(&mut self) {
            self.events.push("power-up")
        }
        fn reset_adapter(&mut self) -> Result<(), MainError> {
            self.events.push("reset");
            Ok(())
        }
        fn update_admin_status(&mut self) {
            self.events.push("admin")
        }
        fn init_failed(&mut self) {
            self.events.push("failed")
        }
        fn log_busmaster_failure(&mut self) {
            self.events.push("busmaster-failed")
        }
        fn set_mac_address(&mut self, _: [u8; 6]) {
            self.events.push("mac")
        }
    }
    impl IgcLifecycleIo for Fake {
        fn restore_led_for_stop(&mut self) {
            self.events.push("led")
        }
        fn prepare_fatal_error_reset(&mut self) {
            self.events.push("prepare")
        }
        fn stop_reset_hw(&mut self) -> Result<(), MainError> {
            self.events.push("stop-reset");
            Ok(())
        }
        fn finish_stop_fatal_error_reset(&mut self) {
            self.events.push("finish")
        }
        fn enable_wakeup(&mut self) -> Result<(), MainError> {
            self.events.push("wakeup");
            Ok(())
        }
        fn release_hw_control(&mut self) {
            self.events.push("release")
        }
        fn disable_broken_l1_2(&mut self) {
            self.events.push("l1.2")
        }
        fn log_wakeup_status(&mut self, _: u32, _: u32) {
            self.events.push("wus")
        }
        fn clear_pme(&mut self) {
            self.events.push("pme")
        }
        fn log_reset_failure(&mut self) {
            self.events.push("reset-failed")
        }
        fn log_wakeup_failure(&mut self) {
            self.events.push("wakeup-failed")
        }
    }
    impl IgcAdminIo for Fake {
        fn fatal_error_admin(&mut self) -> bool {
            self.fatal_admin
        }
        fn is_copper(&self) -> bool {
            self.copper
        }
        fn is_unknown_media(&self) -> bool {
            self.unknown_media
        }
        fn get_link_status(&self) -> bool {
            self.get_link_status
        }
        fn check_for_link(&mut self) {
            self.get_link_status = false
        }
        fn get_speed_duplex(&mut self) -> (u16, u16) {
            (2500, 2)
        }
        fn set_link_state(&mut self, up: bool, speed: u16) {
            self.events.push(if up { "link-up" } else { "link-down" });
            self.link_active = up;
            self.link_speed = speed
        }
        fn set_link_fields(&mut self, active: bool, speed: u16, duplex: u16) {
            self.link_active = active;
            self.link_speed = speed;
            self.link_duplex = duplex
        }
        fn apply_i225_ipg_workaround(&mut self) {
            self.events.push("ipg")
        }
        fn update_stats_counters(&mut self) {
            self.events.push("stats")
        }
    }
    impl IgcFatalIo for Fake {
        fn delay_ms(&mut self, _: u32) {}
        fn disable_pcie_master(&mut self) -> Result<(), MainError> {
            self.events.push("master-off");
            Ok(())
        }
        fn log_parity_reset_timeout(&mut self) {
            self.events.push("parity-timeout")
        }
        fn log_master_disable_failure(&mut self) {
            self.events.push("master-fail")
        }
    }
    impl IgcInterruptIo for Fake {
        fn disable_interrupts(&mut self) {
            self.events.push("disable-interrupts")
        }
    }
    impl IgcPciIo for Fake {
        fn read_config(&mut self, o: u16, w: u8) -> u32 {
            self.config
                .iter()
                .rev()
                .find(|v| v.0 == o && v.1 == w)
                .map_or(0, |v| v.2)
        }
        fn write_config(&mut self, o: u16, w: u8, val: u32) {
            if let Some(v) = self.config.iter_mut().find(|v| v.0 == o && v.1 == w) {
                v.2 = val
            } else {
                self.config.push((o, w, val))
            }
        }
        fn enable_busmaster(&mut self) -> Result<(), MainError> {
            let cmd = self.read_config(PCI_COMMAND, 2) | PCI_BUSMASTER_ENABLE;
            self.write_config(PCI_COMMAND, 2, cmd);
            Ok(())
        }
        fn find_l1ss_capability(&mut self) -> Option<u16> {
            Some(0x100)
        }
        fn l1ss_aspm_l12_mask(&self) -> u32 {
            2
        }
        fn l1ss_pcipm_l12_mask(&self) -> u32 {
            8
        }
    }
    impl IfCounterIo for Fake {
        fn default_counter(&mut self, _: IfCounter) -> u64 {
            5
        }
    }
    impl IgcWakeupIo for Fake {
        fn pme_d3_hot_supported(&mut self) -> bool {
            self.pme_supported
        }
        fn enabled_wake_filters(&self) -> WakeFilters {
            self.wake_filters
        }
        fn set_wake_capabilities(&mut self, c: WakeCapabilities) {
            self.wake_caps = Some(c)
        }
        fn management_passthrough(&mut self) -> bool {
            self.manage_passthrough
        }
        fn multicast_addresses(&mut self) -> Vec<u8> {
            vec![2, 3, 4, 5, 6, 7]
        }
        fn update_multicast(&mut self, _: &[u8], _: u32) -> Result<(), MainError> {
            self.events.push("multicast");
            Ok(())
        }
        fn current_mac_address(&self) -> [u8; 6] {
            self.mac_address
        }
        fn set_mac_address(&mut self, a: [u8; 6]) {
            self.mac_address = a
        }
        fn rar_set(&mut self, _: [u8; 6], _: u32) -> Result<(), MainError> {
            if self.rar_failure {
                Err(MainError::Io)
            } else {
                Ok(())
            }
        }
        fn power_up_phy(&mut self) {
            self.events.push("power-up-phy")
        }
        fn power_down_phy(&mut self) {
            self.events.push("power-down-phy")
        }
        fn suspend_link_powered_down(&self) -> bool {
            self.suspended
        }
        fn set_suspend_link_powered_down(&mut self, v: bool) {
            self.suspended = v
        }
        fn enable_pme(&mut self) {
            self.pme_enabled = true;
            self.events.push("pme-on")
        }
        fn clear_pme(&mut self) {
            self.pme_enabled = false;
            self.events.push("pme-off")
        }
        fn disable_pcie_master(&mut self) -> Result<(), MainError> {
            self.events.push("disable-master");
            Ok(())
        }
        fn disable_busmaster(&mut self) -> Result<(), MainError> {
            self.events.push("disable-busmaster");
            Ok(())
        }
        fn log_wakeup_error(&mut self) {
            self.events.push("wakeup-error")
        }
        fn log_pcie_disable_error(&mut self) {
            self.events.push("pcie-error")
        }
        fn log_busmaster_disable_error(&mut self) {
            self.events.push("busmaster-error")
        }
    }
    #[test]
    fn aim_snapshot_delta_ring_rate_and_idle_admin_follow_source() {
        let mut c = AimCounters::default();
        c.bytes_last = u32::MAX - 3;
        c.packets_last = u32::MAX;
        c.snapshot.store((2u64 << 32) | 1, Ordering::Release);
        assert_eq!(igc_aim_rx_delta(&mut c), (6, 2));
        let device = AimDevice {
            enabled: 1,
            max_interrupt_rate: 8000,
            link_speed_mbps: 2500,
            max_frame_size: 1518,
            packet_buffer_kb: 20408,
        };
        assert_eq!(igc_ring_itr(&device, 1500, 1, 0, 0), 5249);
        let mut io = Fake::default();
        igc_if_timer(&mut io, 1);
        igc_if_timer(&mut io, 0);
        assert_eq!(io.admin, 1);
        assert!(!igc_if_needs_restart(RestartEvent::VlanChange));
    }
    #[test]
    fn filter_policy_preserves_vlan_shadow_and_multicast_promisc() {
        let mut io = Fake::default();
        io.set(RCTL, 0x1000);
        igc_if_set_promisc(&mut io, IFF_PROMISC, 0, false, false, false).unwrap();
        assert_ne!(io.get(RCTL) & (RCTL_UPE | RCTL_MPE), 0);
        assert_eq!(io.get(RCTL) & 0x1000, 0x1000);
        let mut shadow = [0u32; 128];
        igc_if_vlan_register(&mut io, &mut shadow, 37).unwrap();
        assert_eq!(shadow[1], 1 << 5);
        igc_if_vlan_register(&mut io, &mut shadow, 37).unwrap();
        assert_eq!(io.vfta.len(), 1);
        assert!(igc_if_vlan_filter_used(true, &shadow));
        igc_setup_vlan_hw_support(&mut io, 1, 1, true, &mut shadow);
        assert_eq!(io.vfta.len(), 129);
        assert_eq!(io.vfta[0], (1, 1 << 5));
        assert_eq!(io.vfta[1], (0, shadow[0]));
        assert!(igc_if_vlan_filter_capable(2, 2, false));
        igc_if_vlan_unregister(&mut io, &mut shadow, 37).unwrap();
        assert_eq!(shadow[1], 0);
    }
    #[test]
    fn eeprom_address_and_ipg_rules_match() {
        assert!(!igc_is_valid_ether_addr(&[0; 6]));
        assert!(!igc_is_valid_ether_addr(&[1, 0, 0, 0, 0, 0]));
        assert!(igc_is_valid_ether_addr(&[2, 0, 0, 0, 0, 1]));
        let mut io = Fake::default();
        io.set(TIPG, 0x2222_0008);
        igc_apply_i225_ipg_workaround(&mut io, true, 1, 2500);
        assert_eq!(io.get(TIPG) & TIPG_IPGT_MASK, I225_IPGT_2P5);
        let before = io.get(TIPG);
        igc_apply_i225_ipg_workaround(&mut io, true, 2, 2500);
        assert_eq!(io.get(TIPG), before);
    }

    #[test]
    fn reset_and_tx_rx_units_keep_register_order_and_descriptor_geometry() {
        let mut io = Fake::default();
        let mut mac = MacState::default();
        let config = UnitConfig {
            tx_rings: vec![RingDma {
                bus_address: 0x1122_3344_5566_7788,
                descriptors: 128,
            }],
            rx_rings: vec![RingDma {
                bus_address: 0xaabb_ccdd_1234_5678,
                descriptors: 256,
            }],
            max_frame_size: 1518,
            mtu: 1500,
            rx_buffer_size: 2048,
            vlan_trunk: false,
            disable_crc_stripping: false,
            rx_checksum: true,
            flow_mode: super::super::mac::FlowMode::Full,
            multicast_filter_type: 0,
            low_water: 0,
            high_water: 0,
            send_xon: true,
        };
        assert_eq!(igc_reset(&mut io, &mut mac, &config, 0).unwrap(), PBA_34K);
        assert_eq!(mac.flow.high_water, 32768);
        assert_eq!(mac.flow.low_water, 32752);
        assert_eq!(io.get(VET), 0x8100);
        igc_initialize_transmit_unit(&mut io, &config).unwrap();
        assert_eq!(io.get(TDLEN), 2048);
        assert_eq!(io.get(TDBAL), 0x5566_7788);
        assert_eq!(io.get(TDBAH), 0x1122_3344);
        assert_eq!(
            io.get(TXDCTL),
            TXDCTL_QUEUE_ENABLE | (TX_PTHRESH) | (TX_HTHRESH << 8)
        );
        assert_ne!(io.get(0x00400) & TCTL_EN, 0);
        igc_initialize_receive_unit(&mut io, &config).unwrap();
        assert_eq!(io.get(RDLEN), 4096);
        assert_eq!(io.get(RDBAL), 0x1234_5678);
        assert_eq!(io.get(RDBAH), 0xaabb_ccdd);
        assert_eq!(
            io.get(RXDCTL),
            RXDCTL_QUEUE_ENABLE | (8) | (8 << 8) | (4 << 16)
        );
        assert_ne!(io.get(RCTL) & RCTL_SECRC, 0);
    }

    #[test]
    fn rss_mapping_packs_four_buckets_per_dword_and_four_queue_mode() {
        let mut io = Fake::default();
        igc_initialize_rss_mapping(&mut io, 4);
        assert_eq!(io.get(RETA), 0x0302_0100);
        assert_eq!(io.get(RETA + 4), 0x0302_0100);
        assert_eq!(io.get(RSSRK + 36), 0x1234);
        assert_eq!(io.get(MRQC), MRQC_ENABLE_RSS_4Q | 0x01f7_0000);
    }

    #[test]
    fn lifecycle_init_stop_resume_and_link_transitions_keep_source_order() {
        let mut io = Fake {
            suspended: true,
            copper: true,
            get_link_status: true,
            ..Fake::default()
        };
        let mut mac = MacState::default();
        let mut tx = vec![super::super::txrx::TxRingState::new(8)];
        tx[0].rs_pidx = 3;
        tx[0].rs_cidx = 0;
        igc_if_init(&mut io, &mut mac, [2, 1, 2, 3, 4, 5], &mut tx);
        assert_eq!(
            &io.events[..],
            &["busmaster", "power-up", "mac", "reset", "admin"]
        );
        assert_eq!(tx[0].rs_cidx, 3);
        io.events.clear();
        igc_if_stop(&mut io).unwrap();
        assert_eq!(&io.events[..], &["led", "prepare", "stop-reset", "finish"]);
        assert_eq!(io.get(WUC), 0);
        io.events.clear();
        io.set(WUS, 1);
        io.set(WUS_EXT, 2);
        igc_if_resume(&mut io);
        assert_eq!(&io.events[..], &["l1.2", "wus", "pme"]);
        assert_eq!(io.get(WUS), u32::MAX);
        assert_eq!(io.get(WUFC), 0);
        io.events.clear();
        let (mut active, mut speed, mut duplex) = (false, 0, 0);
        igc_if_update_admin_status(&mut io, &mut active, &mut speed, &mut duplex);
        assert!(active);
        assert_eq!((speed, duplex), (2500, 2));
        assert_eq!(io.events, ["link-up", "ipg", "stats"]);
        assert_eq!(igc_if_mtu_set(1500), Ok(1518));
        assert_eq!(igc_if_mtu_set(9216), Ok(9234));
        assert_eq!(igc_if_mtu_set(9217), Err(MainError::Bounds));
    }

    #[test]
    fn fatal_error_recovery_resets_before_busmaster_and_clears_latched_faults() {
        let mut io = Fake::default();
        io.set(EECD, EECD_AUTO_RD);
        io.set(STATUS, STATUS_RST_DONE);
        io.set(PCIEERRSTS, PCIEERRSTS_FATAL_MASK);
        io.set(LANPERRSTS, LANPERRSTS_RETX_BUF);
        let mut fault = FatalErrorState {
            state: AtomicU32::new(1),
            peind: PEIND_PCIE_PARITY_FATAL,
            pcie_error: 0,
            lan_error: 0,
            mng_error: 4,
            ..FatalErrorState::default()
        };
        igc_prepare_fatal_error_reset(&mut io, &fault);
        assert!(io.get(CTRL) & CTRL_DEV_RST != 0);
        assert!(io.events.contains(&"master-off"));
        igc_finish_fatal_error_reset(&mut io, &mut fault);
        assert_eq!(fault.state.load(Ordering::Acquire), 0);
        assert_eq!(fault.peind, 0);
        assert_eq!(io.get(PEIND), 0);
    }

    #[test]
    fn interrupt_masking_and_legacy_msix_paths_follow_register_flow() {
        let mut io = Fake::default();
        let state = InterruptState {
            msix: true,
            queue_mask: 0x100,
            link_mask: 0x20,
            link_interrupts: 0,
            rx_overruns: 0,
        };
        let fatal = FatalErrorState::default();
        igc_if_intr_enable(&mut io, &state, &fatal);
        assert_eq!(io.get(EIAC), 0x120);
        assert_eq!(io.get(EIAM), 0x120);
        assert_eq!(io.get(EIMS), 0x120);
        assert_eq!(io.get(IMS), IMS_LSC | IMS_FER);
        igc_if_intr_disable(&mut io, true);
        assert_eq!(io.get(EIMC), u32::MAX);
        assert_eq!(io.get(EIAC), 0);
        assert_eq!(io.get(IMC), u32::MAX);
        io.set(ICR, ICR_INT_ASSERTED | ICR_LSC);
        let mut intr = InterruptState {
            msix: true,
            queue_mask: 0,
            link_mask: 0x10,
            link_interrupts: 0,
            rx_overruns: 0,
        };
        let mut mac = MacState::default();
        let mut fatal = FatalErrorState::default();
        let device = AimDevice {
            enabled: 0,
            max_interrupt_rate: 8000,
            link_speed_mbps: 1000,
            max_frame_size: 1518,
            packet_buffer_kb: 34,
        };
        let mut rx = AimRxQueue {
            counters: AimCounters::default(),
            vector: 0,
            eitr_setting: 0,
            interrupts: 0,
        };
        assert_eq!(
            igc_intr(
                &mut io,
                &mut intr,
                &mut mac,
                &mut fatal,
                &device,
                &mut rx,
                &mut []
            ),
            InterruptResult::ScheduleThread
        );
        assert!(mac.get_link_status);
        assert!(io.events.contains(&"disable-interrupts"));
        assert_eq!(
            igc_msix_link(&mut io, &mut intr, &mut mac, &mut fatal),
            InterruptResult::Handled
        );
        assert_eq!(io.get(EIMS), 0x10);
    }

    #[test]
    fn fatal_interrupt_captures_status_before_admin_recovery() {
        let mut io = Fake::default();
        io.set(PEIND, PEIND_MNG_PARITY_FATAL);
        io.set(PCIEERRSTS, PCIEERRSTS_FATAL_MASK);
        io.set(LANPERRSTS, LANPERRSTS_RETX_BUF);
        io.set(MNGPARSTS, MNGPARSTS_FATAL_MASK);
        let mut fatal = FatalErrorState::default();
        igc_handle_fatal_error_intr(&mut io, &mut fatal, ICR_FER);
        assert_eq!(fatal.state.load(Ordering::Acquire), FATAL_DETECTED);
        assert_eq!(
            fatal.peind,
            PEIND_MNG_PARITY_FATAL | PEIND_PCIE_PARITY_FATAL | PEIND_LANPORT_PARITY_FATAL
        );
        assert_eq!(io.admin, 1);
        assert!(igc_handle_fatal_error_admin(&mut fatal));
        assert_eq!(fatal.lan_parity_count, 1);
        assert_eq!(fatal.mng_parity_count, 1);
        assert_eq!(fatal.pcie_parity_count, 1);
    }

    #[test]
    fn msix_queue_routes_pack_ivar_bytes_and_initial_rate() {
        let mut io = Fake::default();
        let mut topology = InterruptTopology {
            rx: vec![
                QueueInterrupt { vector: 1, eims: 1 },
                QueueInterrupt { vector: 2, eims: 2 },
            ],
            tx: vec![
                QueueInterrupt {
                    vector: 3,
                    eims: 0x100,
                },
                QueueInterrupt {
                    vector: 4,
                    eims: 0x200,
                },
            ],
            link_vector: 5,
            queue_mask: 0,
            link_mask: 0,
        };
        igc_configure_queues(&mut io, &mut topology);
        assert_eq!(
            io.get(GPIE),
            GPIE_MSIX_MODE | GPIE_EIAME | GPIE_PBA | GPIE_NSICR
        );
        assert_eq!(io.get(IVAR0), 0x8482_8381);
        assert_eq!(io.get(IVAR0 + 4), 0);
        assert_eq!(io.get(IVAR_MISC), (5 | IVAR_VALID) << 8);
        assert_eq!(topology.queue_mask, 0x300);
        assert_eq!(topology.link_mask, 1 << 5);
        let mut queues = vec![AimRxQueue {
            counters: AimCounters::default(),
            vector: 2,
            eitr_setting: 0,
            interrupts: 0,
        }];
        igc_initialize_interrupt_rate(&mut io, &mut queues, 8000);
        assert_eq!(
            queues[0].eitr_setting,
            (((1_000_000 / 8000) << 2) & EITR_QVECTOR_MASK) | EITR_CNT_IGNR
        );
    }

    #[test]
    fn pci_identification_busmaster_l1ss_and_counter_callbacks_translate() {
        let mut io = Fake::default();
        io.config.extend([
            (PCI_COMMAND, 2, 0),
            (PCI_VENDOR, 2, 0x8086),
            (PCI_DEVICE, 2, 0x15f3),
            (PCI_REVISION, 1, 1),
            (PCI_SUBVENDOR, 2, 0x8086),
            (PCI_SUBDEVICE, 2, 7),
            (0x108, 4, 0xffff),
        ]);
        let mut hw = IgcHardware::new(0, false);
        igc_identify_hardware(&mut io, &mut hw).unwrap();
        assert_eq!(hw.device_id, 0x15f3);
        assert_eq!(hw.mac_type, Some(super::super::api::IgcMacType::I225));
        igc_enable_pci_busmaster(&mut io).unwrap();
        assert_eq!(
            io.read_config(PCI_COMMAND, 2) & PCI_BUSMASTER_ENABLE,
            PCI_BUSMASTER_ENABLE
        );
        igc_disable_broken_l1_2(&mut io, true, false);
        assert_eq!(io.read_config(0x108, 4) & 0xa, 0);
        igc_get_hw_control(&mut io, false);
        assert_ne!(io.get(CTRL_EXT) & CTRL_EXT_DRV_LOAD, 0);
        igc_release_hw_control(&mut io);
        assert_eq!(io.get(CTRL_EXT) & CTRL_EXT_DRV_LOAD, 0);
        let stats = IfStats {
            colc: 2,
            dropped_pkts: 1,
            rxerrc: 2,
            crcerrs: 3,
            algnerrc: 4,
            ruc: 5,
            rfc: 6,
            roc: 7,
            mpc: 8,
            ecol: 9,
            latecol: 10,
            ..IfStats::default()
        };
        assert_eq!(
            igc_if_get_counter(&mut io, &stats, IfCounter::InputErrors),
            36
        );
        assert_eq!(
            igc_if_get_counter(&mut io, &stats, IfCounter::OutputErrors),
            24
        );
        assert_eq!(igc_if_get_counter(&mut io, &stats, IfCounter::Other(0)), 5);
        assert_eq!(igc_set_num_queues(), 4);
        assert!(igc_setup_msix().is_ok());
    }

    #[test]
    fn media_and_led_callbacks_update_state_without_losing_oem_led_config() {
        let mut io = Fake::default();
        io.set(LEDCTL, 0xa5a5_5a5a);
        let mut led = LedState::default();
        igc_if_led_func(&mut io, &mut led, true);
        assert!(led.active);
        assert_eq!(io.get(LEDCTL) & LED1_MODE_MASK, 0);
        assert_eq!(io.get(LEDCTL) & LED1_BLINK, 0);
        igc_led_restore(&mut io, &mut led);
        assert!(!led.active);
        assert_eq!(io.get(LEDCTL), 0xa5a5_5a5a);
        let mut phy = super::super::phy::PhyState::default();
        let mut autoneg = false;
        igc_if_media_change(true, MediaSubtype::Speed100, false, &mut phy, &mut autoneg).unwrap();
        assert!(autoneg);
        assert_eq!(phy.autoneg_advertised, ADVERTISE_100_HALF);
        assert!(
            igc_if_media_change(false, MediaSubtype::Auto, true, &mut phy, &mut autoneg).is_err()
        );
        assert_eq!(
            igc_if_media_status(&mut io, true, 2500, 2),
            MediaStatus {
                valid: true,
                active: true,
                speed_mbps: 2500,
                full_duplex: true
            }
        );
        assert_eq!(
            igc_if_media_status(&mut io, false, 0, 0),
            MediaStatus {
                valid: true,
                active: false,
                speed_mbps: 0,
                full_duplex: false
            }
        );
    }

    #[test]
    fn stat_reads_preserve_64bit_low_before_high_and_ecc_clear_masks() {
        let mut io = Fake::default();
        io.set(XOFFRXC, 1);
        io.set(GORCL, 0x1234);
        io.set(GORCH, 2);
        io.set(GOTCL, 0x5678);
        io.set(GOTCH, 3);
        io.set(PBECCSTS, PBECCSTS_ECC_ENABLE | PBECCSTS_CORR_ERR);
        io.set(PCIEECCSTS, PCIEECCSTS_CORR_MASK);
        let mut stats = IfStats::default();
        igc_update_stats_counters(&mut io, &mut stats);
        assert_eq!(stats.xoffrxc, 1);
        assert!(stats.xoff_pause_observed);
        assert_eq!(stats.gorc, (2u64 << 32) | 0x1234);
        assert_eq!(stats.gotc, (3u64 << 32) | 0x5678);
        assert_eq!(stats.corrected_error_dma_count, 1);
        assert_eq!(stats.corrected_error_pcie_tx_data_count, 1);
        assert_eq!(stats.corrected_error_pcie_retry_count, 1);
        assert!(
            io.writes
                .contains(&(PBECCSTS, PBECCSTS_ECC_ENABLE | PBECCSTS_CORR_ERR))
        );
        assert!(io.writes.contains(&(PCIEECCSTS, PCIEECCSTS_CORR_MASK)));
    }

    #[test]
    fn dmac_register_programming_uses_pba_watermark_and_sku_watchdog() {
        let mut io = Fake::default();
        io.set(STATUS, STATUS_2P5_SKU);
        io.set(PCIEMISC, PCIEMISC_LX_DECISION);
        igc_init_dmac(&mut io, 80, 34, 1518);
        assert_eq!(io.get(DMCTXTH), (TXPBSIZE - 2 * 1518) >> 6);
        assert_eq!(
            io.get(FCRTC) & FCRTC_RTH_COAL_MASK,
            ((64 * 34 - 1518 / 16) << FCRTC_RTH_COAL_SHIFT) & FCRTC_RTH_COAL_MASK
        );
        assert_eq!(
            io.get(DMACR) & DMACR_DMACTHR_MASK,
            32 << DMACR_DMACTHR_SHIFT
        );
        assert_eq!(io.get(DMACR) & 0x3fff, 6);
        assert_eq!(io.get(DMCTLX) & 0xf, 0xa);
        assert_eq!(io.get(PCIEMISC) & PCIEMISC_LX_DECISION, 0);
        igc_init_dmac(&mut io, 0, 34, 1518);
        assert_eq!(io.get(DMACR), !DMACR_DMAC_EN);
    }

    #[test]
    fn wol_capability_filters_and_pme_order_follow_suspend_policy() {
        let mut io = Fake {
            pme_supported: true,
            suspended: true,
            manage_passthrough: true,
            wake_filters: WakeFilters::default(),
            mac_address: [2, 1, 2, 3, 4, 5],
            ..Fake::default()
        };
        let caps = igc_configure_wakeup(&mut io);
        assert!(caps.wol && caps.enabled.magic);
        let mut mac = MacState::default();
        igc_enable_wakeup(&mut io, &mut mac).unwrap();
        assert!(io.pme_enabled);
        assert!(!io.suspended);
        assert!(io.events.contains(&"power-up-phy"));
        assert!(io.events.contains(&"disable-busmaster"));
        io.events.clear();
        io.pme_enabled = false;
        io.suspended = false;
        io.manage_passthrough = false;
        igc_enable_wakeup(&mut io, &mut mac).unwrap();
        assert!(!io.pme_enabled);
        assert!(io.suspended);
        assert!(io.events.contains(&"power-down-phy"));
        io.events.clear();
        io.wake_filters = WakeFilters {
            magic: true,
            unicast: true,
            multicast: true,
        };
        io.suspended = true;
        igc_enable_wakeup(&mut io, &mut mac).unwrap();
        assert!(io.pme_enabled);
        assert_ne!(io.get(WUFC) & (WUFC_MAG | WUFC_EX | WUFC_MC), 0);
        assert_ne!(io.get(RCTL) & RCTL_MPE, 0);
        assert!(!io.suspended);
        assert!(io.events.contains(&"multicast"));
    }
    #[test]
    fn firmware_version_text_uses_source_labels_and_order() {
        let fw = super::super::nvm::FirmwareVersion {
            eep_major: 1,
            eep_minor: 2,
            eep_build: 3,
            or_valid: true,
            or_major: 4,
            or_build: 5,
            or_patch: 6,
            etrack_id: 0x1234_5678,
            ..super::super::nvm::FirmwareVersion::default()
        };
        assert_eq!(
            igc_sbuf_fw_version(&fw),
            "EEPROM V1.2-3 Option ROM V4-b5-p6 eTrack 0x12345678"
        );
        assert_eq!(
            igc_print_fw_version(&super::super::nvm::FirmwareVersion::default()),
            None
        )
    }

    #[test]
    fn attach_pre_keeps_hardware_init_order_and_retries_nvm_checksum_once() {
        let mut io = AttachFake::default();
        let plan = igc_if_attach_pre(&mut io, 256, 512, false).unwrap();
        assert_eq!(plan.max_queues, 4);
        assert_eq!(plan.tx_queue_bytes, 4096);
        assert_eq!(plan.rx_queue_bytes, 8192);
        assert_eq!(plan.max_frame_size, 1518);
        assert_eq!(plan.msix_bar, 0x20);
        assert_eq!(io.checksum_calls, 2);
        assert_eq!(io.mac, [2, 1, 2, 3, 4, 5]);
        assert_eq!(
            io.events,
            [
                "sysctl",
                "identify",
                "l1.2",
                "context",
                "pci",
                "shared",
                "msix",
                "bus",
                "mta",
                "reset-block",
                "reset",
                "checksum",
                "checksum",
                "mac-read",
                "firmware",
                "wakeup",
                "set-mac"
            ]
        );
    }

    #[test]
    fn flow_control_sysctl_policy_validates_and_requests_reinit_only_when_up() {
        let mut setting = FlowControlSetting {
            mode: super::super::mac::FlowMode::Full,
            interface_up: true,
        };
        assert_eq!(
            igc_set_flowcntl(&mut setting, 3),
            Ok(FlowControlUpdate {
                changed: false,
                request_reinit: false,
            })
        );
        assert_eq!(
            igc_set_flowcntl(&mut setting, 1),
            Ok(FlowControlUpdate {
                changed: true,
                request_reinit: true,
            })
        );
        assert_eq!(setting.mode, super::super::mac::FlowMode::RxPause);
        assert_eq!(
            igc_set_flowcntl(&mut setting, 4),
            Err(FlowControlRequestError::InvalidMode)
        );
        setting.interface_up = false;
        assert_eq!(
            igc_set_flowcntl(&mut setting, 0),
            Ok(FlowControlUpdate {
                changed: true,
                request_reinit: false,
            })
        );
    }

    #[test]
    fn dmac_and_eee_sysctl_updates_keep_source_policy() {
        let mut dmac = DmacSetting {
            timer_us: 500,
            interface_up: true,
        };
        assert_eq!(igc_sysctl_dmac(&mut dmac, 1), Ok(true));
        assert_eq!(dmac.timer_us, 1000);
        assert_eq!(igc_sysctl_dmac(&mut dmac, 750), Err(DmacRequestError));
        assert_eq!(dmac.timer_us, 0);
        dmac.interface_up = false;
        assert_eq!(igc_sysctl_dmac(&mut dmac, 500), Ok(false));

        let mut eee = EeeSetting {
            disabled: false,
            interface_up: true,
        };
        assert!(igc_sysctl_eee(&mut eee, 1));
        assert!(eee.disabled);
        eee.interface_up = false;
        assert!(!igc_sysctl_eee(&mut eee, 0));
        assert!(!eee.disabled);
    }

    #[test]
    fn tso_tcp_flag_sysctl_masks_and_selects_register_half() {
        let mut io = Fake::default();
        io.set(DTXTCPFLGL, 0xa5a5_5a5a);
        io.set(DTXTCPFLGH, 0x1234_ffff);
        igc_sysctl_tso_tcp_flags_mask(&mut io, TsoFlagBank::LowUpper, 0x321).unwrap();
        assert_eq!(
            io.get(DTXTCPFLGL),
            (0xa5a5_5a5a & !(0x0fff << 16)) | (0x321 << 16)
        );
        igc_sysctl_tso_tcp_flags_mask(&mut io, TsoFlagBank::HighLower, 0x456).unwrap();
        assert_eq!(io.get(DTXTCPFLGH), (0x1234_ffff & !0x0fff) | 0x456);
        assert_eq!(
            igc_sysctl_tso_tcp_flags_mask(&mut io, TsoFlagBank::LowLower, -1),
            Err(MainError::Bounds)
        );
        assert_eq!(
            igc_sysctl_tso_tcp_flags_mask(&mut io, TsoFlagBank::LowLower, 0x1000),
            Err(MainError::Bounds)
        );
    }

    #[test]
    fn interrupt_rate_sysctl_reads_eitr_and_applies_inverse_conversion() {
        let mut io = Fake::default();
        io.set(CTRL, 0x1234_5678);
        assert_eq!(igc_sysctl_reg_handler(&mut io, CTRL), 0x1234_5678);
        io.set(EITR_BASE + 8, (100 << EITR_SHIFT) | 0x8000_0000);
        assert_eq!(igc_sysctl_interrupt_rate_handler(&mut io, 2), 10_000);
        io.set(EITR_BASE + 12, 0x8000_0000);
        assert_eq!(igc_sysctl_interrupt_rate_handler(&mut io, 3), 0);
        assert!(igc_sysctl_request_reinit(true));
        assert!(!igc_sysctl_request_reinit(false));
    }
}
