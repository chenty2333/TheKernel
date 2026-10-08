//! FreeBSD IGC device/netif policy helpers translated to the NetDriver boundary.
//!
//! Translated from FreeBSD `sys/dev/igc/if_igc.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024 Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2021-2024 Rubicon Communications, LLC (Netgate).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

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
}
