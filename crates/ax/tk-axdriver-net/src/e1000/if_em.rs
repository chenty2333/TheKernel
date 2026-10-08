//! FreeBSD `if_em.c` adapter policy translated to the TheKernel driver layer.
//!
//! Source revision `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024, Intel Corporation; copyright (c) 2016 Nicole
//! Graziano; copyright (c) 2024 Kevin Bowling.

use alloc::collections::BTreeMap;

use axdriver_base::{DevError, DevResult};

use super::{
    api::{self, E1000MacType},
    osdep::{E1000PciConfig, E1000RegisterIo},
    registers::*,
};

const ITR_RATE_DIVIDEND: u64 = 1_000_000_000;
const ITR_RATE_MULTIPLIER: u64 = 256;
const EITR_RATE_DIVIDEND: u32 = 1_000_000;
const EITR_SHIFT: u32 = 2;
const EITR_MASK: u32 = 0x7ffc;
const EITR_COUNT_IGNORE: u32 = 0x8000_0000;
const INTS_4K: u32 = 4_000;
const INTS_20K: u32 = 20_000;
const INTS_70K: u32 = 70_000;
const INTS_DEFAULT: u32 = 8_000;
const MAX_FRAME_NORMAL: u32 = 1518;
const IGB_MAX_FRAME: u32 = 9234;
const MAX_JUMBO_FRAME: u32 = 0x3f00;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AimRing {
    pub snapshot: u64,
    pub bytes_last: u32,
    pub packets_last: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmErrorStats {
    pub corrected_packet_buffer: u64,
    pub uncorrected_packet_buffer: u64,
    pub corrected_dma: u64,
    pub uncorrected_dma: u64,
    pub corrected_pcie_retry: u64,
    pub corrected_pcie_tx_data: u64,
    pub corrected_pcie_other: u64,
    pub uncorrected_pcie: u64,
    pub corrected_lan_mng_fifo: u64,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EmHardwareStats {
    pub counters: BTreeMap<u32, u64>,
    pub pause_frames: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmVfStats {
    pub last: [u32; 9],
    pub total: [u64; 9],
    pub valid: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmIfCounter {
    Collisions,
    InputErrors,
    OutputErrors,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VftaTable {
    pub words: [u32; 128],
    pub stale: [u32; 128],
    pub count: u32,
}
impl Default for VftaTable {
    fn default() -> Self {
        Self {
            words: [0; 128],
            stale: [0; 128],
            count: 0,
        }
    }
}
pub trait EmVlanOps {
    fn set_vf_vlan(&mut self, vid: u16, add: bool) -> DevResult;
    fn rebuild_iov_vlan(&mut self);
    fn write_vfta(&mut self, index: u32, value: u32) -> DevResult;
    fn set_pf_vlan_promisc(&mut self, enabled: bool);
    fn retry_add(&mut self, vid: u16);
    fn retry_clear(&mut self, vid: u16);
    fn write_rlpml(&mut self, size: u32) -> DevResult;
}
pub trait EmMulticastOps {
    fn update_multicast(&mut self, addresses: &[[u8; 6]]) -> DevResult;
    fn update_vf_unicast(&mut self) -> DevResult;
    fn set_vf_promisc(&mut self, promisc: bool, allmulti: bool) -> DevResult;
    fn rebuild_iov_mta(&mut self) -> DevResult;
    fn rebuild_iov_vlan(&mut self) -> DevResult;
    fn update_iov_vmolr(&mut self) -> DevResult;
    fn clear_pci_mwi(&mut self) -> DevResult;
    fn set_pci_mwi(&mut self) -> DevResult;
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmPromiscState {
    pub promisc: bool,
    pub allmulti: bool,
    pub vf: bool,
    pub iov: bool,
    pub num_multicast: u16,
    pub flags_pending: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TsoAutoMask {
    pub capability_enabled: bool,
    pub automasked: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorStatsContext {
    pub mac: E1000MacType,
    pub device_id: u16,
    pub pcie: bool,
    pub ixgbe_fatal_pending: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AimConfig {
    pub mac: E1000MacType,
    pub enable_aim: u8,
    pub link_speed: u16,
    pub max_frame_size: u32,
    pub pba_kb: u32,
    pub intr_type: EmInterruptType,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmInterruptType {
    Intx,
    Msi,
    Msix,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmHardwareIdentity {
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision_id: u8,
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub mac: E1000MacType,
    pub is_vf: bool,
}
pub trait EmPciBusmaster {
    fn command(&mut self) -> DevResult<u16>;
    fn enable_busmaster(&mut self) -> DevResult;
    fn disable_busmaster(&mut self) -> DevResult;
    fn wait_pending_transactions(&mut self, timeout_ms: u32) -> bool;
    fn max_completion_timeout_us(&mut self) -> u32;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmIrqDisposition {
    Stray,
    Handled,
    ScheduleThread,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmIrqActions {
    pub disposition: Option<EmIrqDisposition>,
    pub link_admin: bool,
    pub fatal_error_captured: bool,
    pub overrun: bool,
    pub interrupt_disabled: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmIrqConfig {
    pub mac: E1000MacType,
    pub vf: bool,
    pub reset_state: EmDeviceResetState,
    pub icr_asserted: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmDeviceResetState {
    #[default]
    None,
    Detected,
    Requested,
    Prepared,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmDeviceReset {
    pub state: EmDeviceResetState,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmFatalState {
    #[default]
    None,
    Capturing,
    Detected,
    ResetRequested,
    ResetPrepared,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmFatalError {
    pub state: EmFatalState,
    pub icr: u32,
    pub peind: u32,
    pub pcie: u32,
    pub pcie_ecc: u32,
    pub lan: u32,
    pub dma_tx: u32,
    pub dma_rx: u32,
    pub dma_host: u32,
    pub pbeccsts: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmFatalStats {
    pub reset_count: u64,
    pub unknown_count: u64,
    pub lan_count: u64,
    pub management_count: u64,
    pub pcie_count: u64,
    pub dma_count: u64,
}
pub trait EmFatalAdminOps {
    fn request_reset(&mut self);
    fn reenable_interrupts(&mut self, mask: u32);
}
pub trait EmFatalResetOps {
    fn disable_pcie_master(&mut self) -> DevResult;
}

/// upstream: if_em.c em_set_num_queues()
pub const fn em_set_num_queues(mac: E1000MacType) -> u8 {
    match mac {
        E1000MacType::I82576 | E1000MacType::I82580 | E1000MacType::I350 | E1000MacType::I354 => 8,
        E1000MacType::I82575 | E1000MacType::I210 => 4,
        E1000MacType::I82574 | E1000MacType::I211 => 2,
        _ => 1,
    }
}

/// upstream: if_em.c em_identify_hardware()
pub fn em_identify_hardware<P: E1000PciConfig>(
    pci: &mut P,
    is_vf: bool,
) -> DevResult<EmHardwareIdentity> {
    let command = pci.read_config_u16(0x04).ok_or(DevError::Io)?;
    let vendor = pci.read_config_u16(0x00).ok_or(DevError::Io)?;
    let device = pci.read_config_u16(0x02).ok_or(DevError::Io)?;
    let revision = pci.read_config_u16(0x08).ok_or(DevError::Io)? as u8;
    let sub_vendor = pci.read_config_u16(0x2c).ok_or(DevError::Io)?;
    let sub_device = pci.read_config_u16(0x2e).ok_or(DevError::Io)?;
    let mac = api::set_mac_type(device).map_err(|_| DevError::Unsupported)?;
    let detected_vf = matches!(mac, E1000MacType::VfAdapt | E1000MacType::VfAdaptI350);
    if detected_vf != is_vf {
        return Err(DevError::InvalidParam);
    }
    let _saved_command = command;
    Ok(EmHardwareIdentity {
        vendor_id: vendor,
        device_id: device,
        revision_id: revision,
        subsystem_vendor_id: sub_vendor,
        subsystem_device_id: sub_device,
        mac,
        is_vf,
    })
}

/// upstream: if_em.c em_enable_pci_busmaster()
pub fn em_enable_pci_busmaster<P: EmPciBusmaster>(pci: &mut P) -> DevResult {
    let mut command = pci.command()?;
    if command == u16::MAX {
        return Err(DevError::Io);
    }
    if command & 0x0004 != 0 {
        return Ok(());
    }
    let enable_result = pci.enable_busmaster();
    command = pci.command()?;
    if command == u16::MAX {
        return Err(DevError::Io);
    }
    if command & 0x0004 == 0 {
        return enable_result.and(Err(DevError::Io));
    }
    Ok(())
}

/// upstream: if_em.c em_fence_pci_busmaster()
pub fn em_fence_pci_busmaster<P: EmPciBusmaster>(pci: &mut P) -> DevResult {
    let result = pci.disable_busmaster();
    let command = pci.command()?;
    if command != u16::MAX && command & 0x0004 != 0 {
        return Err(DevError::ResourceBusy);
    }
    let timeout_ms = (pci.max_completion_timeout_us() / 1000).max(10);
    if command != u16::MAX && !pci.wait_pending_transactions(timeout_ms) {
        let after = pci.command()?;
        if after != u16::MAX {
            return Err(DevError::ResourceBusy);
        }
    }
    if result.is_err() && command == u16::MAX {
        return Ok(());
    }
    Ok(())
}

fn aim_delta(snapshot: u64, bytes_last: &mut u32, packets_last: &mut u32) -> (u32, u32) {
    let now_bytes = (snapshot >> 32) as u32;
    let now_packets = snapshot as u32;
    let delta = (
        now_bytes.wrapping_sub(*bytes_last),
        now_packets.wrapping_sub(*packets_last),
    );
    *bytes_last = now_bytes;
    *packets_last = now_packets;
    delta
}

/// upstream: if_em.c em_aim_rx_delta()
pub fn em_aim_rx_delta(ring: &mut AimRing) -> (u32, u32) {
    aim_delta(ring.snapshot, &mut ring.bytes_last, &mut ring.packets_last)
}
/// upstream: if_em.c em_aim_tx_delta()
pub fn em_aim_tx_delta(ring: &mut AimRing) -> (u32, u32) {
    aim_delta(ring.snapshot, &mut ring.bytes_last, &mut ring.packets_last)
}

/// upstream: if_em.c em_ring_itr()
pub fn em_ring_itr(
    rx_bytes: u32,
    rx_packets: u32,
    tx_bytes: u32,
    tx_packets: u32,
    enable_aim: u8,
) -> u32 {
    let mut average = 0;
    if tx_bytes != 0 && tx_packets != 0 {
        average = tx_bytes / tx_packets;
    }
    if rx_bytes != 0 && rx_packets != 0 {
        average = average.max(rx_bytes / rx_packets);
    }
    if average == 0 {
        return 0;
    }
    average = (average + 24).min(3000);
    average = if average > 300 && average < 1200 {
        average / 3
    } else {
        average / 2
    };
    let ceiling = if enable_aim == 1 { INTS_20K } else { INTS_70K };
    ((EITR_RATE_DIVIDEND << EITR_SHIFT) / average).min(ceiling)
}

/// upstream: if_em.c em_newitr()
pub fn em_newitr<I: E1000RegisterIo>(
    io: &mut I,
    config: AimConfig,
    vector: u16,
    queue_itr: &mut u32,
    rx_delta: (u32, u32),
    tx_deltas: &[(u16, u32, u32)],
) -> DevResult<bool> {
    let mut tx_bytes = 0u32;
    let mut tx_packets = 0u32;
    for (tx_vector, bytes, packets) in tx_deltas {
        if *tx_vector == vector {
            tx_bytes = tx_bytes.wrapping_add(*bytes);
            tx_packets = tx_packets.wrapping_add(*packets);
        }
    }
    let (rx_bytes, rx_packets) = rx_delta;
    if tx_bytes == 0 && rx_bytes == 0 {
        return Ok(false);
    }
    let mut rate = if config.enable_aim == 0 {
        INTS_DEFAULT
    } else if config.link_speed < 1000 {
        INTS_4K
    } else if config.mac >= E1000MacType::I82575
        && config.max_frame_size.saturating_mul(2) > (config.pba_kb << 10)
    {
        INTS_DEFAULT
    } else {
        let interval = em_ring_itr(
            rx_bytes,
            rx_packets,
            tx_bytes,
            tx_packets,
            config.enable_aim,
        );
        if interval == 0 {
            return Ok(false);
        }
        interval
    };
    if config.mac >= E1000MacType::I82575 {
        rate = ((EITR_RATE_DIVIDEND / rate) << EITR_SHIFT) & EITR_MASK;
        if config.mac == E1000MacType::I82575 {
            rate |= rate << 16;
        } else {
            rate |= EITR_COUNT_IGNORE;
        }
        if rate == *queue_itr {
            return Ok(false);
        }
        *queue_itr = rate;
        io.write_register(0x01680 + u32::from(vector) * 4, rate)?;
    } else {
        rate = (ITR_RATE_DIVIDEND * 1000 / (u64::from(rate) * ITR_RATE_MULTIPLIER)) as u32;
        if rate == *queue_itr {
            return Ok(false);
        }
        *queue_itr = rate;
        if config.mac == E1000MacType::I82574 && config.intr_type == EmInterruptType::Msix {
            io.write_register(0x000e8 + u32::from(vector) * 4, rate)?;
        } else {
            io.write_register(E1000_ITR, rate)?;
        }
    }
    Ok(true)
}

/// upstream: if_em.c em_has_pch_ecc()
pub fn em_has_pch_ecc(mac: E1000MacType) -> bool {
    mac >= E1000MacType::PchLpt && mac < E1000MacType::I82575
}
/// upstream: if_em.c em_has_82571_ecc_stats()
pub const fn em_has_82571_ecc_stats(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I82571)
}
/// upstream: if_em.c em_has_82575_memory_errors()
pub const fn em_has_82575_memory_errors(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I82575)
}
/// upstream: if_em.c em_has_82576_memory_errors()
pub const fn em_has_82576_memory_errors(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I82576)
}
/// upstream: if_em.c em_82576_has_ipsec()
pub const fn em_82576_has_ipsec(device_id: u16) -> bool {
    device_id != 0x150a && device_id != 0x1518
}
/// upstream: if_em.c em_has_82580_memory_errors()
pub const fn em_has_82580_memory_errors(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I82580)
}
/// upstream: if_em.c em_has_i210_memory_errors()
pub const fn em_has_i210_memory_errors(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I210 | E1000MacType::I211)
}
/// upstream: if_em.c em_has_i350_i354_memory_errors()
pub const fn em_has_i350_i354_memory_errors(mac: E1000MacType) -> bool {
    matches!(mac, E1000MacType::I350 | E1000MacType::I354)
}
/// upstream: if_em.c em_has_peind_memory_errors()
pub const fn em_has_peind_memory_errors(mac: E1000MacType) -> bool {
    em_has_82580_memory_errors(mac)
        || em_has_i350_i354_memory_errors(mac)
        || em_has_i210_memory_errors(mac)
}

/// upstream: if_em.c em_pcie_fatal_error_mask()
pub const fn em_pcie_fatal_error_mask(mac: E1000MacType) -> u32 {
    if em_has_82580_memory_errors(mac) {
        u32::MAX
    } else if em_has_i350_i354_memory_errors(mac) {
        0x0000_007c
    } else if em_has_i210_memory_errors(mac) {
        0x0000_0078
    } else {
        0
    }
}
/// upstream: if_em.c em_memory_error_intr_mask()
pub fn em_memory_error_intr_mask(mac: E1000MacType) -> u32 {
    if em_has_82575_memory_errors(mac) {
        0x03c0_0000
    } else if em_has_82576_memory_errors(mac) {
        0x00c0_0000
    } else if em_has_pch_ecc(mac) || em_has_peind_memory_errors(mac) {
        0x0040_0000
    } else {
        0
    }
}
/// upstream: if_em.c em_has_memory_errors()
pub fn em_has_memory_errors(mac: E1000MacType) -> bool {
    em_memory_error_intr_mask(mac) != 0
}
/// upstream: if_em.c em_has_memory_error_stats()
pub fn em_has_memory_error_stats(mac: E1000MacType) -> bool {
    em_has_82571_ecc_stats(mac) || em_has_memory_errors(mac)
}
/// upstream: if_em.c em_fatal_error_intr_mask()
pub fn em_fatal_error_intr_mask(mac: E1000MacType, fatal_reset_pending: bool) -> u32 {
    if fatal_reset_pending {
        0
    } else {
        em_memory_error_intr_mask(mac)
    }
}

/// upstream: if_em.c em_if_mtu_set()
pub fn em_if_mtu_set(mac: E1000MacType, mtu: u32, hyperv_vf: bool) -> DevResult<u32> {
    if hyperv_vf && mtu > 1500 {
        return Err(DevError::InvalidParam);
    }
    let max_frame = match mac {
        E1000MacType::I82571
        | E1000MacType::I82572
        | E1000MacType::Ich9Lan
        | E1000MacType::Ich10Lan
        | E1000MacType::Pch2Lan
        | E1000MacType::PchLpt
        | E1000MacType::PchSpt
        | E1000MacType::PchCnp
        | E1000MacType::PchTgp
        | E1000MacType::PchAdp
        | E1000MacType::PchMtp
        | E1000MacType::PchPtp
        | E1000MacType::PchNvp
        | E1000MacType::I82574
        | E1000MacType::I82583
        | E1000MacType::I80003Es2lan => 9234,
        E1000MacType::PchLan => 4096,
        E1000MacType::I82542 | E1000MacType::Ich8Lan => MAX_FRAME_NORMAL,
        _ if mac >= E1000MacType::I82575 => IGB_MAX_FRAME,
        _ => MAX_JUMBO_FRAME,
    };
    let framed = mtu.saturating_add(18);
    if framed > max_frame {
        return Err(DevError::InvalidParam);
    }
    Ok(framed)
}

/// upstream: if_em.c em_is_valid_ether_addr()
pub fn em_is_valid_ether_addr(address: [u8; 6]) -> bool {
    (address[0] & 1) == 0 && address != [0; 6]
}

/// upstream: if_em.c em_mac_has_eee()
pub fn em_mac_has_eee(mac: E1000MacType) -> bool {
    (mac >= E1000MacType::Pch2Lan && mac < E1000MacType::I82575)
        || (mac >= E1000MacType::I350 && mac <= E1000MacType::I211)
}

/// upstream: if_em.c em_automask_tso()
pub fn em_automask_tso(
    unsupported_tso: bool,
    link_speed: u16,
    is_up: bool,
    tso: &mut TsoAutoMask,
) -> bool {
    let changed =
        if !unsupported_tso && link_speed != 0 && link_speed != 1000 && tso.capability_enabled {
            tso.automasked = true;
            tso.capability_enabled = false;
            true
        } else if link_speed == 1000 && tso.automasked {
            tso.capability_enabled = true;
            tso.automasked = false;
            true
        } else {
            false
        };
    changed && is_up
}

/// upstream: if_em.c em_update_82580_ecc_stats()
pub fn em_update_82580_ecc_stats(
    stats: &mut EmErrorStats,
    rpbeccsts: u32,
    tpbeccsts: u32,
    pcieeccsts: u32,
) -> u32 {
    stats.corrected_packet_buffer += u64::from(
        (rpbeccsts & E1000_PBECCSTS_82580_CORR_CNT_MASK)
            + (tpbeccsts & E1000_PBECCSTS_82580_CORR_CNT_MASK),
    );
    let status = pcieeccsts & E1000_PCIEECCSTS_82580_ERROR_MASK;
    stats.uncorrected_pcie += u64::from(status.count_ones());
    status
}
/// upstream: if_em.c em_update_82575_ecc_stats()
pub fn em_update_82575_ecc_stats(
    stats: &mut EmErrorStats,
    pbeccsts: u32,
    rdhests: u32,
    tdhests: u32,
) {
    stats.corrected_packet_buffer += u64::from(pbeccsts & E1000_ECC_82575_CORR_CNT_MASK);
    stats.uncorrected_packet_buffer +=
        u64::from((pbeccsts & E1000_ECC_82575_UNCORR_CNT_MASK) >> E1000_ECC_82575_UNCORR_CNT_SHIFT);
    stats.corrected_dma += u64::from(
        (rdhests & E1000_ECC_82575_CORR_CNT_MASK) + (tdhests & E1000_ECC_82575_CORR_CNT_MASK),
    );
    stats.uncorrected_dma += u64::from(
        ((rdhests & E1000_ECC_82575_UNCORR_CNT_MASK) >> E1000_ECC_82575_UNCORR_CNT_SHIFT)
            + ((tdhests & E1000_ECC_82575_UNCORR_CNT_MASK) >> E1000_ECC_82575_UNCORR_CNT_SHIFT),
    );
}
/// upstream: if_em.c em_update_82576_ecc_counter()
pub fn em_update_82576_ecc_counter(
    status: u32,
    corrected: &mut u64,
    uncorrected: Option<&mut u64>,
) {
    *corrected += u64::from(status & E1000_ECC_82576_CORR_CNT_MASK);
    if let Some(value) = uncorrected {
        *value += u64::from(
            (status & E1000_ECC_82576_UNCORR_CNT_MASK) >> E1000_ECC_82576_UNCORR_CNT_SHIFT,
        );
    }
}
/// upstream: if_em.c em_update_pch_ecc_stats()
pub fn em_update_pch_ecc_stats(stats: &mut EmErrorStats, status: u32) {
    stats.corrected_packet_buffer += u64::from(status & 0xff);
    stats.uncorrected_packet_buffer += u64::from((status >> 8) & 0xff);
}

/// upstream: if_em.c em_configure_82575_memory_errors()
pub fn em_configure_82575_memory_errors<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
) -> DevResult {
    if !em_has_82575_memory_errors(mac) {
        return Ok(());
    }
    let _ = io.read_register(E1000_PBECCSTS_82575)?;
    let _ = io.read_register(E1000_RDHESTS_82575)?;
    let _ = io.read_register(E1000_TDHESTS_82575)?;
    for reg in [
        E1000_PBECCSTS_82575,
        E1000_RDHESTS_82575,
        E1000_TDHESTS_82575,
    ] {
        io.write_register(reg, E1000_ECC_82575_ENABLE)?;
    }
    let ctrl = io.read_register(E1000_CTRL_EXT)?;
    io.write_register(E1000_CTRL_EXT, ctrl | E1000_CTRL_EXT_MEHE)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_configure_82576_memory_errors()
pub fn em_configure_82576_memory_errors<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    device_id: u16,
) -> DevResult {
    if !em_has_82576_memory_errors(mac) {
        return Ok(());
    }
    let mut reactions = E1000_PEIND_82576_NONFATAL_MASK
        | E1000_PEIND_82576_FATAL_MASK
        | E1000_PEINDM_82576_PARITY_ENABLE;
    if !em_82576_has_ipsec(device_id) {
        reactions &= !E1000_PEIND_82576_IPSEC_MASK;
    }
    let _ = io.read_register(E1000_PEIND)?;
    let status = io.read_register(E1000_PEINDM)?;
    io.write_register(E1000_PEINDM, status | reactions)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_clear_82580_memory_error_status()
pub fn em_clear_82580_memory_error_status<I: E1000RegisterIo>(
    io: &mut I,
    register: u32,
) -> DevResult {
    let status = io.read_register(register)?;
    if status != 0 {
        io.write_register(register, status)?;
    }
    Ok(())
}

/// upstream: if_em.c em_configure_82580_memory_errors()
pub fn em_configure_82580_memory_errors<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmErrorStats,
    mac: E1000MacType,
    rx_queues: u16,
) -> DevResult {
    if !em_has_82580_memory_errors(mac) {
        return Ok(());
    }
    let _ = io.read_register(E1000_PEIND)?;
    for reg in [
        E1000_DTPARS_82580,
        E1000_DRPARS_82580,
        E1000_DDPARS_82580,
        0x05b00,
    ] {
        em_clear_82580_memory_error_status(io, reg)?;
    }
    let _ = io.read_register(E1000_LANPERRSTS)?;
    let _ = em_update_82580_ecc_stats(
        stats,
        io.read_register(E1000_RPBECCSTS)?,
        io.read_register(E1000_TPBECCSTS)?,
        io.read_register(E1000_PCIEECCSTS)?,
    );
    io.write_register(E1000_RPBECCSTS, E1000_PBECCSTS_82580_ECC_ENABLE)?;
    io.write_register(E1000_TPBECCSTS, E1000_PBECCSTS_82580_ECC_ENABLE)?;
    for (reg, mask) in [
        (E1000_DTPARC_82580, E1000_DTPARC_82580_ENABLE_MASK),
        (E1000_DRPARC_82580, E1000_DRPARC_82580_ENABLE_MASK),
        (E1000_DDPARC_82580, E1000_DDPARC_82580_ENABLE_MASK),
        (E1000_PCIEERRCTL_82580, E1000_PCIEERRCTL_82580_ENABLE_MASK),
        (E1000_PCIEECCCTL_82580, E1000_PCIEECCCTL_82580_ENABLE_MASK),
    ] {
        let value = io.read_register(reg)?;
        io.write_register(reg, value | mask)?;
    }
    let mut lane = io.read_register(E1000_LANPERRCTL_82580)? | E1000_LANPERRCTL_82580_HOST_MASK;
    if rx_queues <= 1 {
        lane &= !E1000_LANPERRCTL_82580_RSS_ENABLE;
    }
    io.write_register(E1000_LANPERRCTL_82580, lane)?;
    let peindm = io.read_register(E1000_PEINDM)?;
    io.write_register(E1000_PEINDM, peindm | E1000_PEIND_FATAL_MASK)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_configure_peind_memory_errors()
pub fn em_configure_peind_memory_errors<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
) -> DevResult {
    if !em_has_i350_i354_memory_errors(mac) && !em_has_i210_memory_errors(mac) {
        return Ok(());
    }
    let _ = io.read_register(E1000_PEIND)?;
    let value = io.read_register(E1000_PEINDM)?;
    io.write_register(E1000_PEINDM, value | E1000_PEIND_FATAL_MASK)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_update_82576_ecc_stats()
pub fn em_update_82576_ecc_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmErrorStats,
    has_ipsec: bool,
) -> DevResult {
    for reg in [E1000_RPBECCSTS, E1000_TPBECCSTS, E1000_SWPBECCSTS_82576] {
        let status = io.read_register(reg)?;
        em_update_82576_ecc_counter(
            status,
            &mut stats.corrected_packet_buffer,
            Some(&mut stats.uncorrected_packet_buffer),
        );
    }
    if has_ipsec {
        let status = io.read_register(E1000_IPPBECCSTS_82576)?;
        em_update_82576_ecc_counter(
            status,
            &mut stats.corrected_packet_buffer,
            Some(&mut stats.uncorrected_packet_buffer),
        );
    }
    for reg in [E1000_RDHESTS_82576, E1000_TDHESTS_82576] {
        let status = io.read_register(reg)?;
        em_update_82576_ecc_counter(
            status,
            &mut stats.corrected_dma,
            Some(&mut stats.uncorrected_dma),
        );
    }
    for (reg, target) in [
        (E1000_PRBESTS_82576, &mut stats.corrected_pcie_retry),
        (E1000_PWBESTS_82576, &mut stats.corrected_pcie_tx_data),
        (E1000_PMSIXESTS_82576, &mut stats.corrected_pcie_other),
    ] {
        let status = io.read_register(reg)?;
        em_update_82576_ecc_counter(status, target, None);
    }
    Ok(())
}

/// upstream: if_em.c em_update_82571_ecc_stats()
pub fn em_update_82571_ecc_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmErrorStats,
) -> DevResult {
    let value = io.read_register(E1000_PBA_ECC)?;
    let count = (value & E1000_PBA_ECC_COUNTER_MASK) >> E1000_PBA_ECC_COUNTER_SHIFT;
    if count != 0 {
        stats.corrected_packet_buffer += u64::from(count);
        io.write_register(E1000_PBA_ECC, value | E1000_PBA_ECC_STAT_CLR)?;
    }
    Ok(())
}

/// upstream: if_em.c em_update_i210_ecc_stats()
pub fn em_update_i210_ecc_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmErrorStats,
) -> DevResult {
    let pbeccsts = io.read_register(E1000_PBECCSTS_I210)?;
    if pbeccsts & E1000_PBECCSTS_I210_CORR_ERR != 0 {
        stats.corrected_dma += 1;
        io.write_register(
            E1000_PBECCSTS_I210,
            pbeccsts & (E1000_PBECCSTS_I210_ECC_ENABLE | E1000_PBECCSTS_I210_CORR_ERR),
        )?;
    }
    let status = io.read_register(E1000_PCIEECCSTS)? & E1000_PCIEECCSTS_I210_CORR_MASK;
    if status & E1000_PCIEECCSTS_TX_WR_DATA != 0 {
        stats.corrected_pcie_tx_data += 1;
    }
    if status & E1000_PCIEECCSTS_RETRY_BUF != 0 {
        stats.corrected_pcie_retry += 1;
    }
    if status != 0 {
        io.write_register(E1000_PCIEECCSTS, status)?;
    }
    Ok(())
}

/// upstream: if_em.c em_update_i350_i354_ecc_stats()
pub fn em_update_i350_i354_ecc_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmErrorStats,
    mac: E1000MacType,
) -> DevResult {
    for (reg, mask) in [
        (E1000_DTPARS, E1000_DTPARS_CORR_MASK),
        (E1000_DRPARS, E1000_DRPARS_CORR_MASK),
        (E1000_DDECCS, E1000_DDECCS_CORR_MASK),
    ] {
        let status = io.read_register(reg)? & mask;
        if status != 0 {
            stats.corrected_dma += u64::from(status.count_ones());
            io.write_register(reg, status)?;
        }
    }
    let lane = io.read_register(E1000_LANPERRSTS)? & E1000_LANPERRSTS_MNG_FIFO_CORR;
    if lane != 0 {
        stats.corrected_lan_mng_fifo += 1;
        io.write_register(E1000_LANPERRSTS, lane)?;
    }
    for reg in [E1000_RPBECCSTS, E1000_TPBECCSTS] {
        let value = io.read_register(reg)?;
        let status = value & E1000_PBECCSTS_I350_I354_CORR_MASK;
        if status != 0 {
            stats.corrected_packet_buffer += u64::from(status.count_ones());
            io.write_register(
                reg,
                value & (E1000_PBECCSTS_I350_I354_ENABLE_MASK | E1000_PBECCSTS_I350_I354_CORR_MASK),
            )?;
        }
    }
    let mask = if mac == E1000MacType::I354 {
        E1000_PCIEECCSTS_I354_CORR_MASK
    } else {
        E1000_PCIEECCSTS_I350_CORR_MASK
    };
    let status = io.read_register(E1000_PCIEECCSTS)? & mask;
    if status & E1000_PCIEECCSTS_TX_WR_DATA != 0 {
        stats.corrected_pcie_tx_data += 1;
    }
    if status & E1000_PCIEECCSTS_RETRY_BUF != 0 {
        stats.corrected_pcie_retry += 1;
    }
    stats.corrected_pcie_other +=
        u64::from((status & E1000_PCIEECCSTS_I350_I354_OTHER_MASK).count_ones());
    if status != 0 {
        io.write_register(E1000_PCIEECCSTS, status)?;
    }
    Ok(())
}

/// upstream: if_em.c em_handle_fatal_error_intr()
pub fn em_handle_fatal_error_intr<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    _device_id: u16,
    icr: u32,
    error: &mut EmFatalError,
) -> DevResult<bool> {
    let error_mask = em_memory_error_intr_mask(mac);
    if error_mask == 0 || icr & error_mask == 0 {
        return Ok(false);
    }
    io.write_register(E1000_IMC, error_mask)?;
    if error.state != EmFatalState::None {
        return Ok(false);
    }
    error.state = EmFatalState::Capturing;
    error.icr = icr & error_mask;
    if em_has_pch_ecc(mac) {
        error.pbeccsts = io.read_register(E1000_PBECCSTS)?;
    } else if em_has_82575_memory_errors(mac) {
        error.pbeccsts = io.read_register(E1000_PBECCSTS_82575)?;
        error.dma_rx = io.read_register(E1000_RDHESTS_82575)?;
        error.dma_tx = io.read_register(E1000_TDHESTS_82575)?;
    } else if em_has_82576_memory_errors(mac) {
        error.peind = io.read_register(E1000_PEIND)?;
    } else {
        let mut peind = io.read_register(E1000_PEIND)? & E1000_PEIND_FATAL_MASK;
        let pcie = io.read_register(E1000_PCIEERRSTS)? & em_pcie_fatal_error_mask(mac);
        let mut host = 0;
        if em_has_82580_memory_errors(mac) {
            peind &= E1000_PEIND_MNG_PARITY_FATAL;
            error.dma_tx = io.read_register(E1000_DTPARS_82580)?;
            error.dma_rx = io.read_register(E1000_DRPARS_82580)?;
            host = io.read_register(E1000_DDPARS_82580)?;
            error.lan = io.read_register(E1000_LANPERRSTS)? & E1000_LANPERRSTS_82580_ERROR_MASK;
        } else if em_has_i350_i354_memory_errors(mac) {
            error.dma_tx = io.read_register(E1000_DTPARS)? & E1000_DTPARS_FATAL_MASK;
            error.dma_rx = io.read_register(E1000_DRPARS)? & E1000_DRPARS_FATAL_MASK;
            error.lan = io.read_register(E1000_LANPERRSTS)? & E1000_LANPERRSTS_I350_I354_FATAL_MASK;
        } else {
            error.lan = io.read_register(E1000_LANPERRSTS)? & E1000_LANPERRSTS_RETX_BUF;
        }
        if pcie != 0 {
            peind |= E1000_PEIND_PCIE_PARITY_FATAL;
        }
        if error.lan != 0 {
            peind |= E1000_PEIND_LANPORT_PARITY_FATAL;
        }
        if error.dma_tx != 0 || error.dma_rx != 0 || host != 0 {
            peind |= E1000_PEIND_DMA_PARITY_FATAL;
        }
        error.peind = peind;
        error.pcie = pcie;
        error.dma_host = host;
    }
    error.state = EmFatalState::Detected;
    Ok(true)
}

/// upstream: if_em.c igb_device_reset_intr_mask()
pub fn igb_device_reset_intr_mask(mac: E1000MacType) -> u32 {
    if mac >= E1000MacType::I82580 {
        E1000_ICR_DRSTA
    } else {
        0
    }
}
/// upstream: if_em.c igb_device_reset_pending()
pub fn igb_device_reset_pending(mac: E1000MacType, state: EmDeviceResetState, vf: bool) -> bool {
    !vf && igb_device_reset_intr_mask(mac) != 0 && state != EmDeviceResetState::None
}
/// upstream: if_em.c igb_handle_device_reset()
pub fn igb_handle_device_reset(
    mac: E1000MacType,
    vf: bool,
    icr: u32,
    reset: &mut EmDeviceReset,
) -> bool {
    if vf || igb_device_reset_intr_mask(mac) == 0 || icr & E1000_ICR_DRSTA == 0 {
        return false;
    }
    let old = reset.state;
    reset.state = EmDeviceResetState::Detected;
    old != EmDeviceResetState::Detected
}

/// upstream: if_em.c igb_prepare_device_reset()
pub fn igb_prepare_device_reset<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    reset: &mut EmDeviceReset,
    timeout_ms: usize,
) -> DevResult {
    if !matches!(
        reset.state,
        EmDeviceResetState::Detected | EmDeviceResetState::Requested
    ) && mac >= E1000MacType::I82580
    {
        let gcr = io.read_register(E1000_GCR)?;
        if gcr & 0x8000_0000 != 0 {
            reset.state = EmDeviceResetState::Detected
        } else {
            let status = io.read_register(E1000_STATUS)?;
            if status & 0x0010_0000 != 0 {
                reset.state = EmDeviceResetState::Detected
            }
        }
    }
    if !matches!(
        reset.state,
        EmDeviceResetState::Detected | EmDeviceResetState::Requested
    ) {
        return Ok(());
    }
    if mac >= E1000MacType::I82580 {
        let mut complete = false;
        for _ in 0..timeout_ms {
            if io.read_register(E1000_GCR)? & 0x8000_0000 == 0 {
                complete = true;
                break;
            }
            io.delay_us(1000)
        }
        if complete {
            io.write_register(E1000_STATUS, 0x0010_0000)?;
            if mac >= E1000MacType::I350 {
                for _ in 0..timeout_ms {
                    let eecd = io.read_register(E1000_EECD)?;
                    let status = io.read_register(E1000_STATUS)?;
                    if eecd & E1000_EECD_AUTO_RD != 0 && status & E1000_STATUS_RST_DONE != 0 {
                        break;
                    }
                    io.delay_us(1000)
                }
            }
        }
    }
    reset.state = EmDeviceResetState::Prepared;
    Ok(())
}

pub trait EmPciStatus {
    fn vendor_id(&mut self) -> DevResult<u16>;
}
/// upstream: if_em.c igb_finish_device_reset()
pub fn igb_finish_device_reset<I: E1000RegisterIo, P: EmPciStatus>(
    io: &mut I,
    pci: &mut P,
    mac: E1000MacType,
    reset: &mut EmDeviceReset,
    icr: u32,
) -> DevResult<bool> {
    let mut again = icr != u32::MAX && icr & E1000_ICR_DRSTA != 0;
    if mac >= E1000MacType::I82580 {
        let gcr = io.read_register(E1000_GCR)?;
        if gcr != u32::MAX && gcr & 0x8000_0000 != 0 {
            again = true
        }
        let status = io.read_register(E1000_STATUS)?;
        if status == u32::MAX && reset.state != EmDeviceResetState::None {
            if pci.vendor_id()? == 0xffff {
                reset.state = EmDeviceResetState::Detected;
                return Ok(true);
            }
            again = true
        } else if status != u32::MAX && status & 0x0010_0000 != 0 {
            again = true
        }
    }
    if matches!(
        reset.state,
        EmDeviceResetState::Detected | EmDeviceResetState::Requested
    ) {
        again = true
    }
    if !again {
        if reset.state == EmDeviceResetState::Prepared {
            reset.state = EmDeviceResetState::None
        }
        return Ok(false);
    }
    let was_detected = reset.state == EmDeviceResetState::Detected;
    reset.state = EmDeviceResetState::Detected;
    Ok(!was_detected)
}

/// upstream: if_em.c em_handle_fatal_error_admin()
pub fn em_handle_fatal_error_admin<I: E1000RegisterIo, O: EmFatalAdminOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    error: &mut EmFatalError,
    stats: &mut EmErrorStats,
    counts: &mut EmFatalStats,
) -> DevResult<bool> {
    if error.state != EmFatalState::Detected {
        return Ok(error.state != EmFatalState::None);
    }
    error.state = EmFatalState::ResetRequested;
    if em_has_pch_ecc(mac) {
        em_update_pch_ecc_stats(stats, error.pbeccsts);
    } else if em_has_82575_memory_errors(mac) {
        em_update_82575_ecc_stats(stats, error.pbeccsts, error.dma_rx, error.dma_tx);
    } else if em_has_82576_memory_errors(mac) {
        em_update_82576_ecc_stats(io, stats, true)?;
        let reset = (error.icr & 0x0040_0000) != 0
            || (error.peind & (E1000_PEIND_82576_FATAL_MASK | E1000_PEIND_82576_MEMORY_HANG)) != 0;
        if !reset {
            error.state = EmFatalState::None;
            error.icr = 0;
            error.peind = 0;
            ops.reenable_interrupts(0x00c0_0000);
            return Ok(true);
        }
        if error.peind & (E1000_PEIND_82576_FATAL_MASK | E1000_PEIND_82576_MEMORY_HANG) == 0 {
            counts.unknown_count += 1;
        }
    } else {
        let mut peind = error.peind;
        if em_has_82580_memory_errors(mac) {
            let pcieecc = io.read_register(E1000_PCIEECCSTS)? & E1000_PCIEECCSTS_82580_ERROR_MASK;
            error.pcie_ecc |= pcieecc;
            if pcieecc != 0 {
                peind |= E1000_PEIND_PCIE_PARITY_FATAL;
                error.peind = peind;
            }
            let _ = em_update_82580_ecc_stats(
                stats,
                io.read_register(E1000_RPBECCSTS)?,
                io.read_register(E1000_TPBECCSTS)?,
                pcieecc,
            );
        } else if em_has_i350_i354_memory_errors(mac) {
            em_update_i350_i354_ecc_stats(io, stats, mac)?;
        }
        if peind & E1000_PEIND_LANPORT_PARITY_FATAL != 0 {
            counts.lan_count += 1;
        }
        if peind & E1000_PEIND_MNG_PARITY_FATAL != 0 {
            counts.management_count += 1;
        }
        if peind & E1000_PEIND_PCIE_PARITY_FATAL != 0 {
            counts.pcie_count += 1;
        }
        if peind & E1000_PEIND_DMA_PARITY_FATAL != 0 {
            counts.dma_count += 1;
        }
        if peind == 0 {
            counts.unknown_count += 1;
        }
        let mut reset =
            (peind & (E1000_PEIND_PCIE_PARITY_FATAL | E1000_PEIND_DMA_PARITY_FATAL)) != 0;
        if peind == 0 {
            reset = true;
        }
        if peind & E1000_PEIND_LANPORT_PARITY_FATAL != 0
            && (!em_has_i350_i354_memory_errors(mac)
                || error.lan == 0
                || (error.lan & E1000_LANPERRSTS_I350_I354_RESET_MASK) != 0)
        {
            reset = true;
        }
        if !reset {
            if em_has_i350_i354_memory_errors(mac) && error.lan != 0 {
                io.write_register(
                    E1000_LANPERRSTS,
                    error.lan & E1000_LANPERRSTS_I350_I354_NO_RESET_MASK,
                )?;
            }
            error.peind = 0;
            error.pcie = 0;
            error.pcie_ecc = 0;
            error.lan = 0;
            error.dma_tx = 0;
            error.dma_rx = 0;
            error.dma_host = 0;
            error.state = EmFatalState::None;
            ops.reenable_interrupts(0x0040_0000);
            return Ok(true);
        }
    }
    counts.reset_count += 1;
    ops.request_reset();
    Ok(true)
}

/// upstream: if_em.c em_finish_fatal_error_reset()
pub fn em_finish_fatal_error_reset<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    error: &mut EmFatalError,
) -> DevResult {
    if !matches!(
        error.state,
        EmFatalState::ResetRequested | EmFatalState::ResetPrepared
    ) {
        return Ok(());
    }
    if em_has_82575_memory_errors(mac) {
        error.dma_tx = 0;
        error.dma_rx = 0;
    } else if em_has_82576_memory_errors(mac) {
        let _ = io.read_register(E1000_PEIND)?;
        error.peind = 0;
    } else if em_has_82580_memory_errors(mac) {
        let pcie = error.pcie | io.read_register(E1000_PCIEERRSTS)?;
        if pcie != 0 {
            io.write_register(E1000_PCIEERRSTS, pcie)?;
        }
        let ecc = error.pcie_ecc
            | (io.read_register(E1000_PCIEECCSTS)? & E1000_PCIEECCSTS_82580_ERROR_MASK);
        if ecc != 0 {
            io.write_register(E1000_PCIEECCSTS, ecc)?;
        }
        for (reg, saved) in [
            (E1000_DTPARS_82580, error.dma_tx),
            (E1000_DRPARS_82580, error.dma_rx),
            (E1000_DDPARS_82580, error.dma_host),
        ] {
            let status = saved | io.read_register(reg)?;
            if status != 0 {
                io.write_register(reg, status)?;
            }
        }
        let _ = io.read_register(E1000_LANPERRSTS)? & E1000_LANPERRSTS_82580_ERROR_MASK;
        let _ = io.read_register(E1000_PEIND)?;
        error.peind = 0;
        error.pcie = 0;
        error.pcie_ecc = 0;
        error.lan = 0;
        error.dma_tx = 0;
        error.dma_rx = 0;
        error.dma_host = 0;
    } else if em_has_peind_memory_errors(mac) {
        let pcie =
            error.pcie | (io.read_register(E1000_PCIEERRSTS)? & em_pcie_fatal_error_mask(mac));
        if pcie != 0 {
            io.write_register(E1000_PCIEERRSTS, pcie)?;
        }
        if em_has_i350_i354_memory_errors(mac) {
            for (reg, saved, mask) in [
                (E1000_DTPARS, error.dma_tx, E1000_DTPARS_FATAL_MASK),
                (E1000_DRPARS, error.dma_rx, E1000_DRPARS_FATAL_MASK),
            ] {
                let status = saved | (io.read_register(reg)? & mask);
                if status != 0 {
                    io.write_register(reg, status)?;
                }
            }
        }
        let lanmask = if em_has_i350_i354_memory_errors(mac) {
            E1000_LANPERRSTS_I350_I354_FATAL_MASK
        } else {
            E1000_LANPERRSTS_RETX_BUF
        };
        let lan = error.lan | (io.read_register(E1000_LANPERRSTS)? & lanmask);
        if lan != 0 {
            io.write_register(E1000_LANPERRSTS, lan)?;
        }
        let _ = io.read_register(E1000_PEIND)?;
        error.peind = 0;
        error.pcie = 0;
        error.pcie_ecc = 0;
        error.lan = 0;
        error.dma_tx = 0;
        error.dma_rx = 0;
        error.dma_host = 0;
    }
    error.icr = 0;
    error.pbeccsts = 0;
    error.state = EmFatalState::None;
    Ok(())
}

pub trait EmFlushOps {
    fn write_dummy_tx_descriptor(
        &mut self,
        index: u16,
        bus_address: u64,
        command_length: u32,
    ) -> DevResult;
    fn write_memory_barrier(&mut self);
    fn read_descriptor_ring_status(&mut self) -> DevResult<u16>;
}

/// upstream: if_em.c em_flush_tx_ring()
pub fn em_flush_tx_ring<I: E1000RegisterIo, O: EmFlushOps>(
    io: &mut I,
    ops: &mut O,
    ring_bus: u64,
    processed_index: u16,
) -> DevResult {
    let tctl = io.read_register(E1000_TCTL)?;
    io.write_register(E1000_TCTL, tctl | E1000_TCTL_EN)?;
    ops.write_dummy_tx_descriptor(processed_index, ring_bus, E1000_TXD_CMD_IFCS | 512)?;
    ops.write_memory_barrier();
    io.write_register(tx_desc_tail(0), u32::from(processed_index))?;
    ops.write_memory_barrier();
    io.delay_us(250);
    Ok(())
}

/// upstream: if_em.c em_flush_rx_ring()
pub fn em_flush_rx_ring<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let rctl = io.read_register(E1000_RCTL)?;
    io.write_register(E1000_RCTL, rctl & !E1000_RCTL_EN)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(150);
    let reg = rx_desc_control(0);
    let mut rxdctl = io.read_register(reg)? & 0xffff_c000;
    rxdctl |= 0x1f | (1 << 8) | 0x0100_0000;
    io.write_register(reg, rxdctl)?;
    io.write_register(E1000_RCTL, rctl | E1000_RCTL_EN)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(150);
    io.write_register(E1000_RCTL, rctl & !E1000_RCTL_EN)
}

/// upstream: if_em.c em_flush_desc_rings()
pub fn em_flush_desc_rings<I: E1000RegisterIo, P: E1000PciConfig, O: EmFlushOps>(
    io: &mut I,
    pci: &mut P,
    ops: &mut O,
    tx_ring_bus: u64,
    processed_index: u16,
) -> DevResult {
    let value = io.read_register(E1000_FEXTNVM11)? | 0x0000_2000;
    io.write_register(E1000_FEXTNVM11, value)?;
    let length = io.read_register(tx_desc_length(0))?;
    let status = pci.read_config_u16(0xe4).ok_or(DevError::Io)?;
    if status & 0x100 == 0 || length == 0 {
        return Ok(());
    }
    em_flush_tx_ring(io, ops, tx_ring_bus, processed_index)?;
    let status = pci.read_config_u16(0xe4).ok_or(DevError::Io)?;
    if status & 0x100 != 0 {
        em_flush_rx_ring(io)?;
    }
    Ok(())
}

/// upstream: if_em.c em_prepare_fatal_error_reset()
pub fn em_prepare_fatal_error_reset<I: E1000RegisterIo, O: EmFatalResetOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    error: &mut EmFatalError,
    timeout_ms: usize,
) -> DevResult {
    if !em_has_peind_memory_errors(mac) || error.state != EmFatalState::ResetRequested {
        return Ok(());
    }
    let pcie = error.pcie | (io.read_register(E1000_PCIEERRSTS)? & em_pcie_fatal_error_mask(mac));
    let pcie_parity = error.peind & E1000_PEIND_PCIE_PARITY_FATAL != 0;
    if !em_has_82580_memory_errors(mac) && !pcie_parity && pcie == 0 {
        return Ok(());
    }
    let ctrl = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    io.delay_us(3000);
    let mut complete = false;
    for _ in 0..timeout_ms {
        if io.read_register(E1000_EECD)? & E1000_EECD_AUTO_RD != 0
            && (em_has_82580_memory_errors(mac)
                || io.read_register(E1000_STATUS)? & E1000_STATUS_RST_DONE != 0)
        {
            complete = true;
            break;
        }
        io.delay_us(1000)
    }
    let _ = complete;
    let _ = ops.disable_pcie_master();
    let mut pcie = pcie | io.read_register(E1000_PCIEERRSTS)? & em_pcie_fatal_error_mask(mac);
    if pcie != 0 {
        io.write_register(E1000_PCIEERRSTS, pcie)?;
    }
    if em_has_82580_memory_errors(mac) {
        pcie = error.pcie_ecc
            | (io.read_register(E1000_PCIEECCSTS)? & E1000_PCIEECCSTS_82580_ERROR_MASK);
        if pcie != 0 {
            io.write_register(E1000_PCIEECCSTS, pcie)?;
        }
    }
    error.state = EmFatalState::ResetPrepared;
    Ok(())
}

/// upstream: if_em.c em_intr()
pub fn em_intr<I: E1000RegisterIo>(
    io: &mut I,
    config: EmIrqConfig,
    device_reset: &mut EmDeviceReset,
    fatal: &mut EmFatalError,
    device_id: u16,
) -> DevResult<EmIrqActions> {
    let cause = io.read_register(E1000_ICR)?;
    if cause == u32::MAX || cause == 0 {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Stray),
            ..EmIrqActions::default()
        });
    }
    if config.mac >= E1000MacType::I82571 && cause & 0x8000_0000 == 0 {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Stray),
            ..EmIrqActions::default()
        });
    }
    if igb_handle_device_reset(config.mac, config.vf, cause, device_reset) {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Handled),
            ..EmIrqActions::default()
        });
    }
    if igb_device_reset_pending(config.mac, device_reset.state, config.vf) {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Handled),
            ..EmIrqActions::default()
        });
    }
    let disable = config.vf || config.mac < E1000MacType::I82575;
    if disable {
        io.write_register(E1000_IMC, u32::MAX)?;
    }
    let link = cause & (0x0000_0008 | 0x0000_0004) != 0;
    let fatal_error_captured = em_handle_fatal_error_intr(io, config.mac, device_id, cause, fatal)?;
    Ok(EmIrqActions {
        disposition: Some(EmIrqDisposition::ScheduleThread),
        link_admin: link,
        fatal_error_captured,
        overrun: cause & 0x40 != 0,
        interrupt_disabled: disable,
    })
}

