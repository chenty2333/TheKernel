//! FreeBSD `if_em.c` adapter policy translated to the TheKernel driver layer.
//!
//! Source revision `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2001-2024, Intel Corporation; copyright (c) 2016 Nicole
//! Graziano; copyright (c) 2024 Kevin Bowling.

use axdriver_base::{DevError, DevResult};

use super::{api::E1000MacType, osdep::E1000RegisterIo, registers::*};

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
const MAX_SCATTER: u32 = 64;
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

/// upstream: if_em.c em_set_num_queues()
pub const fn em_set_num_queues(mac: E1000MacType) -> u8 {
    match mac {
        E1000MacType::I82576 | E1000MacType::I82580 | E1000MacType::I350 | E1000MacType::I354 => 8,
        E1000MacType::I82575 | E1000MacType::I210 => 4,
        E1000MacType::I82574 | E1000MacType::I211 => 2,
        _ => 1,
    }
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
        rate = (ITR_RATE_DIVIDEND as u64 * 1000 / (u64::from(rate) * ITR_RATE_MULTIPLIER)) as u32;
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
