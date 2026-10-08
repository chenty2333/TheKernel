//! Shared Intel e1000 MAC operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_mac.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    osdep::{E1000PciConfig, E1000RegisterIo, read_pcie_cap_reg},
    registers::*,
};

const ETHER_ADDR_LEN: usize = 6;
const MTA_REG_COUNT: usize = 128;
const VFTA_REG_COUNT: usize = 128;
const RAL: u32 = 0x05400;
const RAH: u32 = 0x05404;
const PCI_HEADER_TYPE_REGISTER: u32 = 0x0e;
const PCI_HEADER_TYPE_MULTIFUNC: u16 = 0x80;
const PCIE_LINK_STATUS: u32 = 0x12;
const PCIE_LINK_SPEED_MASK: u16 = 0x000f;
const PCIE_LINK_WIDTH_MASK: u16 = 0x03f0;
const PCIE_LINK_WIDTH_SHIFT: u32 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusType {
    Pci,
    PciX,
    PciExpress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusSpeed {
    Unknown,
    Reserved,
    Mhz33,
    Mhz66,
    Mhz100,
    Mhz133,
    Gt2500,
    Gt5000,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusWidth {
    Unknown,
    Bits32,
    Bits64,
    Pcie(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E1000BusInfo {
    pub kind: E1000BusType,
    pub speed: E1000BusSpeed,
    pub width: E1000BusWidth,
    pub function: u8,
}

/// upstream: e1000_mac.c e1000_set_lan_id_multi_port_pcie()
pub fn set_lan_id_multi_port_pcie<I: E1000RegisterIo>(io: &mut I) -> DevResult<u8> {
    Ok(
        ((io.read_register(E1000_STATUS)? & E1000_STATUS_FUNC_MASK) >> E1000_STATUS_FUNC_SHIFT)
            as u8,
    )
}

/// upstream: e1000_mac.c e1000_set_lan_id_multi_port_pci()
pub fn set_lan_id_multi_port_pci<I: E1000RegisterIo, P: E1000PciConfig>(
    io: &mut I,
    pci: &mut P,
) -> DevResult<u8> {
    let header = super::osdep::read_pci_cfg(pci, PCI_HEADER_TYPE_REGISTER)?;
    if header & PCI_HEADER_TYPE_MULTIFUNC != 0 {
        set_lan_id_multi_port_pcie(io)
    } else {
        Ok(0)
    }
}

/// upstream: e1000_mac.c e1000_set_lan_id_single_port()
pub const fn set_lan_id_single_port() -> u8 {
    0
}

/// upstream: e1000_mac.c e1000_get_bus_info_pci_generic()
pub fn get_bus_info_pci_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult<E1000BusInfo> {
    let status = io.read_register(E1000_STATUS)?;
    let kind = if status & E1000_STATUS_PCIX_MODE != 0 {
        E1000BusType::PciX
    } else {
        E1000BusType::Pci
    };
    let speed = if kind == E1000BusType::Pci {
        if status & E1000_STATUS_PCI66 != 0 {
            E1000BusSpeed::Mhz66
        } else {
            E1000BusSpeed::Mhz33
        }
    } else {
        match status & E1000_STATUS_PCIX_SPEED {
            E1000_STATUS_PCIX_SPEED_66 => E1000BusSpeed::Mhz66,
            E1000_STATUS_PCIX_SPEED_100 => E1000BusSpeed::Mhz100,
            E1000_STATUS_PCIX_SPEED_133 => E1000BusSpeed::Mhz133,
            _ => E1000BusSpeed::Reserved,
        }
    };
    Ok(E1000BusInfo {
        kind,
        speed,
        width: if status & E1000_STATUS_BUS64 != 0 {
            E1000BusWidth::Bits64
        } else {
            E1000BusWidth::Bits32
        },
        function: set_lan_id_multi_port_pcie(io)?,
    })
}

/// upstream: e1000_mac.c e1000_get_bus_info_pcie_generic()
pub fn get_bus_info_pcie_generic<I: E1000RegisterIo, P: E1000PciConfig>(
    io: &mut I,
    pci: &mut P,
) -> DevResult<E1000BusInfo> {
    let link = read_pcie_cap_reg(pci, PCIE_LINK_STATUS).ok();
    let (speed, width) = match link {
        Some(link) => (
            match link & PCIE_LINK_SPEED_MASK {
                1 => E1000BusSpeed::Gt2500,
                2 => E1000BusSpeed::Gt5000,
                _ => E1000BusSpeed::Unknown,
            },
            E1000BusWidth::Pcie(((link & PCIE_LINK_WIDTH_MASK) >> PCIE_LINK_WIDTH_SHIFT) as u8),
        ),
        None => (E1000BusSpeed::Unknown, E1000BusWidth::Unknown),
    };
    Ok(E1000BusInfo {
        kind: E1000BusType::PciExpress,
        speed,
        width,
        function: set_lan_id_multi_port_pcie(io)?,
    })
}

/// upstream: e1000_mac.c e1000_config_collision_dist_generic()
pub fn config_collision_dist_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut control = io.read_register(E1000_TCTL)?;
    control &= !E1000_TCTL_COLD;
    control |= E1000_COLLISION_DISTANCE << E1000_COLD_SHIFT;
    io.write_register(E1000_TCTL, control)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_null_ops_generic()
pub const fn null_ops_generic() -> i32 {
    0
}

/// upstream: e1000_mac.c e1000_null_mac_generic()
pub const fn null_mac_generic() {}

/// upstream: e1000_mac.c e1000_null_link_info()
pub const fn null_link_info() -> (i32, u16, u16) {
    (0, 0, 0)
}

/// upstream: e1000_mac.c e1000_null_mng_mode()
pub const fn null_mng_mode() -> bool {
    false
}

/// upstream: e1000_mac.c e1000_null_update_mc()
pub const fn null_update_mc() {}

/// upstream: e1000_mac.c e1000_null_write_vfta()
pub const fn null_write_vfta() {}

/// upstream: e1000_mac.c e1000_null_rar_set()
pub const fn null_rar_set() -> i32 {
    0
}

/// upstream: e1000_mac.c e1000_null_set_obff_timer()
pub const fn null_set_obff_timer() -> i32 {
    0
}

fn array_register(base: u32, index: usize, count: usize, limit: u32) -> DevResult<u32> {
    if index >= count {
        return Err(DevError::InvalidParam);
    }
    let offset = (index as u32)
        .checked_mul(4)
        .ok_or(DevError::InvalidParam)?;
    let register = base.checked_add(offset).ok_or(DevError::InvalidParam)?;
    if register >= limit {
        return Err(DevError::InvalidParam);
    }
    Ok(register)
}

/// upstream: e1000_mac.c e1000_rar_set_generic()
pub fn rar_set_generic<I: E1000RegisterIo>(io: &mut I, address: [u8; 6], index: u32) -> DevResult {
    let index = usize::try_from(index).map_err(|_| DevError::InvalidParam)?;
    let low_reg = array_register(RAL, index, 16, 0x6000)?;
    let high_reg = array_register(RAH, index, 16, 0x6000)?;
    let low = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    let mut high = u32::from(address[4]) | (u32::from(address[5]) << 8);
    if low != 0 || high != 0 {
        high |= E1000_RAH_AV;
    }

    // Separate writes and flushes avoid bridge coalescing on affected parts.
    io.write_register(low_reg, low)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.write_register(high_reg, high)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_hash_mc_addr_generic()
pub fn hash_mc_addr_generic(
    address: [u8; 6],
    mta_reg_count: usize,
    filter_type: u8,
) -> DevResult<u32> {
    if mta_reg_count == 0 || !mta_reg_count.is_power_of_two() {
        return Err(DevError::InvalidParam);
    }
    let hash_mask = u32::try_from(
        mta_reg_count
            .checked_mul(32)
            .ok_or(DevError::InvalidParam)?
            - 1,
    )
    .map_err(|_| DevError::InvalidParam)?;
    let mut bit_shift = 1u32;
    while bit_shift < 4 && (hash_mask >> bit_shift) != 0xff {
        bit_shift += 1;
    }
    bit_shift += match filter_type {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 0,
    };
    let mut hash = u32::from(address[4]) >> (8 - bit_shift);
    hash |= u32::from(address[5]) << bit_shift;
    Ok(hash & hash_mask)
}

/// upstream: e1000_mac.c e1000_clear_vfta_generic()
pub fn clear_vfta_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for index in 0..VFTA_REG_COUNT {
        let register = array_register(E1000_VFTA, index, VFTA_REG_COUNT, 0x6000)?;
        io.write_register(register, 0)?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_write_vfta_generic()
pub fn write_vfta_generic<I: E1000RegisterIo>(io: &mut I, offset: u32, value: u32) -> DevResult {
    let register = array_register(
        E1000_VFTA,
        usize::try_from(offset).map_err(|_| DevError::InvalidParam)?,
        VFTA_REG_COUNT,
        0x6000,
    )?;
    io.write_register(register, value)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_update_mc_addr_list_generic()
pub fn update_mc_addr_list_generic<I: E1000RegisterIo>(
    io: &mut I,
    addresses: &[[u8; ETHER_ADDR_LEN]],
    mta_reg_count: usize,
    filter_type: u8,
) -> DevResult {
    if mta_reg_count == 0 || mta_reg_count > MTA_REG_COUNT || !mta_reg_count.is_power_of_two() {
        return Err(DevError::InvalidParam);
    }
    let mut shadow = [0u32; MTA_REG_COUNT];
    for address in addresses {
        let hash_value = hash_mc_addr_generic(*address, mta_reg_count, filter_type)?;
        let register = ((hash_value >> 5) as usize) & (mta_reg_count - 1);
        let bit = hash_value & 0x1f;
        shadow[register] |= 1u32 << bit;
    }
    for index in (0..mta_reg_count).rev() {
        let register = array_register(E1000_MTA, index, MTA_REG_COUNT, 0x6000)?;
        io.write_register(register, shadow[index])?;
    }
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Registers {
        writes: alloc::vec::Vec<(u32, u32)>,
        status: u32,
    }
    impl E1000RegisterIo for Registers {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            Ok(if register == E1000_STATUS {
                self.status
            } else {
                0
            })
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            self.writes.push((register, value));
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }

    struct Pci {
        words: [u16; 128],
        pcie: Option<u32>,
    }
    impl Default for Pci {
        fn default() -> Self {
            Self {
                words: [0; 128],
                pcie: None,
            }
        }
    }
    impl E1000PciConfig for Pci {
        fn read_config_u16(&mut self, register: u32) -> Option<u16> {
            self.words.get(register as usize / 2).copied()
        }
        fn write_config_u16(&mut self, register: u32, value: u16) -> bool {
            let Some(word) = self.words.get_mut(register as usize / 2) else {
                return false;
            };
            *word = value;
            true
        }
        fn find_capability(&mut self, capability_id: u8) -> Option<u32> {
            (capability_id == 0x10).then_some(self.pcie).flatten()
        }
    }

    #[test]
    fn generic_rar_encodes_little_endian_and_valid_bit() {
        let mut io = Registers::default();
        rar_set_generic(&mut io, [0x02, 0x11, 0x22, 0x33, 0x44, 0x55], 0).unwrap();
        assert_eq!(
            io.writes,
            [(RAL, 0x3322_1102), (RAH, E1000_RAH_AV | 0x5544)]
        );
    }

    #[test]
    fn generic_null_ops_preserve_success_and_false_defaults() {
        assert_eq!(null_ops_generic(), 0);
        null_mac_generic();
        assert_eq!(null_link_info(), (0, 0, 0));
        assert!(!null_mng_mode());
        null_update_mc();
        null_write_vfta();
        assert_eq!(null_rar_set(), 0);
        assert_eq!(null_set_obff_timer(), 0);
    }

    #[test]
    fn generic_pci_bus_info_decodes_type_speed_width_and_function() {
        let mut io = Registers {
            status: E1000_STATUS_PCIX_MODE
                | E1000_STATUS_PCIX_SPEED_100
                | E1000_STATUS_BUS64
                | E1000_STATUS_FUNC_1,
            ..Registers::default()
        };
        let info = get_bus_info_pci_generic(&mut io).unwrap();
        assert_eq!(info.kind, E1000BusType::PciX);
        assert_eq!(info.speed, E1000BusSpeed::Mhz100);
        assert_eq!(info.width, E1000BusWidth::Bits64);
        assert_eq!(info.function, 1);
        assert_eq!(set_lan_id_single_port(), 0);
    }

    #[test]
    fn generic_pcie_bus_info_and_pci_function_mapping() {
        let mut io = Registers {
            status: E1000_STATUS_FUNC_MASK & (2 << E1000_STATUS_FUNC_SHIFT),
            ..Registers::default()
        };
        let mut pci = Pci {
            pcie: Some(0x40),
            ..Pci::default()
        };
        pci.words[0x52 / 2] = 1 | (8 << PCIE_LINK_WIDTH_SHIFT);
        pci.words[PCI_HEADER_TYPE_REGISTER as usize / 2] = PCI_HEADER_TYPE_MULTIFUNC;
        let info = get_bus_info_pcie_generic(&mut io, &mut pci).unwrap();
        assert_eq!(info.kind, E1000BusType::PciExpress);
        assert_eq!(info.speed, E1000BusSpeed::Gt2500);
        assert_eq!(info.width, E1000BusWidth::Pcie(8));
        assert_eq!(info.function, 2);
        assert_eq!(set_lan_id_multi_port_pci(&mut io, &mut pci).unwrap(), 2);
    }

    #[test]
    fn generic_collision_distance_programs_tctl_field() {
        let mut io = Registers::default();
        config_collision_dist_generic(&mut io).unwrap();
        assert_eq!(
            io.writes,
            [(E1000_TCTL, E1000_COLLISION_DISTANCE << E1000_COLD_SHIFT)]
        );
    }

    #[test]
    fn multicast_hash_uses_upstream_filter_shifts() {
        let address = [1, 0xaa, 0, 0x12, 0x34, 0x56];
        assert_eq!(hash_mc_addr_generic(address, 128, 0).unwrap(), 0x563);
        assert_eq!(hash_mc_addr_generic(address, 128, 1).unwrap(), 0xac6);
        // The source comment's example says 0x163, but the executable C
        // expression shifts 0x56 by six and masks to 0x58d.
        assert_eq!(hash_mc_addr_generic(address, 128, 2).unwrap(), 0x58d);
        assert_eq!(hash_mc_addr_generic(address, 128, 3).unwrap(), 0x634);
    }

    #[test]
    fn generic_vfta_and_multicast_programming_are_bounded() {
        let mut io = Registers::default();
        write_vfta_generic(&mut io, 127, 0x1234).unwrap();
        assert_eq!(io.writes, [(E1000_VFTA + 127 * 4, 0x1234)]);
        assert!(write_vfta_generic(&mut io, 128, 1).is_err());
        update_mc_addr_list_generic(&mut io, &[[1, 0, 0, 0, 0, 1]], 128, 0).unwrap();
        assert_eq!(io.writes.len(), 129);
        assert_eq!(io.writes[1], (E1000_MTA + 127 * 4, 0));
    }
}