/// upstream: if_em.c em_handle_link()
pub fn em_handle_link(get_link_status: &mut bool) {
    *get_link_status = true;
}

/// upstream: if_em.c em_if_rx_queue_intr_enable()
pub fn em_if_rx_queue_intr_enable<I: E1000RegisterIo>(io: &mut I, eims: u32) -> DevResult {
    io.write_register(E1000_IMS, eims)
}
/// upstream: if_em.c em_if_tx_queue_intr_enable()
pub fn em_if_tx_queue_intr_enable<I: E1000RegisterIo>(io: &mut I, eims: u32) -> DevResult {
    io.write_register(E1000_IMS, eims)
}
/// upstream: if_em.c igb_if_rx_queue_intr_enable()
pub fn igb_if_rx_queue_intr_enable<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    state: EmDeviceResetState,
    vf: bool,
    eims: u32,
) -> DevResult {
    if igb_device_reset_pending(mac, state, vf) {
        return Ok(());
    }
    io.write_register(E1000_EIMS, eims)
}
/// upstream: if_em.c igb_if_tx_queue_intr_enable()
pub fn igb_if_tx_queue_intr_enable<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    state: EmDeviceResetState,
    vf: bool,
    eims: u32,
) -> DevResult {
    igb_if_rx_queue_intr_enable(io, mac, state, vf, eims)
}

