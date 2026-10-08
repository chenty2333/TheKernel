//! FreeBSD IGC device/netif policy helpers translated to the NetDriver boundary.
//!
//! Translated from FreeBSD `sys/dev/igc/if_igc.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024 Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2021-2024 Rubicon Communications, LLC (Netgate).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::mac::MacState;

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
const MAX_JUMBO_MTU: u32 = 9234;
const ETHER_HDR_LEN: u32 = 14;
const ETHER_CRC_LEN: u32 = 4;

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
}
pub trait IgcFatalIo: IgcMainIo {
    fn delay_ms(&mut self, ms: u32);
    fn disable_pcie_master(&mut self) -> Result<(), MainError>;
    fn log_parity_reset_timeout(&mut self);
    fn log_master_disable_failure(&mut self);
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
// upstream: if_igc.c igc_is_valid_ether_addr()
pub fn igc_is_valid_ether_addr(address: &[u8; 6]) -> bool {
    address[0] & 1 == 0 && address.iter().any(|b| *b != 0)
}

#[cfg(test)]
mod tests {

    use alloc::vec::Vec;

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
        };
        igc_prepare_fatal_error_reset(&mut io, &fault);
        assert!(io.get(CTRL) & CTRL_DEV_RST != 0);
        assert!(io.events.contains(&"master-off"));
        igc_finish_fatal_error_reset(&mut io, &mut fault);
        assert_eq!(fault.state.load(Ordering::Acquire), 0);
        assert_eq!(fault.peind, 0);
        assert_eq!(io.get(PEIND), 0);
    }
}
