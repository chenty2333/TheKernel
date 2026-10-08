//! FreeBSD IGC device/netif policy helpers translated to the NetDriver boundary.
//!
//! Translated from FreeBSD `sys/dev/igc/if_igc.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024 Intel Corporation.
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2021-2024 Rubicon Communications, LLC (Netgate).

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
}