/// upstream: if_em.c em_msix_que()
pub fn em_msix_que<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    state: EmDeviceResetState,
    vf: bool,
    config: AimConfig,
    vector: u16,
    queue_itr: &mut u32,
    rx_delta: (u32, u32),
    tx_deltas: &[(u16, u32, u32)],
) -> DevResult<EmIrqDisposition> {
    if igb_device_reset_pending(mac, state, vf) {
        return Ok(EmIrqDisposition::Handled);
    }
    let _ = em_newitr(io, config, vector, queue_itr, rx_delta, tx_deltas)?;
    Ok(EmIrqDisposition::ScheduleThread)
}

/// upstream: if_em.c em_msix_link()
pub fn em_msix_link<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    vf: bool,
    reset: &mut EmDeviceReset,
    fatal: &mut EmFatalError,
    device_id: u16,
    link_mask: u32,
    ims_mask: u32,
) -> DevResult<EmIrqActions> {
    if vf {
        io.write_register(E1000_EIMS, link_mask)?;
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Handled),
            link_admin: true,
            ..EmIrqActions::default()
        });
    }
    let cause = io.read_register(E1000_ICR)?;
    if igb_device_reset_pending(mac, reset.state, false) {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Handled),
            ..EmIrqActions::default()
        });
    }
    if cause != u32::MAX && igb_handle_device_reset(mac, false, cause, reset) {
        return Ok(EmIrqActions {
            disposition: Some(EmIrqDisposition::Handled),
            ..EmIrqActions::default()
        });
    }
    let fatal = if cause == u32::MAX {
        false
    } else {
        em_handle_fatal_error_intr(io, mac, device_id, cause, fatal)?
    };
    if mac >= E1000MacType::I82575 {
        io.write_register(E1000_IMS, ims_mask)?;
        io.write_register(E1000_EIMS, link_mask)?;
    } else if mac == E1000MacType::I82574 {
        io.write_register(E1000_IMS, ims_mask)?;
        if cause != 0 && cause != u32::MAX {
            io.write_register(E1000_ICS, ims_mask)?;
        }
    } else {
        io.write_register(E1000_IMS, ims_mask)?;
    }
    Ok(EmIrqActions {
        disposition: Some(EmIrqDisposition::Handled),
        link_admin: cause != u32::MAX && cause & (0x8 | 0x4) != 0,
        fatal_error_captured: fatal,
        overrun: cause != u32::MAX && cause & 0x40 != 0,
        interrupt_disabled: false,
    })
}

/// upstream: if_em.c em_if_intr_enable()
pub fn em_if_intr_enable<I: E1000RegisterIo>(
    io: &mut I,
    msix: bool,
    queue_mask: u32,
    fatal_mask: u32,
) -> DevResult {
    if msix {
        io.write_register(0x000dc, queue_mask)?;
    }
    io.write_register(
        E1000_IMS,
        0x0000009d | fatal_mask | if msix { queue_mask } else { 0 },
    )?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}
/// upstream: if_em.c em_if_intr_disable()
pub fn em_if_intr_disable<I: E1000RegisterIo>(io: &mut I, msix: bool) -> DevResult {
    if msix {
        io.write_register(0x000dc, 0)?;
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_initialize_rss_mapping()
pub fn em_initialize_rss_mapping<I: E1000RegisterIo>(
    io: &mut I,
    rss_key: &[u8; 40],
    rx_queues: u16,
) -> DevResult {
    if rx_queues == 0 {
        return Err(DevError::InvalidParam);
    }
    for index in 0..10 {
        let offset = index * 4;
        let word = u32::from_le_bytes([
            rss_key[offset],
            rss_key[offset + 1],
            rss_key[offset + 2],
            rss_key[offset + 3],
        ]);
        io.write_register(0x05c80 + index as u32 * 4, word)?;
    }
    let mut reta = 0u32;
    for index in 0..4 {
        let queue = (index % u32::from(rx_queues)) << 7;
        reta |= queue << (index * 8);
    }
    for index in 0..32 {
        io.write_register(0x05c00 + index * 4, reta)?;
    }
    io.write_register(
        E1000_MRQC,
        E1000_MRQC_RSS_ENABLE_2Q
            | E1000_MRQC_RSS_FIELD_IPV4_TCP
            | E1000_MRQC_RSS_FIELD_IPV4
            | E1000_MRQC_RSS_FIELD_IPV6_TCP_EX
            | E1000_MRQC_RSS_FIELD_IPV6_EX
            | E1000_MRQC_RSS_FIELD_IPV6,
    )
}

/// upstream: if_em.c em_integrated_jumbo_rx()
pub const fn em_integrated_jumbo_rx(mac: E1000MacType) -> bool {
    matches!(
        mac,
        E1000MacType::Ich9Lan
            | E1000MacType::Ich10Lan
            | E1000MacType::PchLan
            | E1000MacType::Pch2Lan
            | E1000MacType::PchLpt
            | E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    )
}

/// upstream: if_em.c em_legacy_txdctl()
pub const fn em_legacy_txdctl(mac: E1000MacType) -> u32 {
    let mut value = 31 | (1 << 8) | (1 << 16) | E1000_TXDCTL_GRAN;
    match mac {
        E1000MacType::I82571
        | E1000MacType::I82572
        | E1000MacType::I82573
        | E1000MacType::I82574
        | E1000MacType::I82583
        | E1000MacType::I80003Es2lan => value |= E1000_TXDCTL_COUNT_DESC,
        E1000MacType::Ich8Lan
        | E1000MacType::Ich9Lan
        | E1000MacType::Ich10Lan
        | E1000MacType::PchLan
        | E1000MacType::Pch2Lan
        | E1000MacType::PchLpt
        | E1000MacType::PchSpt
        | E1000MacType::PchCnp
        | E1000MacType::PchTgp
        | E1000MacType::PchAdp
        | E1000MacType::PchMtp
        | E1000MacType::PchPtp
        | E1000MacType::PchNvp => value |= 1 << 22,
        E1000MacType::I82542 | E1000MacType::I82543 | E1000MacType::I82544 => value = 0,
        _ => {}
    }
    value
}

/// upstream: if_em.c igb_txdctl()
pub const fn igb_txdctl(mac: E1000MacType) -> u32 {
    let pthresh = if matches!(mac, E1000MacType::I354) {
        20
    } else {
        8
    };
    pthresh | (1 << 8) | E1000_TXDCTL_QUEUE_ENABLE
}

pub trait EmStopOps {
    fn stop_vf_retry(&mut self);
    fn flush_descriptor_rings(&mut self) -> DevResult;
    fn prepare_iov_reset(&mut self);
    fn prepare_fatal_reset(&mut self) -> DevResult;
    fn reset_mac(&mut self) -> DevResult;
    fn sanitize_vf_queues(&mut self) -> bool;
    fn fence_busmaster(&mut self) -> DevResult;
    fn led_off_cleanup(&mut self);
    fn link_down(&mut self);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmStopContext {
    pub mac: E1000MacType,
    pub vf: bool,
    pub vf_mailbox_ready: bool,
    pub interface_up: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfStopState {
    pub queues_sanitized: bool,
    pub mailbox_ready: bool,
    pub link_speed: u16,
    pub link_duplex: u16,
    pub link_up: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxRingConfig {
    pub queue: u32,
    pub dma_base: u64,
    pub descriptor_count: u32,
    pub descriptor_size: u32,
    pub itr_vector: u16,
    pub csum_flags: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmFlowMode {
    RxPause,
    TxPause,
    Full,
    #[default]
    None,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmFlowState {
    pub high_water: u32,
    pub low_water: u32,
    pub pause_time: u16,
    pub refresh_time: u16,
    pub send_xon: bool,
    pub pba_kb: u32,
    pub mode: EmFlowMode,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmResetConfig {
    pub mac: E1000MacType,
    pub max_frame_size: u32,
    pub mtu: u32,
    pub flow_mode: EmFlowMode,
    pub media_reset: bool,
    pub smart_power_down: bool,
}
pub trait EmResetOps {
    fn get_hw_control(&mut self) -> DevResult;
    fn disable_smart_power_down(&mut self) -> DevResult;
    fn adjust_rxpbs_82580(&mut self, value: u32) -> u32;
    fn configure_flow_control(&mut self, flow: EmFlowState) -> DevResult;
    fn flush_descriptor_rings(&mut self) -> DevResult;
    fn prepare_fatal_error_reset(&mut self) -> DevResult;
    fn reset_hw(&mut self) -> DevResult;
    fn disable_aspm(&mut self) -> DevResult;
    fn setup_init_functions(&mut self) -> DevResult;
    fn init_hw(&mut self) -> DevResult;
    fn configure_82576_memory_errors(&mut self) -> DevResult;
    fn finish_fatal_error_reset(&mut self) -> DevResult;
    fn init_dmac(&mut self, pba: u32) -> DevResult;
    fn get_phy_info(&mut self) -> DevResult;
    fn check_for_link(&mut self) -> DevResult;
}

fn em_reset_pba<I: E1000RegisterIo, O: EmResetOps>(
    io: &mut I,
    ops: &mut O,
    config: EmResetConfig,
) -> DevResult<(u32, EmFlowState)> {
    let mac = config.mac;
    let frame = config.max_frame_size;
    let mut pba = match mac {
        E1000MacType::I82547 | E1000MacType::I82547Rev2 => {
            if frame > 8192 {
                22
            } else {
                30
            }
        }
        E1000MacType::I82571 | E1000MacType::I82572 | E1000MacType::I80003Es2lan => 32,
        E1000MacType::I82573 => 12,
        E1000MacType::I82574 | E1000MacType::I82583 => {
            if frame > 8192 {
                22
            } else {
                32
            }
        }
        E1000MacType::Ich8Lan => 8,
        E1000MacType::Ich9Lan | E1000MacType::Ich10Lan => {
            if frame > 4096 {
                14
            } else {
                10
            }
        }
        E1000MacType::PchLan
        | E1000MacType::Pch2Lan
        | E1000MacType::PchLpt
        | E1000MacType::PchSpt
        | E1000MacType::PchCnp
        | E1000MacType::PchTgp
        | E1000MacType::PchAdp
        | E1000MacType::PchMtp
        | E1000MacType::PchPtp
        | E1000MacType::PchNvp => 26,
        E1000MacType::I82575 => 32,
        E1000MacType::I82576 => io.read_register(E1000_RXPBS)? & 0xffff,
        E1000MacType::I82580 | E1000MacType::I350 | E1000MacType::I354 => {
            ops.adjust_rxpbs_82580(io.read_register(E1000_RXPBS)?)
        }
        E1000MacType::I210 | E1000MacType::I211 => 34,
        _ => {
            if frame > 8192 {
                40
            } else {
                48
            }
        }
    };
    if mac == E1000MacType::I82575 && config.mtu > 1500 {
        let current = io.read_register(E1000_PBA)?;
        let tx_space = current >> 16;
        let mut rx_space = current & 0xffff;
        let min_tx = (((frame + 12) * 2 + 1023) & !1023) / 1024;
        let min_rx = ((frame + 1023) & !1023) / 1024;
        if tx_space < min_tx && min_tx - tx_space < rx_space {
            rx_space -= min_tx - tx_space;
            if rx_space < min_rx {
                rx_space = min_rx;
            }
        }
        pba = rx_space;
        io.write_register(E1000_PBA, pba)?;
    }
    if mac < E1000MacType::I82575 {
        io.write_register(E1000_PBA, pba)?;
    }
    let mut flow = EmFlowState {
        pba_kb: pba,
        high_water: (pba & 0xffff) * 1024 - ((frame + 1023) & !1023),
        low_water: (pba & 0xffff) * 1024 - ((frame + 1023) & !1023) - 1500,
        pause_time: if mac == E1000MacType::I80003Es2lan {
            0xffff
        } else {
            0x0680
        },
        refresh_time: 0,
        send_xon: true,
        mode: config.flow_mode,
    };
    if mac == E1000MacType::PchLan {
        flow.mode = match config.flow_mode {
            EmFlowMode::Full => EmFlowMode::RxPause,
            EmFlowMode::TxPause => EmFlowMode::None,
            mode => mode,
        };
        flow.pause_time = 0xffff;
        if config.mtu > 1500 {
            flow.high_water = 0x3500;
            flow.low_water = 0x1500
        } else {
            flow.high_water = 0x5000;
            flow.low_water = 0x3000
        }
        flow.refresh_time = 0x1000;
    } else if matches!(
        mac,
        E1000MacType::Pch2Lan
            | E1000MacType::PchLpt
            | E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        flow.high_water = 0x5c20;
        flow.low_water = 0x5048;
        flow.pause_time = 0xffff;
        flow.refresh_time = 0xffff;
        pba = if config.mtu > 1500 { 12 } else { 26 };
        flow.pba_kb = pba;
        io.write_register(E1000_PBA, pba)?;
    } else if matches!(mac, E1000MacType::I82575 | E1000MacType::I82576) {
        flow.low_water = flow.high_water - 8;
    } else if matches!(
        mac,
        E1000MacType::I82580
            | E1000MacType::I350
            | E1000MacType::I354
            | E1000MacType::I210
            | E1000MacType::I211
    ) {
        flow.low_water = flow.high_water - 16;
    } else if matches!(mac, E1000MacType::Ich9Lan | E1000MacType::Ich10Lan) && config.mtu > 1500 {
        flow.high_water = 0x2800;
        flow.low_water = flow.high_water - 8;
    } else if mac == E1000MacType::I80003Es2lan {
        flow.pause_time = 0xffff;
    }
    Ok((pba, flow))
}

/// upstream: if_em.c em_reset()
pub fn em_reset<I: E1000RegisterIo, O: EmResetOps>(
    io: &mut I,
    ops: &mut O,
    config: EmResetConfig,
) -> DevResult<u32> {
    ops.get_hw_control()?;
    if !config.smart_power_down && matches!(config.mac, E1000MacType::I82571 | E1000MacType::I82572)
    {
        ops.disable_smart_power_down()?;
    }
    let (pba, flow) = em_reset_pba(io, ops, config)?;
    ops.configure_flow_control(flow)?;
    if config.mac >= E1000MacType::PchSpt && config.mac < E1000MacType::I82575 {
        ops.flush_descriptor_rings()?;
    }
    ops.prepare_fatal_error_reset()?;
    ops.reset_hw()?;
    if config.mac >= E1000MacType::I82575 {
        io.write_register(E1000_WUC, 0)?;
    } else {
        io.write_register(E1000_WUFC, 0)?;
        ops.disable_aspm()?;
    }
    if config.media_reset {
        ops.setup_init_functions()?;
    }
    ops.init_hw()?;
    ops.configure_82576_memory_errors()?;
    ops.finish_fatal_error_reset()?;
    if config.mac >= E1000MacType::I82575 {
        ops.init_dmac(pba)?;
    }
    io.write_register(E1000_VET, 0x8100)?;
    ops.get_phy_info()?;
    ops.check_for_link()?;
    Ok(pba)
}

/// upstream: if_em.c em_initialize_transmit_rings()
pub fn em_initialize_transmit_rings<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    rings: &mut [EmTxRingConfig],
) -> DevResult {
    for ring in rings.iter_mut() {
        ring.csum_flags = 0;
        if mac >= E1000MacType::I82575 {
            let control = tx_desc_control(ring.queue);
            let value = io.read_register(control)?;
            io.write_register(control, value & !0x0200_0000)?;
            let _ = io.read_register(E1000_STATUS)?;
        }
        io.write_register(
            tx_desc_length(ring.queue),
            ring.descriptor_count * ring.descriptor_size,
        )?;
        io.write_register(tx_desc_base_high(ring.queue), (ring.dma_base >> 32) as u32)?;
        io.write_register(tx_desc_base_low(ring.queue), ring.dma_base as u32)?;
        io.write_register(tx_desc_tail(ring.queue), 0)?;
        io.write_register(tx_desc_head(ring.queue), 0)?;
        let control = if mac < E1000MacType::I82575 {
            em_legacy_txdctl(mac)
        } else {
            igb_txdctl(mac)
        };
        io.write_register(tx_desc_control(ring.queue), control)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxUnitConfig {
    pub mac: E1000MacType,
    pub fiber_or_serdes: bool,
    pub tx_delay: u32,
    pub tx_abs_delay: u32,
    pub queue_count: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmRxRingConfig {
    pub queue: u32,
    pub dma_base: u64,
    pub descriptor_count: u32,
    pub descriptor_size: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmRxUnitConfig {
    pub mac: E1000MacType,
    pub mtu: u32,
    pub max_frame_size: u32,
    pub rx_queues: u16,
    pub mbuf_size: u32,
    pub max_interrupt_rate: u32,
    pub rx_delay: u32,
    pub rx_abs_delay: u32,
    pub mc_filter_type: u8,
    pub rx_checksum: bool,
    pub ipv6_checksum: bool,
    pub disable_crc_stripping: bool,
    pub iov: bool,
    pub flow_mode: EmFlowMode,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmVectorQueue {
    pub queue: u32,
    pub vector: u16,
    pub eims: u32,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmVectorMasks {
    pub queue: u32,
    pub link: u32,
}

/// Media selection requested by the interface-control layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmMediaRequest {
    Auto,
    Fiber1000 { lx: bool },
    Copper1000,
    Copper100 { full_duplex: bool },
    Copper10 { full_duplex: bool },
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmMediaConfig {
    pub autoneg: bool,
    pub autoneg_advertised: u32,
    pub forced_speed_duplex: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmMediaType {
    Copper,
    Fiber,
    InternalSerdes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmMediaStatus {
    pub valid: bool,
    pub active: bool,
    pub subtype: Option<EmMediaRequest>,
}

/// upstream: if_em.c em_if_media_change()
pub fn em_if_media_change(request: EmMediaRequest, config: &mut EmMediaConfig) -> DevResult {
    match request {
        EmMediaRequest::Auto => {
            config.autoneg = true;
            config.autoneg_advertised = 0x2f;
        }
        EmMediaRequest::Fiber1000 { .. } | EmMediaRequest::Copper1000 => {
            config.autoneg = true;
            config.autoneg_advertised = 1 << 5;
        }
        EmMediaRequest::Copper100 { full_duplex } => {
            config.autoneg = false;
            config.autoneg_advertised = 0;
            config.forced_speed_duplex = if full_duplex { 0x0008 } else { 0x0004 };
        }
        EmMediaRequest::Copper10 { full_duplex } => {
            config.autoneg = false;
            config.autoneg_advertised = 0;
            config.forced_speed_duplex = if full_duplex { 0x0002 } else { 0x0001 };
        }
        // FreeBSD logs the unrecognized subtype but returns success without
        // changing the selected mode.
        EmMediaRequest::Unsupported => return Ok(()),
    }
    Ok(())
}

/// upstream: if_em.c em_if_media_status()
pub fn em_if_media_status(
    media: EmMediaType,
    mac: E1000MacType,
    link_up: bool,
    speed_mbps: u16,
    full_duplex: bool,
) -> EmMediaStatus {
    let mut status = EmMediaStatus {
        valid: true,
        active: false,
        subtype: None,
    };
    if !link_up {
        return status;
    }
    status.active = true;
    status.subtype = Some(match media {
        EmMediaType::Fiber | EmMediaType::InternalSerdes => EmMediaRequest::Fiber1000 {
            lx: mac == E1000MacType::I82545,
        },
        EmMediaType::Copper => match speed_mbps {
            10 => EmMediaRequest::Copper10 { full_duplex },
            100 => EmMediaRequest::Copper100 { full_duplex },
            1000 => EmMediaRequest::Copper1000,
            _ => return status,
        },
    });
    status
}

/// upstream: if_em.c em_set_flowcntl()
pub fn em_set_flowcntl(mode: u8) -> DevResult<u8> {
    match mode {
        0..=3 => Ok(mode),
        _ => Err(DevError::InvalidParam),
    }
}

pub trait EmWakeLinkOps {
    fn power_up_phy(&mut self) -> DevResult;
    fn power_up_fiber_serdes(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn shutdown_fiber_serdes(&mut self) -> DevResult;
    fn power_down_phy(&mut self) -> DevResult;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmWakeLinkState {
    pub suspend_link_powered_down: bool,
}

/// upstream: if_em.c em_power_up_wakeup_link()
pub fn em_power_up_wakeup_link<O: EmWakeLinkOps>(
    ops: &mut O,
    mac: E1000MacType,
    media: EmMediaType,
    state: &mut EmWakeLinkState,
) -> DevResult {
    if mac < E1000MacType::I82575 || media == EmMediaType::Copper {
        ops.power_up_phy()?;
    } else {
        ops.power_up_fiber_serdes()?;
        ops.setup_link()?;
    }
    state.suspend_link_powered_down = false;
    Ok(())
}

/// upstream: if_em.c em_power_down_wakeup_link()
pub fn em_power_down_wakeup_link<O: EmWakeLinkOps>(
    ops: &mut O,
    mac: E1000MacType,
    media: EmMediaType,
    state: &mut EmWakeLinkState,
) -> DevResult {
    if mac >= E1000MacType::I82575 && media != EmMediaType::Copper {
        ops.shutdown_fiber_serdes()?;
    } else {
        ops.power_down_phy()?;
    }
    state.suspend_link_powered_down = true;
    Ok(())
}

pub trait EmLedOps {
    fn setup_led(&mut self) -> DevResult;
    fn blink_led(&mut self) -> DevResult;
    fn led_on(&mut self) -> DevResult;
    fn led_off(&mut self) -> DevResult;
    fn cleanup_led(&mut self) -> DevResult;
}

/// upstream: if_em.c em_if_led_func()
pub fn em_if_led_func<O: EmLedOps>(ops: &mut O, on: bool, internal_serdes: bool) -> DevResult {
    if on {
        ops.setup_led()?;
        if internal_serdes {
            ops.blink_led()?;
        } else {
            ops.led_on()?;
        }
    } else {
        ops.led_off()?;
        ops.cleanup_led()?;
    }
    Ok(())
}

pub trait EmNvmVectorOps {
    fn read_nvm_word(&mut self, offset: u16) -> u16;
    fn write_nvm_word(&mut self, offset: u16, value: u16);
    fn update_nvm_checksum(&mut self);
}

/// upstream: if_em.c em_enable_vectors_82574()
pub fn em_enable_vectors_82574<O: EmNvmVectorOps>(ops: &mut O) {
    const PCIE_CTRL_WORD: u16 = 0x1b;
    const MSIX_COUNT_MASK: u16 = 0x7 << 7;
    let mut value = ops.read_nvm_word(PCIE_CTRL_WORD);
    if ((value & MSIX_COUNT_MASK) >> 7) != 4 {
        value = (value & !MSIX_COUNT_MASK) | (4 << 7);
        ops.write_nvm_word(PCIE_CTRL_WORD, value);
        ops.update_nvm_checksum();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IgbInterruptConfig {
    pub msix: bool,
    pub reset_pending: bool,
    pub queue_mask: u32,
    pub link_mask: u32,
    pub reset_mask: u32,
    pub iov_mask: u32,
    pub fatal_mask: u32,
    pub legacy_mask: u32,
}

pub trait IgbInterruptOps {
    fn drain_stale_vectors(&mut self) -> DevResult;
    fn prepare_device_reset(&mut self) -> DevResult;
}

/// upstream: if_em.c igb_if_intr_enable()
pub fn igb_if_intr_enable<I: E1000RegisterIo, O: IgbInterruptOps>(
    io: &mut I,
    ops: &mut O,
    config: IgbInterruptConfig,
) -> DevResult {
    if config.reset_pending {
        return Ok(());
    }
    if config.msix {
        let mask = config.queue_mask | config.link_mask;
        let eiac = io.read_register(E1000_EIAC)? | mask;
        io.write_register(E1000_EIAC, eiac)?;
        let eiam = io.read_register(E1000_EIAM)? | mask;
        io.write_register(E1000_EIAM, eiam)?;
        ops.drain_stale_vectors()?;
        io.write_register(E1000_EIMS, mask)?;
        io.write_register(
            E1000_IMS,
            0x0000_0004 | config.reset_mask | config.iov_mask | config.fatal_mask,
        )?;
    } else {
        let mask = config.legacy_mask | config.reset_mask | config.fatal_mask;
        io.write_register(E1000_IAM, mask)?;
        io.write_register(E1000_IMS, mask)?;
    }
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c igb_if_intr_disable()
pub fn igb_if_intr_disable<I: E1000RegisterIo, O: IgbInterruptOps>(
    io: &mut I,
    ops: &mut O,
    config: IgbInterruptConfig,
) -> DevResult {
    ops.prepare_device_reset()?;
    if config.msix {
        let mask = config.queue_mask | config.link_mask;
        let eiam = io.read_register(E1000_EIAM)? & !mask;
        io.write_register(E1000_EIAM, eiam)?;
        io.write_register(E1000_EIMC, mask)?;
        let eiac = io.read_register(E1000_EIAC)? & !mask;
        io.write_register(E1000_EIAC, eiac)?;
    } else {
        io.write_register(E1000_IAM, 0)?;
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmSleepPowerConfig {
    pub mac: E1000MacType,
    pub wake_filters: u32,
    pub suspend_link_powered_down: bool,
    pub phy_i217: bool,
    pub eee_disabled: bool,
    pub eee_low_power_ability: u16,
}

pub trait EmSleepPowerOps {
    fn enable_ulp_lpt_lp(&mut self) -> DevResult;
    fn acquire_phy(&mut self) -> DevResult;
    fn read_lpi_control(&mut self) -> DevResult<u16>;
    fn read_eee_advertisement(&mut self) -> DevResult<u16>;
    fn write_lpi_control(&mut self, value: u16) -> DevResult;
    fn release_phy(&mut self);
}

/// upstream: if_em.c em_configure_sx_low_power()
pub fn em_configure_sx_low_power<O: EmSleepPowerOps>(
    ops: &mut O,
    config: EmSleepPowerConfig,
) -> DevResult {
    if config.mac < E1000MacType::PchLpt
        || config.mac >= E1000MacType::I82575
        || config.suspend_link_powered_down
    {
        return Ok(());
    }
    if config.wake_filters != 0
        && config.wake_filters & (E1000_WUFC_EX | E1000_WUFC_MC | E1000_WUFC_BC) == 0
    {
        ops.enable_ulp_lpt_lp()?;
    }
    if !config.phy_i217 || config.eee_disabled || config.eee_low_power_ability == 0 {
        return Ok(());
    }
    ops.acquire_phy()?;
    let update = (|| {
        let mut lpi = ops.read_lpi_control()?;
        let advertised = ops.read_eee_advertisement()?;
        if advertised & config.eee_low_power_ability & (1 << 1) != 0 {
            lpi |= 0x2000;
        }
        if advertised & config.eee_low_power_ability & (1 << 2) != 0 {
            lpi |= 0x4000;
        }
        ops.write_lpi_control(lpi)
    })();
    ops.release_phy();
    update
}

/// Cached firmware/NVM version captured while the device lock is held.
/// upstream: if_em.c em_fw_version_locked()
pub fn em_fw_version_locked<A: super::nvm::E1000NvmAccess>(
    access: &mut A,
    mac: E1000MacType,
) -> super::nvm::E1000FwVersion {
    if mac >= E1000MacType::I82575 {
        return super::nvm::get_fw_version(access, mac);
    }
    let mut version = super::nvm::E1000FwVersion::default();
    let Some(word) = access
        .read_nvm_words(0x0005, 1)
        .ok()
        .and_then(|words| words.first().copied())
    else {
        return version;
    };
    version.eep_major = (word & 0xf000) >> 12;
    version.eep_minor = (word & 0x0ff0) >> 4;
    version.eep_build = word & 0x000f;
    version
}

pub trait EmRxUnitOps {
    fn initialize_rss(&mut self) -> DevResult;
    fn initialize_advanced_rx_rings(&mut self, drop: bool) -> DevResult;
    fn jumbo_workaround(&mut self, enable: bool) -> DevResult;
}

/// upstream: if_em.c em_initialize_transmit_unit()
pub fn em_initialize_transmit_unit<I: E1000RegisterIo>(
    io: &mut I,
    config: EmTxUnitConfig,
    rings: &mut [EmTxRingConfig],
    txd_command: &mut u32,
) -> DevResult {
    em_initialize_transmit_rings(io, config.mac, rings)?;
    let tipg = if config.mac == E1000MacType::I80003Es2lan {
        8 | (7 << 20)
    } else if config.mac == E1000MacType::I82542 {
        10 | (2 << 10) | (10 << 20)
    } else {
        (if config.fiber_or_serdes { 9 } else { 8 }) | (8 << 10) | (6 << 20)
    };
    if config.mac < E1000MacType::I82575 {
        io.write_register(E1000_TIPG, tipg)?;
        io.write_register(E1000_TIDV, config.tx_delay)?;
        if config.tx_delay > 0 {
            *txd_command |= E1000_TXD_CMD_IDE;
        }
    }
    if config.mac >= E1000MacType::I82540 && config.mac < E1000MacType::I82575 {
        io.write_register(E1000_TADV, config.tx_abs_delay)?;
    }
    if matches!(config.mac, E1000MacType::I82571 | E1000MacType::I82572) {
        let tarc = io.read_register(0x03840)?;
        io.write_register(0x03840, tarc | (1 << 21))?;
    } else if config.mac == E1000MacType::I80003Es2lan {
        for register in [0x03840, 0x03940] {
            let tarc = io.read_register(register)?;
            io.write_register(register, tarc | 1)?;
        }
    } else if config.mac == E1000MacType::I82574 {
        let mut tarc = io.read_register(0x03840)? | (1 << 26);
        if config.queue_count > 1 {
            tarc |= (1 << 7) | (1 << 23) | (1 << 24) | (1 << 25);
            io.write_register(0x03840, tarc)?;
            io.write_register(0x03940, tarc)?;
        } else {
            io.write_register(0x03840, tarc)?;
        }
    }
    let mut tctl = io.read_register(E1000_TCTL)? & !E1000_TCTL_CT;
    tctl |= E1000_TCTL_PSP
        | E1000_TCTL_RTLC
        | E1000_TCTL_EN
        | (E1000_COLLISION_THRESHOLD << E1000_CT_SHIFT);
    if config.mac >= E1000MacType::I82571 && config.mac < E1000MacType::I82575 {
        tctl |= E1000_TCTL_MULR;
    }
    io.write_register(E1000_TCTL, tctl)?;
    if config.mac == E1000MacType::PchSpt {
        let iosf = io.read_register(E1000_IOSFPC)?;
        io.write_register(E1000_IOSFPC, iosf | E1000_RCTL_RDMTS_HEX)?;
        let tarc = io.read_register(0x03840)?;
        io.write_register(0x03840, (tarc & !0x3000_0000) | 0x2000_0000)?;
    }
    Ok(())
}

/// upstream: if_em.c em_if_stop()
pub fn em_if_stop<I: E1000RegisterIo, O: EmStopOps>(
    io: &mut I,
    ops: &mut O,
    context: EmStopContext,
    state: &mut VfStopState,
) -> DevResult {
    if context.vf {
        ops.stop_vf_retry();
    }
    if context.mac >= E1000MacType::PchSpt && context.mac < E1000MacType::I82575 {
        ops.flush_descriptor_rings()?;
    }
    ops.prepare_iov_reset();
    let do_reset = !context.vf || (context.vf_mailbox_ready && !context.interface_up);
    if do_reset {
        ops.prepare_fatal_reset()?;
        if ops.reset_mac().is_err() && !context.vf {
            let _ = ops.fence_busmaster();
            return Err(DevError::Io);
        }
    }
    if context.vf {
        state.queues_sanitized = ops.sanitize_vf_queues();
        state.mailbox_ready = false;
        if !state.queues_sanitized {
            ops.fence_busmaster()?;
        }
    }
    if context.mac >= E1000MacType::I82544 && !context.vf {
        io.write_register(E1000_WUFC, 0)?;
    }
    if context.vf {
        state.link_speed = 0;
        state.link_duplex = 0;
        if state.link_up {
            state.link_up = false;
            ops.link_down();
        }
    } else {
        ops.led_off_cleanup();
    }
    Ok(())
}

/// upstream: if_em.c em_update_stats_counters()
pub fn em_update_stats_counters<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    copper_or_link: bool,
    vf: bool,
    stats: &mut EmHardwareStats,
    ecc: &mut EmErrorStats,
    device_id: u16,
    mut update_vf: impl FnMut() -> DevResult,
) -> DevResult {
    if vf {
        return update_vf();
    }
    let prior_xoff = *stats.counters.get(&E1000_XOFFRXC).unwrap_or(&0);
    if copper_or_link {
        for reg in [E1000_SYMERRS, E1000_SEC] {
            let value = io.read_register(reg)?;
            *stats.counters.entry(reg).or_default() += u64::from(value);
        }
    }
    let counters = [
        E1000_CRCERRS,
        E1000_MPC,
        E1000_SCC,
        E1000_ECOL,
        E1000_MCC,
        E1000_LATECOL,
        E1000_COLC,
        E1000_DC,
        E1000_RLEC,
        E1000_XONRXC,
        E1000_XONTXC,
        E1000_XOFFRXC,
        E1000_XOFFTXC,
        E1000_FCRUC,
        E1000_PRC64,
        E1000_PRC127,
        E1000_PRC255,
        E1000_PRC511,
        E1000_PRC1023,
        E1000_PRC1522,
        E1000_GPRC,
        E1000_BPRC,
        E1000_MPRC,
        E1000_GPTC,
        E1000_RNBC,
        E1000_RUC,
        E1000_RFC,
        E1000_ROC,
        E1000_RJC,
        E1000_MGTPRC,
        E1000_MGTPDC,
        E1000_MGTPTC,
        E1000_TPR,
        E1000_TPT,
        E1000_IAC,
        E1000_ICRXPTC,
        E1000_ICRXATC,
        E1000_ICTXPTC,
        E1000_ICTXATC,
        E1000_ICTXQEC,
        E1000_ICTXQMTC,
        E1000_ICRXDMTC,
        E1000_ICRXOC,
    ];
    for reg in counters {
        let value = io.read_register(reg)?;
        *stats.counters.entry(reg).or_default() += u64::from(value);
    }
    if stats.counters.get(&E1000_XOFFRXC).copied().unwrap_or(0) != prior_xoff {
        stats.pause_frames = true;
    }
    let gorc = u64::from(io.read_register(E1000_GORCL)?)
        | (u64::from(io.read_register(E1000_GORCH)?) << 32);
    *stats.counters.entry(E1000_GORCL).or_default() += gorc;
    let gotc = u64::from(io.read_register(E1000_GOTCL)?)
        | (u64::from(io.read_register(E1000_GOTCH)?) << 32);
    *stats.counters.entry(E1000_GOTCL).or_default() += gotc;
    *stats.counters.entry(E1000_TORH).or_default() += u64::from(io.read_register(E1000_TORH)?);
    *stats.counters.entry(E1000_TOTH).or_default() += u64::from(io.read_register(E1000_TOTH)?);
    if mac >= E1000MacType::I82543 {
        for reg in [
            E1000_ALGNERRC,
            E1000_RXERRC,
            E1000_TNCRS,
            E1000_CEXTERR,
            E1000_TSCTC,
            E1000_TSCTFC,
        ] {
            let value = io.read_register(reg)?;
            *stats.counters.entry(reg).or_default() += u64::from(value);
        }
    }
    if em_has_82571_ecc_stats(mac) {
        em_update_82571_ecc_stats(io, ecc)?;
    } else if em_has_pch_ecc(mac) {
        em_update_pch_ecc_stats(ecc, io.read_register(E1000_PBECCSTS)?);
    } else if em_has_82575_memory_errors(mac) {
        em_update_82575_ecc_stats(
            ecc,
            io.read_register(E1000_PBECCSTS_82575)?,
            io.read_register(E1000_RDHESTS_82575)?,
            io.read_register(E1000_TDHESTS_82575)?,
        );
    } else if em_has_82576_memory_errors(mac) {
        em_update_82576_ecc_stats(io, ecc, em_82576_has_ipsec(device_id))?;
    } else if em_has_82580_memory_errors(mac) {
        let _ = em_update_82580_ecc_stats(
            ecc,
            io.read_register(E1000_RPBECCSTS)?,
            io.read_register(E1000_TPBECCSTS)?,
            io.read_register(E1000_PCIEECCSTS)?,
        );
    } else if em_has_i350_i354_memory_errors(mac) {
        em_update_i350_i354_ecc_stats(io, ecc, mac)?;
    } else if em_has_i210_memory_errors(mac) {
        em_update_i210_ecc_stats(io, ecc)?;
    }
    Ok(())
}

/// upstream: if_em.c igb_disable_dmac()
pub fn igb_disable_dmac<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let reg = io.read_register(E1000_DMACR)?;
    io.write_register(
        E1000_DMACR,
        (reg & !E1000_DMACR_DMAC_EN) | E1000_DMACR_DMAC_LX_MASK,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgbDmacConfig {
    pub mac: E1000MacType,
    pub enabled: bool,
    pub iov: bool,
    pub dmac: u32,
    pub pba_kb: u32,
    pub max_frame_size: u16,
    pub status: u32,
}

/// upstream: if_em.c igb_init_dmac()
pub fn igb_init_dmac<I: E1000RegisterIo>(io: &mut I, config: IgbDmacConfig) -> DevResult {
    if config.mac == E1000MacType::I211 {
        return Ok(());
    }
    if config.iov {
        if config.mac > E1000MacType::I82580 {
            igb_disable_dmac(io)?;
        }
        return Ok(());
    }
    if config.mac > E1000MacType::I82580 {
        if !config.enabled {
            igb_disable_dmac(io)?;
            return Ok(());
        }
        io.write_register(E1000_DMCTXTH, 0)?;
        let mut high = 64 * config.pba_kb - u32::from(config.max_frame_size) / 16;
        if high < 64 * config.pba_kb.saturating_sub(6) {
            high = 64 * config.pba_kb.saturating_sub(6);
        }
        let reg = io.read_register(E1000_FCRTC)? & !E1000_FCRTC_RTH_COAL_MASK;
        io.write_register(
            E1000_FCRTC,
            reg | ((high << E1000_FCRTC_RTH_COAL_SHIFT) & E1000_FCRTC_RTH_COAL_MASK),
        )?;
        let mut threshold = config.pba_kb - u32::from(config.max_frame_size) / 512;
        if threshold < config.pba_kb.saturating_sub(10) {
            threshold = config.pba_kb.saturating_sub(10);
        }
        let mut dmacr = io.read_register(E1000_DMACR)?
            & !(E1000_DMACR_DMACWT_MASK
                | E1000_DMACR_DMACTHR_MASK
                | E1000_DMACR_DMAC_LX_MASK
                | E1000_DMACR_DMAC_EN
                | E1000_DMACR_DC_LPBKW_EN
                | E1000_DMACR_DC_BMC2OSW_EN);
        dmacr |= ((threshold << E1000_DMACR_DMACTHR_SHIFT) & E1000_DMACR_DMACTHR_MASK)
            | E1000_DMACR_DMAC_EN
            | E1000_DMACR_DMAC_LX_MASK;
        let watchdog = if config.mac == E1000MacType::I354
            && config.status & E1000_STATUS_2P5_SKU != 0
            && config.status & E1000_STATUS_2P5_SKU_OVER == 0
        {
            (config.dmac * 5) >> 6
        } else {
            config.dmac >> 5
        };
        dmacr |= watchdog & E1000_DMACR_DMACWT_MASK;
        if matches!(config.mac, E1000MacType::I350 | E1000MacType::I354) {
            dmacr |= E1000_DMACR_DC_LPBKW_EN;
        }
        if config.mac == E1000MacType::I354 {
            dmacr |= E1000_DMACR_DC_BMC2OSW_EN;
        }
        io.write_register(E1000_DMACR, dmacr)?;
        io.write_register(E1000_DMCRTRH, 0)?;
        let mut ctlx = io.read_register(E1000_DMCTLX)? & !E1000_DMCTLX_TTLX_MASK;
        if config.mac == E1000MacType::I350 {
            ctlx |= 0x8000_0000;
        }
        let ticks = if config.mac == E1000MacType::I210 {
            0x20
        } else if config.mac == E1000MacType::I354
            && config.status & E1000_STATUS_2P5_SKU != 0
            && config.status & E1000_STATUS_2P5_SKU_OVER == 0
        {
            0xa
        } else {
            4
        };
        io.write_register(E1000_DMCTLX, ctlx | ticks)?;
        io.write_register(
            E1000_DMCTXTH,
            (20408 - 2 * u32::from(config.max_frame_size)) >> 6,
        )?;
        let misc = io.read_register(E1000_PCIEMISC)?;
        io.write_register(E1000_PCIEMISC, misc | E1000_PCIEMISC_LX_DECISION)?;
    } else if config.mac == E1000MacType::I82580 {
        let misc = io.read_register(E1000_PCIEMISC)?;
        io.write_register(E1000_PCIEMISC, misc & !E1000_PCIEMISC_LX_DECISION)?;
        io.write_register(E1000_DMACR, 0)?;
    }
    Ok(())
}

pub trait LemSmartSpeedOps {
    fn read_phy(&mut self, register: u16) -> DevResult<u16>;
    fn write_phy(&mut self, register: u16, value: u16) -> DevResult;
    fn copper_link_autoneg(&mut self) -> bool;
}
/// upstream: if_em.c lem_smartspeed()
pub fn lem_smartspeed<P: LemSmartSpeedOps>(
    phy: &mut P,
    link_up: bool,
    igp_phy: bool,
    autoneg: bool,
    advertise_gigabit: bool,
    smartspeed: &mut u8,
) -> DevResult {
    if link_up || !igp_phy || !autoneg || !advertise_gigabit {
        return Ok(());
    }
    if *smartspeed == 0 {
        let status = phy.read_phy(0x0a)?;
        if status & 0x8000 == 0 {
            return Ok(());
        }
        let status = phy.read_phy(0x0a)?;
        if status & 0x8000 != 0 {
            let mut control = phy.read_phy(0x09)?;
            if control & 0x1000 != 0 {
                control &= !0x1000;
                phy.write_phy(0x09, control)?;
                *smartspeed += 1;
                if !phy.copper_link_autoneg() {
                    let mut phy_control = phy.read_phy(0)?;
                    phy_control |= 0x1200;
                    phy.write_phy(0, phy_control)?;
                }
            }
        }
        return Ok(());
    }
    if *smartspeed == 3 {
        let mut control = phy.read_phy(0x09)?;
        control |= 0x1000;
        phy.write_phy(0x09, control)?;
        if !phy.copper_link_autoneg() {
            let mut phy_control = phy.read_phy(0)?;
            phy_control |= 0x1200;
            phy.write_phy(0, phy_control)?;
        }
    }
    if *smartspeed == 15 {
        *smartspeed = 0
    } else {
        *smartspeed += 1;
    }
    Ok(())
}

fn read_vf_stat_registers<I: E1000RegisterIo>(io: &mut I, vf_82576: bool) -> DevResult<[u32; 9]> {
    let mut values = [0u32; 9];
    for (i, reg) in [
        E1000_VFGPRC,
        E1000_VFGORC,
        E1000_VFGPTC,
        E1000_VFGOTC,
        if vf_82576 { E1000_VFMPRC } else { 0 },
        E1000_VFGOTLBC,
        E1000_VFGPTLBC,
        E1000_VFGORLBC,
        E1000_VFGPRLBC,
    ]
    .iter()
    .enumerate()
    {
        if *reg != 0 {
            values[i] = io.read_register(*reg)?;
        }
    }
    Ok(values)
}

/// upstream: if_em.c em_initialize_vf_stats()
pub fn em_initialize_vf_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmVfStats,
    vf_82576: bool,
    hyperv: bool,
) -> DevResult {
    *stats = EmVfStats::default();
    em_rebase_vf_stats(io, stats, vf_82576, hyperv)
}

/// upstream: if_em.c em_rebase_vf_stats()
pub fn em_rebase_vf_stats<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmVfStats,
    vf_82576: bool,
    hyperv: bool,
) -> DevResult {
    stats.valid = false;
    if hyperv && io.read_register(E1000_STATUS)? == u32::MAX {
        return Ok(());
    }
    stats.last = read_vf_stat_registers(io, vf_82576)?;
    stats.valid = !hyperv || io.read_register(E1000_STATUS)? != u32::MAX;
    Ok(())
}

/// upstream: if_em.c em_update_vf_stats_counters()
pub fn em_update_vf_stats_counters<I: E1000RegisterIo>(
    io: &mut I,
    stats: &mut EmVfStats,
    vf_82576: bool,
    hyperv: bool,
    mut check_reset: impl FnMut() -> bool,
) -> DevResult {
    if hyperv && io.read_register(E1000_STATUS)? == u32::MAX {
        stats.valid = false;
        return Ok(());
    }
    let mut reset = hyperv && check_reset();
    if hyperv && !stats.valid {
        reset = true
    }
    if hyperv && io.read_register(tx_desc_control(0))? & 0x0200_0000 == 0 {
        reset = true
    }
    let sample = read_vf_stat_registers(io, vf_82576)?;
    let mut next = *stats;
    for index in 0..9 {
        if index == 4 && !vf_82576 {
            continue;
        }
        next.total[index] =
            next.total[index].wrapping_add(u64::from(sample[index].wrapping_sub(next.last[index])));
        next.last[index] = sample[index];
    }
    if hyperv {
        if check_reset() {
            reset = true
        }
        if io.read_register(tx_desc_control(0))? & 0x0200_0000 == 0 {
            reset = true
        }
        if io.read_register(E1000_STATUS)? == u32::MAX {
            stats.valid = false;
            return Ok(());
        }
    }
    if reset {
        em_rebase_vf_stats(io, stats, vf_82576, hyperv)?
    } else {
        next.valid = true;
        *stats = next;
    }
    Ok(())
}

/// upstream: if_em.c em_if_get_vf_counter()
pub fn em_if_get_vf_counter(counter: EmIfCounter, dropped: u64, default: u64) -> u64 {
    if counter == EmIfCounter::InputErrors {
        dropped
    } else {
        default
    }
}

/// upstream: if_em.c em_if_get_counter()
pub fn em_if_get_counter(
    counter: EmIfCounter,
    vf: bool,
    dropped: u64,
    stats: &EmHardwareStats,
    default: u64,
) -> u64 {
    if vf {
        return em_if_get_vf_counter(counter, dropped, default);
    }
    let value = |register: u32| *stats.counters.get(&register).unwrap_or(&0);
    match counter {
        EmIfCounter::Collisions => value(E1000_COLC),
        EmIfCounter::InputErrors => {
            dropped
                + value(E1000_RXERRC)
                + value(E1000_CRCERRS)
                + value(E1000_ALGNERRC)
                + value(E1000_RUC)
                + value(E1000_ROC)
                + value(E1000_MPC)
                + value(E1000_CEXTERR)
        }
        EmIfCounter::OutputErrors => default + value(E1000_ECOL) + value(E1000_LATECOL),
        EmIfCounter::Other => default,
    }
}

/// upstream: if_em.c em_if_needs_restart()
pub const fn em_if_needs_restart(_event: u32) -> bool {
    false
}

/// upstream: if_em.c em_if_vlan_register()
pub fn em_if_vlan_register<V: EmVlanOps>(
    ops: &mut V,
    table: &mut VftaTable,
    vid: u16,
    vf: bool,
    hyperv: bool,
    iov: bool,
) -> DevResult {
    if hyperv {
        return Ok(());
    }
    let index = usize::from((vid >> 5) & 0x7f);
    let mask = 1u32 << (vid & 0x1f);
    let present = table.words[index] & mask != 0;
    table.words[index] |= mask;
    table.stale[index] &= !mask;
    if !present {
        table.count += 1;
    }
    if vf {
        if ops.set_vf_vlan(vid, true).is_err() {
            ops.retry_add(vid)
        } else {
            ops.retry_clear(vid)
        }
    } else if iov {
        ops.rebuild_iov_vlan()
    } else {
        ops.write_vfta(index as u32, table.words[index])?;
    }
    Ok(())
}

/// upstream: if_em.c em_if_vlan_unregister()
pub fn em_if_vlan_unregister<V: EmVlanOps>(
    ops: &mut V,
    table: &mut VftaTable,
    vid: u16,
    vf: bool,
    hyperv: bool,
    iov: bool,
) -> DevResult {
    if hyperv {
        return Ok(());
    }
    let index = usize::from((vid >> 5) & 0x7f);
    let mask = 1u32 << (vid & 0x1f);
    let present = table.words[index] & mask != 0;
    if vf {
        ops.retry_clear(vid)
    }
    if vf && ops.set_vf_vlan(vid, false).is_err() {
        table.stale[index] |= mask;
    } else {
        table.stale[index] &= !mask;
    }
    table.words[index] &= !mask;
    if present {
        table.count = table.count.saturating_sub(1)
    }
    if !vf {
        if iov {
            ops.rebuild_iov_vlan()
        } else {
            ops.write_vfta(index as u32, table.words[index])?;
        }
    }
    Ok(())
}

/// upstream: if_em.c em_if_vlan_filter_capable()
pub const fn em_if_vlan_filter_capable(
    vlan_hwfilter_enabled: bool,
    disable_crc_stripping: bool,
) -> bool {
    vlan_hwfilter_enabled && !disable_crc_stripping
}
/// upstream: if_em.c em_if_vlan_filter_used()
pub fn em_if_vlan_filter_used(table: &VftaTable, capable: bool) -> bool {
    capable && table.words.iter().any(|word| *word != 0)
}

/// upstream: if_em.c em_if_vlan_filter_enable()
pub fn em_if_vlan_filter_enable<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut rctl = io.read_register(E1000_RCTL)?;
    rctl &= !E1000_RCTL_CFIEN;
    rctl |= E1000_RCTL_VFE;
    io.write_register(E1000_RCTL, rctl)
}
/// upstream: if_em.c em_if_vlan_filter_disable()
pub fn em_if_vlan_filter_disable<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let rctl = io.read_register(E1000_RCTL)?;
    io.write_register(E1000_RCTL, rctl & !(E1000_RCTL_VFE | E1000_RCTL_CFIEN))
}
/// upstream: if_em.c em_if_vlan_filter_write()
pub fn em_if_vlan_filter_write<V: EmVlanOps>(
    ops: &mut V,
    table: &VftaTable,
    changed_index: usize,
    legacy_interrupts: bool,
    mut disable: impl FnMut() -> DevResult,
    mut enable: impl FnMut() -> DevResult,
) -> DevResult {
    if legacy_interrupts {
        disable()?;
    }
    for (index, value) in table.words.iter().enumerate() {
        if *value != 0 || index == changed_index {
            ops.write_vfta(index as u32, *value)?;
        }
    }
    if legacy_interrupts {
        enable()?;
    }
    Ok(())
}

/// upstream: if_em.c em_setup_vlan_hw_support()
pub fn em_setup_vlan_hw_support<I: E1000RegisterIo, V: EmVlanOps>(
    io: &mut I,
    ops: &mut V,
    table: &mut VftaTable,
    mac_tagging: bool,
    crc_stripping_disabled: bool,
    vf: bool,
    hyperv: bool,
    iov: bool,
    max_frame_size: u32,
    vlan_tagging_enabled: bool,
) -> DevResult {
    if hyperv {
        return Ok(());
    }
    if vf {
        ops.write_rlpml((max_frame_size + 4).min(0x2600))?;
        for vid in 0..4096 {
            let mask = 1u32 << (vid & 0x1f);
            if table.words[(vid >> 5) as usize] & mask != 0 {
                if ops.set_vf_vlan(vid as u16, true).is_err() {
                    ops.retry_add(vid as u16)
                } else {
                    ops.retry_clear(vid as u16)
                }
            }
        }
        return Ok(());
    }
    let mut ctrl = io.read_register(E1000_CTRL)?;
    if vlan_tagging_enabled && !crc_stripping_disabled {
        ctrl |= E1000_CTRL_VME
    } else {
        ctrl &= !E1000_CTRL_VME
    }
    io.write_register(E1000_CTRL, ctrl)?;
    if !em_if_vlan_filter_capable(mac_tagging, crc_stripping_disabled) {
        if iov {
            ops.set_pf_vlan_promisc(true);
            em_if_vlan_filter_enable(io)?;
        } else {
            em_if_vlan_filter_disable(io)?;
        }
        return Ok(());
    }
    if iov {
        ops.set_pf_vlan_promisc(false)
    }
    em_if_vlan_register(ops, table, 0, false, false, iov)?;
    em_if_vlan_filter_enable(io)
}

/// upstream: if_em.c em_if_defer_promisc()
pub const fn em_if_defer_promisc(mac: E1000MacType) -> bool {
    matches!(
        mac,
        E1000MacType::I82576
            | E1000MacType::I350
            | E1000MacType::VfAdapt
            | E1000MacType::VfAdaptI350
    )
}

/// upstream: if_em.c em_if_set_promisc()
pub fn em_if_set_promisc(state: &mut EmPromiscState, mac: E1000MacType) -> bool {
    if em_if_defer_promisc(mac) {
        state.flags_pending = true;
        true
    } else {
        false
    }
}

/// upstream: if_em.c em_if_set_promisc_impl()
pub fn em_if_set_promisc_impl<I: E1000RegisterIo, V: EmVlanOps, M: EmMulticastOps>(
    io: &mut I,
    vlan: &mut V,
    ops: &mut M,
    table: &VftaTable,
    state: EmPromiscState,
    disable_crc_stripping: bool,
) -> DevResult {
    if state.vf {
        return ops.set_vf_promisc(state.promisc, state.allmulti);
    }
    let mut rctl = io.read_register(E1000_RCTL)?;
    rctl &= !(E1000_RCTL_SBP | E1000_RCTL_UPE);
    let mcnt = if state.allmulti {
        128
    } else {
        state.num_multicast
    };
    if mcnt < 128 {
        rctl &= !E1000_RCTL_MPE;
    }
    io.write_register(E1000_RCTL, rctl)?;
    if state.promisc {
        rctl |= E1000_RCTL_UPE | E1000_RCTL_MPE;
        io.write_register(E1000_RCTL, rctl)?;
        if state.iov {
            em_if_vlan_filter_enable(io)?;
        } else {
            em_if_vlan_filter_disable(io)?;
        }
    } else {
        if state.allmulti {
            rctl |= E1000_RCTL_MPE;
            rctl &= !E1000_RCTL_UPE;
            io.write_register(E1000_RCTL, rctl)?;
        }
        if state.iov
            || em_if_vlan_filter_used(
                table,
                em_if_vlan_filter_capable(true, disable_crc_stripping),
            )
        {
            em_if_vlan_filter_enable(io)?;
        }
    }
    ops.update_iov_vmolr()?;
    ops.rebuild_iov_vlan()?;
    let _ = vlan;
    Ok(())
}

/// upstream: if_em.c em_copy_maddr()
pub fn em_copy_maddr(destination: &mut [u8], address: [u8; 6], index: usize) -> u32 {
    if index >= 128 || destination.len() < index * 6 + 6 {
        return 0;
    }
    destination[index * 6..index * 6 + 6].copy_from_slice(&address);
    1
}

/// upstream: if_em.c em_fill_wakeup_mta()
pub fn em_fill_wakeup_mta<I: E1000RegisterIo>(io: &mut I, mta_registers: u16) -> DevResult {
    for index in (0..mta_registers).rev() {
        io.write_register(E1000_MTA + u32::from(index) * 4, u32::MAX)?;
    }
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: if_em.c em_if_multi_set()
pub fn em_if_multi_set<I: E1000RegisterIo, V: EmMulticastOps>(
    io: &mut I,
    ops: &mut V,
    mac: E1000MacType,
    revision: u8,
    addresses: &[[u8; 6]],
    vf: bool,
    iov: bool,
    promisc: bool,
    allmulti: bool,
    pci_mwi: bool,
) -> DevResult {
    let reset = mac == E1000MacType::I82542 && revision == 2;
    if reset {
        let rctl = io.read_register(E1000_RCTL)?;
        if pci_mwi {
            ops.clear_pci_mwi()?;
        }
        io.write_register(E1000_RCTL, rctl | E1000_RCTL_RST)?;
        io.delay_us(5000);
    }
    let count = addresses.len().min(128);
    let list = &addresses[..count];
    if vf {
        ops.update_multicast(list)?;
        ops.update_vf_unicast()?;
        return Ok(());
    }
    if count < 128 && !iov {
        ops.update_multicast(list)?;
    }
    let mut rctl = io.read_register(E1000_RCTL)?;
    if promisc {
        rctl |= E1000_RCTL_UPE | E1000_RCTL_MPE;
    } else if count >= 128 || allmulti {
        rctl |= E1000_RCTL_MPE;
        rctl &= !E1000_RCTL_UPE;
    } else {
        rctl &= !(E1000_RCTL_UPE | E1000_RCTL_MPE);
    }
    io.write_register(E1000_RCTL, rctl)?;
    if reset {
        rctl = io.read_register(E1000_RCTL)?;
        io.write_register(E1000_RCTL, rctl & !E1000_RCTL_RST)?;
        io.delay_us(5000);
        if pci_mwi {
            ops.set_pci_mwi()?;
        }
    }
    ops.rebuild_iov_mta()?;
    ops.update_iov_vmolr()
}

/// upstream: if_em.c em_get_hw_control()
pub fn em_get_hw_control<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    is_vf: bool,
) -> DevResult {
    if is_vf {
        return Ok(());
    }
    if mac == E1000MacType::I82573 {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm | E1000_SWSM_DRV_LOAD)
    } else {
        let ctrl = io.read_register(E1000_CTRL_EXT)?;
        io.write_register(E1000_CTRL_EXT, ctrl | E1000_CTRL_EXT_DRV_LOAD)
    }
}

/// upstream: if_em.c em_release_hw_control()
pub fn em_release_hw_control<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    has_manage: bool,
) -> DevResult {
    if !has_manage {
        return Ok(());
    }
    if mac == E1000MacType::I82573 {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm & !E1000_SWSM_DRV_LOAD)
    } else {
        let ctrl = io.read_register(E1000_CTRL_EXT)?;
        io.write_register(E1000_CTRL_EXT, ctrl & !E1000_CTRL_EXT_DRV_LOAD)
    }
}

/// upstream: if_em.c em_init_manageability()
pub fn em_init_manageability<I: E1000RegisterIo>(io: &mut I, has_manage: bool) -> DevResult {
    if !has_manage {
        return Ok(());
    }
    let manc2h = io.read_register(E1000_MANC2H)?;
    let manc = io.read_register(E1000_MANC)?;
    io.write_register(
        E1000_MANC2H,
        manc2h | E1000_MANC2H_PORT_623 | E1000_MANC2H_PORT_664,
    )?;
    io.write_register(
        E1000_MANC,
        (manc & !E1000_MANC_ARP_EN) | E1000_MANC_EN_MNG2HOST,
    )
}

/// upstream: if_em.c em_release_manageability()
pub fn em_release_manageability<I: E1000RegisterIo>(io: &mut I, has_manage: bool) -> DevResult {
    if !has_manage {
        return Ok(());
    }
    let manc = io.read_register(E1000_MANC)?;
    io.write_register(
        E1000_MANC,
        (manc | E1000_MANC_ARP_EN) & !E1000_MANC_EN_MNG2HOST,
    )
}

/// upstream: if_em.c em_disable_aspm()
pub fn em_disable_aspm<P: E1000PciConfig>(pci: &mut P, mac: E1000MacType) -> DevResult {
    if !matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) {
        return Ok(());
    }
    let base = match pci.find_capability(0x10) {
        Some(value) => value,
        None => return Ok(()),
    };
    let cap = pci.read_config_u16(base + 0x0c).ok_or(DevError::Io)?;
    if cap & 0x0c00 == 0 {
        return Ok(());
    }
    let link = pci.read_config_u16(base + 0x10).ok_or(DevError::Io)?;
    pci.write_config_u16(base + 0x10, link & !0x0003)
        .then_some(())
        .ok_or(DevError::Io)
}

/// upstream: if_em.c em_initialize_receive_unit()
pub fn em_initialize_receive_unit<I: E1000RegisterIo, O: EmRxUnitOps>(
    io: &mut I,
    ops: &mut O,
    config: EmRxUnitConfig,
    rings: &[EmRxRingConfig],
) -> DevResult {
    let mut rctl = io.read_register(E1000_RCTL)?;
    if !matches!(config.mac, E1000MacType::I82574 | E1000MacType::I82583) {
        io.write_register(E1000_RCTL, rctl & !E1000_RCTL_EN)?;
    }
    rctl &= !(3 << E1000_RCTL_MO_SHIFT);
    rctl |= E1000_RCTL_EN
        | E1000_RCTL_BAM
        | E1000_RCTL_LBM_NO
        | E1000_RCTL_RDMTS_HALF
        | (u32::from(config.mc_filter_type) << E1000_RCTL_MO_SHIFT);
    rctl &= !E1000_RCTL_SBP;
    if config.iov || config.mtu > 1500 {
        rctl |= E1000_RCTL_LPE
    } else {
        rctl &= !E1000_RCTL_LPE;
    }
    if !config.disable_crc_stripping {
        rctl |= E1000_RCTL_SECRC;
    }
    if config.mac < E1000MacType::I82575 {
        if config.mac >= E1000MacType::I82540 {
            io.write_register(E1000_RADV, config.rx_abs_delay)?;
            io.write_register(
                E1000_ITR,
                (ITR_RATE_DIVIDEND / (u64::from(config.max_interrupt_rate) * ITR_RATE_MULTIPLIER))
                    as u32,
            )?;
        }
        let rdtr = if config.mac == E1000MacType::I82573 {
            0x20
        } else {
            config.rx_delay
        };
        io.write_register(E1000_RDTR, rdtr)?;
    }
    if config.mac >= E1000MacType::I82540 {
        let mut rfctl = io.read_register(E1000_RFCTL)? | E1000_RFCTL_EXTEN;
        if config.mac == E1000MacType::I82574 {
            for q in 0..4 {
                io.write_register(
                    0x000e8 + q * 4,
                    (ITR_RATE_DIVIDEND
                        / (u64::from(config.max_interrupt_rate) * ITR_RATE_MULTIPLIER))
                        as u32,
                )?;
            }
            rfctl |= E1000_RFCTL_ACK_DIS;
        }
        io.write_register(E1000_RFCTL, rfctl)?;
    }
    let mut rxcsum = io.read_register(E1000_RXCSUM)?;
    if config.rx_checksum {
        rxcsum |= E1000_RXCSUM_TUOFL | E1000_RXCSUM_IPOFL;
        if config.mac > E1000MacType::I82575 {
            rxcsum |= E1000_RXCSUM_CRCOFL;
        } else if config.mac < E1000MacType::I82540 && config.ipv6_checksum {
            rxcsum |= E1000_RXCSUM_IPV6OFL;
        }
    } else {
        rxcsum &= !(E1000_RXCSUM_IPOFL | E1000_RXCSUM_TUOFL);
        if config.mac > E1000MacType::I82575 {
            rxcsum &= !E1000_RXCSUM_CRCOFL;
        } else if config.mac < E1000MacType::I82540 {
            rxcsum &= !E1000_RXCSUM_IPV6OFL;
        }
    }
    if config.rx_queues > 1 {
        rxcsum |= E1000_RXCSUM_PCSD;
        ops.initialize_rss()?;
    }
    io.write_register(E1000_RXCSUM, rxcsum)?;
    if config.mac < E1000MacType::I82575 {
        for ring in rings.iter().take(usize::from(config.rx_queues)) {
            io.write_register(
                rx_desc_length(ring.queue),
                ring.descriptor_count * ring.descriptor_size,
            )?;
            io.write_register(rx_desc_base_high(ring.queue), (ring.dma_base >> 32) as u32)?;
            io.write_register(rx_desc_base_low(ring.queue), ring.dma_base as u32)?;
            io.write_register(rx_desc_head(ring.queue), 0)?;
            io.write_register(rx_desc_tail(ring.queue), 0)?;
        }
    }
    if em_integrated_jumbo_rx(config.mac) && config.mtu > 1500 {
        let mut value = io.read_register(rx_desc_control(0))?;
        value = (value & !0x00003f3f) | 3 | (1 << 8);
        io.write_register(rx_desc_control(0), value)?;
    } else if config.mac == E1000MacType::I82574 {
        for queue in 0..config.rx_queues {
            let reg = rx_desc_control(u32::from(queue));
            let value = io.read_register(reg)?;
            io.write_register(
                reg,
                (value & !0x003f_3f3f) | 32 | (4 << 8) | (4 << 16) | 0x0100_0000,
            )?;
        }
    } else if config.mac >= E1000MacType::I82575 {
        if config.iov {
            io.write_register(E1000_RLPML, 0x2600)?;
        } else if config.mtu > 1500 {
            io.write_register(E1000_RLPML, config.max_frame_size)?;
        }
        let drop = config.iov
            || (config.rx_queues > 1
                && matches!(config.flow_mode, EmFlowMode::None | EmFlowMode::RxPause));
        ops.initialize_advanced_rx_rings(drop)?;
    } else if config.mac >= E1000MacType::Pch2Lan {
        ops.jumbo_workaround(config.mtu > 1500)?;
    }
    rctl &= !E1000_RCTL_VFE;
    if config.mac < E1000MacType::I82575 {
        if config.mbuf_size > 2048 && config.mbuf_size <= 4096 {
            rctl |= E1000_RCTL_SZ_4096 | E1000_RCTL_BSEX;
        } else if config.mbuf_size > 4096 && config.mbuf_size <= 8192 {
            rctl |= E1000_RCTL_SZ_8192 | E1000_RCTL_BSEX;
        } else if config.mbuf_size > 8192 {
            rctl |= E1000_RCTL_SZ_16384 | E1000_RCTL_BSEX;
        } else {
            rctl |= E1000_RCTL_SZ_2048;
            rctl &= !E1000_RCTL_BSEX;
        }
    } else {
        rctl |= E1000_RCTL_SZ_2048;
    }
    rctl &= !0x0000_0c00;
    io.write_register(E1000_RCTL, rctl)
}

/// upstream: if_em.c igb_rxdctl()
pub const fn igb_rxdctl(mac: E1000MacType, msix: bool, current: u32) -> u32 {
    let (mut mask, mut pthresh, mut wthresh) = (0x001f_1f1f, 8u32, 4u32);
    match mac {
        E1000MacType::I82575 => {
            mask = 0x003f_3f3f;
        }
        E1000MacType::I82576 => {
            wthresh = if msix { 1 } else { 4 };
        }
        E1000MacType::VfAdapt => {
            wthresh = 1;
        }
        E1000MacType::I354 => {
            pthresh = 12;
        }
        _ => {}
    }
    (current & !mask) | pthresh | (8 << 8) | (wthresh << 16) | 0x0200_0000
}

/// upstream: if_em.c igb_initialize_rss_mapping()
pub fn igb_initialize_rss_mapping<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    rx_queues: u16,
    rss_key: &[u32; 10],
) -> DevResult {
    if rx_queues == 0 {
        return Err(DevError::InvalidParam);
    }
    let shift = if mac == E1000MacType::I82575 { 6 } else { 0 };
    let mut reta = 0u32;
    for i in 0..128u32 {
        let queue = (i % u32::from(rx_queues)) << shift;
        reta = (reta >> 8) | (queue << 24);
        if i & 3 == 3 {
            io.write_register(0x05c00 + (i >> 2) * 4, reta)?;
            reta = 0;
        }
    }
    for (i, value) in rss_key.iter().enumerate() {
        io.write_register(0x05c80 + i as u32 * 4, *value)?;
    }
    io.write_register(
        E1000_MRQC,
        0x0000_0002
            | E1000_MRQC_RSS_FIELD_IPV4
            | E1000_MRQC_RSS_FIELD_IPV4_TCP
            | E1000_MRQC_RSS_FIELD_IPV6
            | E1000_MRQC_RSS_FIELD_IPV6_TCP
            | 0x0040_0000
            | 0x0080_0000
            | 0x0100_0000
            | E1000_MRQC_RSS_FIELD_IPV6_TCP_EX,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgbRxRingConfig {
    pub queue: u32,
    pub dma_base: u64,
    pub descriptor_count: u32,
    pub descriptor_size: u32,
}

/// upstream: if_em.c igb_initialize_receive_rings()
pub fn igb_initialize_receive_rings<I: E1000RegisterIo>(
    io: &mut I,
    rx_mbuf_size: u32,
    drop: bool,
    mac: E1000MacType,
    msix: bool,
    rings: &[IgbRxRingConfig],
) -> DevResult {
    let mut srrctl = ((rx_mbuf_size + 1023) >> 10) | 0x0200_0000;
    if drop {
        srrctl |= 0x8000_0000;
    }
    for ring in rings {
        let control = rx_desc_control(ring.queue);
        let rxdctl = io.read_register(control)?;
        io.write_register(control, rxdctl & !0x0200_0000)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.write_register(
            rx_desc_length(ring.queue),
            ring.descriptor_count * ring.descriptor_size,
        )?;
        io.write_register(rx_desc_base_high(ring.queue), (ring.dma_base >> 32) as u32)?;
        io.write_register(rx_desc_base_low(ring.queue), ring.dma_base as u32)?;
        io.write_register(rx_desc_head(ring.queue), 0)?;
        io.write_register(rx_desc_tail(ring.queue), 0)?;
        io.write_register(rx_split_control(ring.queue), srrctl)?;
        io.write_register(control, igb_rxdctl(mac, msix, rxdctl))?;
    }
    Ok(())
}

/// upstream: if_em.c igb_initialize_interrupt_rate()
pub fn igb_initialize_interrupt_rate<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    rx_vectors: &[u16],
    link_vector: Option<u16>,
    rate: u32,
) -> DevResult {
    if rate == 0 {
        return Err(DevError::InvalidParam);
    }
    let mut value = ((EITR_RATE_DIVIDEND / rate) << EITR_SHIFT) & EITR_MASK;
    if mac == E1000MacType::I82575 {
        value |= value << 16;
    } else {
        value |= EITR_COUNT_IGNORE;
    }
    for vector in rx_vectors {
        io.write_register(0x01680 + u32::from(*vector) * 4, value)?;
    }
    if let Some(vector) = link_vector {
        io.write_register(0x01680 + u32::from(vector) * 4, value)?;
    }
    Ok(())
}

/// upstream: if_em.c igb_configure_queues()
pub fn igb_configure_queues<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    vf: bool,
    rx: &mut [EmVectorQueue],
    tx: &[EmVectorQueue],
    link_vector: u16,
) -> DevResult<EmVectorMasks> {
    let mut masks = EmVectorMasks::default();
    if !vf && mac != E1000MacType::I82575 {
        io.write_register(
            E1000_GPIE,
            E1000_GPIE_MSIX_MODE | E1000_GPIE_EIAME | E1000_GPIE_PBA | E1000_GPIE_NSICR,
        )?;
    }
    if matches!(
        mac,
        E1000MacType::I82580
            | E1000MacType::I350
            | E1000MacType::I354
            | E1000MacType::I210
            | E1000MacType::I211
            | E1000MacType::VfAdapt
            | E1000MacType::VfAdaptI350
    ) {
        for q in rx.iter_mut() {
            let index = q.queue >> 1;
            let reg = E1000_IVAR0 + index * 4;
            let mut ivar = io.read_register(reg)?;
            if q.queue & 1 != 0 {
                ivar = (ivar & 0xff00_ffff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 16);
            } else {
                ivar = (ivar & 0xffff_ff00) | u32::from(q.vector) | E1000_IVAR_VALID;
            }
            io.write_register(reg, ivar)?;
            masks.queue |= q.eims;
        }
        for q in tx {
            let index = q.queue >> 1;
            let reg = E1000_IVAR0 + index * 4;
            let mut ivar = io.read_register(reg)?;
            if q.queue & 1 != 0 {
                ivar = (ivar & 0x00ff_ffff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 24);
            } else {
                ivar = (ivar & 0xffff_00ff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 8);
            }
            io.write_register(reg, ivar)?;
            masks.queue |= q.eims;
        }
        let ivar = if vf {
            u32::from(link_vector) | E1000_IVAR_VALID
        } else {
            (u32::from(link_vector) | E1000_IVAR_VALID) << 8
        };
        masks.link = 1u32 << link_vector;
        io.write_register(E1000_IVAR_MISC, ivar)?;
    } else if mac == E1000MacType::I82576 {
        for q in rx.iter_mut() {
            let index = q.queue & 7;
            let reg = E1000_IVAR0 + index * 4;
            let mut ivar = io.read_register(reg)?;
            if q.queue < 8 {
                ivar = (ivar & 0xffff_ff00) | u32::from(q.vector) | E1000_IVAR_VALID;
            } else {
                ivar = (ivar & 0xff00_ffff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 16);
            }
            io.write_register(reg, ivar)?;
            masks.queue |= q.eims;
        }
        for q in tx {
            let index = q.queue & 7;
            let reg = E1000_IVAR0 + index * 4;
            let mut ivar = io.read_register(reg)?;
            if q.queue < 8 {
                ivar = (ivar & 0xffff_00ff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 8);
            } else {
                ivar = (ivar & 0x00ff_ffff) | ((u32::from(q.vector) | E1000_IVAR_VALID) << 24);
            }
            io.write_register(reg, ivar)?;
            masks.queue |= q.eims;
        }
        masks.link = 1u32 << link_vector;
        io.write_register(
            E1000_IVAR_MISC,
            (u32::from(link_vector) | E1000_IVAR_VALID) << 8,
        )?;
    } else if mac == E1000MacType::I82575 {
        let ctrl = io.read_register(E1000_CTRL_EXT)?
            | E1000_CTRL_EXT_PBA_CLR
            | E1000_CTRL_EXT_EIAME
            | E1000_CTRL_EXT_IRCA;
        io.write_register(E1000_CTRL_EXT, ctrl)?;
        for (i, q) in rx.iter_mut().enumerate() {
            q.eims = (E1000_EICR_RX_QUEUE0 << i) | (E1000_EICR_TX_QUEUE0 << i);
            io.write_register(0x01600 + i as u32 * 4, q.eims)?;
            masks.queue |= q.eims;
        }
        io.write_register(0x01600 + u32::from(link_vector) * 4, 0x8000_0000)?;
        masks.link |= 0x8000_0000;
    }
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct RegisterMock(BTreeMap<u32, u32>);
    impl E1000RegisterIo for RegisterMock {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            Ok(*self.0.get(&register).unwrap_or(&0))
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            self.0.insert(register, value);
            Ok(())
        }
        fn delay_us(&mut self, _micros: u32) {}
        fn invalid_tail_write(&mut self, _direction: &'static str) {}
    }
    #[derive(Default)]
    struct IgbInterruptMock {
        drained: usize,
        reset_prepared: usize,
    }
    impl IgbInterruptOps for IgbInterruptMock {
        fn drain_stale_vectors(&mut self) -> DevResult {
            self.drained += 1;
            Ok(())
        }
        fn prepare_device_reset(&mut self) -> DevResult {
            self.reset_prepared += 1;
            Ok(())
        }
    }
    #[derive(Default)]
    struct PciMock(BTreeMap<u32, u16>);
    impl E1000PciConfig for PciMock {
        fn read_config_u16(&mut self, register: u32) -> Option<u16> {
            self.0.get(&register).copied()
        }
        fn write_config_u16(&mut self, register: u32, value: u16) -> bool {
            self.0.insert(register, value);
            true
        }
        fn find_capability(&mut self, id: u8) -> Option<u32> {
            (id == 0x10).then_some(0x40)
        }
    }
    #[derive(Default)]
    struct VlanMock {
        writes: Vec<(u32, u32)>,
        fail_remove: bool,
    }

    #[derive(Default)]
    struct WakeMock(Vec<&'static str>);
    impl EmWakeLinkOps for WakeMock {
        fn power_up_phy(&mut self) -> DevResult {
            self.0.push("up_phy");
            Ok(())
        }
        fn power_up_fiber_serdes(&mut self) -> DevResult {
            self.0.push("up_fiber");
            Ok(())
        }
        fn setup_link(&mut self) -> DevResult {
            self.0.push("setup");
            Ok(())
        }
        fn shutdown_fiber_serdes(&mut self) -> DevResult {
            self.0.push("down_fiber");
            Ok(())
        }
        fn power_down_phy(&mut self) -> DevResult {
            self.0.push("down_phy");
            Ok(())
        }
    }

    #[derive(Default)]
    struct LedMock(Vec<&'static str>);
    impl EmLedOps for LedMock {
        fn setup_led(&mut self) -> DevResult {
            self.0.push("setup");
            Ok(())
        }
        fn blink_led(&mut self) -> DevResult {
            self.0.push("blink");
            Ok(())
        }
        fn led_on(&mut self) -> DevResult {
            self.0.push("on");
            Ok(())
        }
        fn led_off(&mut self) -> DevResult {
            self.0.push("off");
            Ok(())
        }
        fn cleanup_led(&mut self) -> DevResult {
            self.0.push("cleanup");
            Ok(())
        }
    }

    #[derive(Default)]
    struct NvmVectorMock {
        word: u16,
        writes: Vec<(u16, u16)>,
        checksums: usize,
    }

    #[derive(Default)]
    struct SleepMock {
        ulp: usize,
        acquired: usize,
        released: usize,
        lpi: u16,
        advertisement: u16,
    }

    #[derive(Default)]
    struct FirmwareNvmMock(BTreeMap<u16, u16>);
    impl super::super::nvm::E1000NvmAccess for FirmwareNvmMock {
        fn read_nvm_words(&mut self, offset: u16, words: u16) -> DevResult<alloc::vec::Vec<u16>> {
            Ok((0..words)
                .map(|index| self.0.get(&(offset + index)).copied().unwrap_or(0))
                .collect())
        }
        fn write_nvm_words(&mut self, _offset: u16, _words: &[u16]) -> DevResult {
            Ok(())
        }
    }
    impl EmSleepPowerOps for SleepMock {
        fn enable_ulp_lpt_lp(&mut self) -> DevResult {
            self.ulp += 1;
            Ok(())
        }
        fn acquire_phy(&mut self) -> DevResult {
            self.acquired += 1;
            Ok(())
        }
        fn read_lpi_control(&mut self) -> DevResult<u16> {
            Ok(self.lpi)
        }
        fn read_eee_advertisement(&mut self) -> DevResult<u16> {
            Ok(self.advertisement)
        }
        fn write_lpi_control(&mut self, value: u16) -> DevResult {
            self.lpi = value;
            Ok(())
        }
        fn release_phy(&mut self) {
            self.released += 1;
        }
    }
    impl EmNvmVectorOps for NvmVectorMock {
        fn read_nvm_word(&mut self, _offset: u16) -> u16 {
            self.word
        }
        fn write_nvm_word(&mut self, offset: u16, value: u16) {
            self.writes.push((offset, value));
            self.word = value;
        }
        fn update_nvm_checksum(&mut self) {
            self.checksums += 1;
        }
    }
    impl EmVlanOps for VlanMock {
        fn set_vf_vlan(&mut self, _vid: u16, add: bool) -> DevResult {
            if !add && self.fail_remove {
                Err(DevError::Io)
            } else {
                Ok(())
            }
        }
        fn rebuild_iov_vlan(&mut self) {}
        fn write_vfta(&mut self, index: u32, value: u32) -> DevResult {
            self.writes.push((index, value));
            Ok(())
        }
        fn set_pf_vlan_promisc(&mut self, _enabled: bool) {}
        fn retry_add(&mut self, _vid: u16) {}
        fn retry_clear(&mut self, _vid: u16) {}
        fn write_rlpml(&mut self, _size: u32) -> DevResult {
            Ok(())
        }
    }

    #[test]
    fn pci_identification_and_firmware_ownership_registers_follow_source() {
        let mut pci = PciMock::default();
        for (reg, value) in [
            (0, 0x8086),
            (2, 0x100e),
            (4, 0),
            (8, 1),
            (0x2c, 0x8086),
            (0x2e, 1),
        ] {
            pci.0.insert(reg, value);
        }
        let identity = em_identify_hardware(&mut pci, false).unwrap();
        assert_eq!(identity.mac, E1000MacType::I82540);
        assert_eq!(identity.vendor_id, 0x8086);
        let mut io = RegisterMock::default();
        em_get_hw_control(&mut io, E1000MacType::I82573, false).unwrap();
        assert_eq!(io.0[&E1000_SWSM] & E1000_SWSM_DRV_LOAD, E1000_SWSM_DRV_LOAD);
        em_release_hw_control(&mut io, E1000MacType::I82573, true).unwrap();
        assert_eq!(io.0[&E1000_SWSM] & E1000_SWSM_DRV_LOAD, 0);
    }

    #[test]
    fn management_host_setup_preserves_unrelated_bits() {
        let mut io = RegisterMock::default();
        io.0.insert(E1000_MANC, E1000_MANC_ARP_EN | 0x100);
        io.0.insert(E1000_MANC2H, 0x80);
        em_init_manageability(&mut io, true).unwrap();
        assert_eq!(
            io.0[&E1000_MANC] & (E1000_MANC_ARP_EN | E1000_MANC_EN_MNG2HOST),
            E1000_MANC_EN_MNG2HOST
        );
        assert_eq!(io.0[&E1000_MANC2H], 0xe0);
        em_release_manageability(&mut io, true).unwrap();
        assert_ne!(io.0[&E1000_MANC] & E1000_MANC_ARP_EN, 0);
        assert_eq!(io.0[&E1000_MANC] & E1000_MANC_EN_MNG2HOST, 0);
    }
    #[test]
    fn vlan_shadow_tracks_registered_and_stale_vf_ids() {
        let mut ops = VlanMock::default();
        let mut table = VftaTable::default();
        em_if_vlan_register(&mut ops, &mut table, 100, false, false, false).unwrap();
        assert_eq!(table.count, 1);
        assert_eq!(ops.writes.last(), Some(&(3, 1 << 4)));
        em_if_vlan_unregister(&mut ops, &mut table, 100, false, false, false).unwrap();
        assert_eq!(table.count, 0);
        assert_eq!(ops.writes.last(), Some(&(3, 0)));
        ops.fail_remove = true;
        em_if_vlan_register(&mut ops, &mut table, 9, true, false, false).unwrap();
        em_if_vlan_unregister(&mut ops, &mut table, 9, true, false, false).unwrap();
        assert_ne!(table.stale[0] & (1 << 9), 0);
    }
    #[test]
    fn queue_limits_and_aim_deltas_match_generation_policy() {
        assert_eq!(em_set_num_queues(E1000MacType::I82576), 8);
        assert_eq!(em_set_num_queues(E1000MacType::I82574), 2);
        assert_eq!(em_set_num_queues(E1000MacType::I210), 4);
        let mut ring = AimRing {
            snapshot: (50u64 << 32) | 5,
            bytes_last: 20,
            packets_last: 2,
        };
        assert_eq!(em_aim_rx_delta(&mut ring), (30, 3));
        assert_eq!(em_aim_rx_delta(&mut ring), (0, 0));
    }
    #[test]
    fn memory_and_frame_rules_cover_old_and_new_families() {
        assert_eq!(em_memory_error_intr_mask(E1000MacType::I82575), 0x03c00000);
        assert!(em_mac_has_eee(E1000MacType::Pch2Lan));
        assert!(!em_is_valid_ether_addr([1, 0, 0, 0, 0, 1]));
        assert!(em_if_mtu_set(E1000MacType::I82542, 1500, false).is_ok());
        assert!(em_if_mtu_set(E1000MacType::I82542, 1501, false).is_err());
    }

    #[test]
    fn media_policy_matches_em_speed_duplex_and_link_state() {
        let mut cfg = EmMediaConfig {
            autoneg: false,
            autoneg_advertised: 0,
            forced_speed_duplex: 0,
        };
        em_if_media_change(EmMediaRequest::Copper100 { full_duplex: true }, &mut cfg).unwrap();
        assert_eq!(cfg.forced_speed_duplex, 0x8);
        em_if_media_change(EmMediaRequest::Copper10 { full_duplex: false }, &mut cfg).unwrap();
        assert_eq!(cfg.forced_speed_duplex, 0x1);
        em_if_media_change(EmMediaRequest::Auto, &mut cfg).unwrap();
        assert!(cfg.autoneg);
        assert_eq!(cfg.autoneg_advertised, 0x2f);
        let down = em_if_media_status(EmMediaType::Copper, E1000MacType::I82540, false, 1000, true);
        assert!(down.valid && !down.active);
        let fiber = em_if_media_status(EmMediaType::Fiber, E1000MacType::I82545, true, 0, true);
        assert_eq!(fiber.subtype, Some(EmMediaRequest::Fiber1000 { lx: true }));
        assert_eq!(em_set_flowcntl(3).unwrap(), 3);
        assert!(em_set_flowcntl(4).is_err());
    }

    #[test]
    fn wake_link_and_led_helpers_keep_generation_order() {
        let mut state = EmWakeLinkState {
            suspend_link_powered_down: true,
        };
        let mut wake = WakeMock::default();
        em_power_up_wakeup_link(
            &mut wake,
            E1000MacType::I82576,
            EmMediaType::Fiber,
            &mut state,
        )
        .unwrap();
        assert_eq!(wake.0, ["up_fiber", "setup"]);
        assert!(!state.suspend_link_powered_down);
        em_power_down_wakeup_link(
            &mut wake,
            E1000MacType::I82576,
            EmMediaType::Fiber,
            &mut state,
        )
        .unwrap();
        assert_eq!(wake.0.last(), Some(&"down_fiber"));
        assert!(state.suspend_link_powered_down);

        let mut led = LedMock::default();
        em_if_led_func(&mut led, true, true).unwrap();
        em_if_led_func(&mut led, false, false).unwrap();
        assert_eq!(led.0, ["setup", "blink", "off", "cleanup"]);
    }

    #[test]
    fn e82574_vector_nvm_update_only_changes_msix_count() {
        let mut nvm = NvmVectorMock {
            word: 0x0045,
            ..NvmVectorMock::default()
        };
        em_enable_vectors_82574(&mut nvm);
        assert_eq!(nvm.word, (0x0045 & !(0x7 << 7)) | (4 << 7));
        assert_eq!(nvm.checksums, 1);
        em_enable_vectors_82574(&mut nvm);
        assert_eq!(nvm.checksums, 1);
    }

    #[test]
    fn igb_interrupt_masks_preserve_other_vectors_and_reset_gate() {
        let config = IgbInterruptConfig {
            msix: true,
            queue_mask: 0x3,
            link_mask: 0x4,
            reset_mask: 0x8,
            iov_mask: 0x10,
            fatal_mask: 0x20,
            ..IgbInterruptConfig::default()
        };
        let mut io = RegisterMock::default();
        io.0.insert(E1000_EIAC, 0x100);
        io.0.insert(E1000_EIAM, 0x200);
        let mut ops = IgbInterruptMock::default();
        igb_if_intr_enable(&mut io, &mut ops, config).unwrap();
        assert_eq!(io.0[&E1000_EIAC], 0x107);
        assert_eq!(io.0[&E1000_EIAM], 0x207);
        assert_eq!(io.0[&E1000_EIMS], 7);
        assert_eq!(io.0[&E1000_IMS], 0x3c);
        assert_eq!(ops.drained, 1);
        igb_if_intr_disable(&mut io, &mut ops, config).unwrap();
        assert_eq!(io.0[&E1000_EIAM], 0x200);
        assert_eq!(io.0[&E1000_EIAC], 0x100);
        assert_eq!(io.0[&E1000_EIMC], 7);
        assert_eq!(io.0[&E1000_IMC], u32::MAX);
        assert_eq!(ops.reset_prepared, 1);
    }

    #[test]
    fn pch_sleep_wakeup_filters_guard_ulp_and_eee_phy_lock() {
        let mut ops = SleepMock {
            advertisement: 0x6,
            ..SleepMock::default()
        };
        let config = EmSleepPowerConfig {
            mac: E1000MacType::PchLpt,
            wake_filters: E1000_WUFC_MAG,
            suspend_link_powered_down: false,
            phy_i217: true,
            eee_disabled: false,
            eee_low_power_ability: 0x6,
        };
        em_configure_sx_low_power(&mut ops, config).unwrap();
        assert_eq!(ops.ulp, 1);
        assert_eq!(ops.lpi, 0x6000);
        assert_eq!((ops.acquired, ops.released), (1, 1));

        ops.ulp = 0;
        let directed = EmSleepPowerConfig {
            wake_filters: E1000_WUFC_EX,
            ..config
        };
        em_configure_sx_low_power(&mut ops, directed).unwrap();
        assert_eq!(ops.ulp, 0);
    }

    #[test]
    fn firmware_version_uses_legacy_eeprom_word_before_igb() {
        let mut nvm = FirmwareNvmMock::default();
        nvm.0.insert(5, 0x1234);
        let version = em_fw_version_locked(&mut nvm, E1000MacType::I82574);
        assert_eq!(
            (version.eep_major, version.eep_minor, version.eep_build),
            (1, 0x23, 4)
        );
    }
}
