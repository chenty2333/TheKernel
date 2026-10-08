//! Intel e1000 virtual-function MAC protocol helpers.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_vf.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{osdep::E1000RegisterIo, registers::*};

const MAILBOX_TIMEOUT: u32 = 2000;
const MAILBOX_SIZE: usize = 16;
const MSG_ACK: u32 = 0x8000_0000;
const MSG_NACK: u32 = 0x4000_0000;
const MSG_CTS: u32 = 0x2000_0000;
const MSG_INFO_SHIFT: u32 = 16;
const PF_CONTROL_MSG: u32 = 0x0100;
const VF_RESET: u32 = 0x01;
const VF_SET_MAC: u32 = 0x02;
const VF_SET_MC: u32 = 0x03;
const VF_SET_VLAN: u32 = 0x04;
const VF_SET_LPE: u32 = 0x05;
const VF_SET_PROMISC: u32 = 0x06;
const STATUS_SPEED_100: u32 = 0x40;
const STATUS_SPEED_1000: u32 = 0x80;
const STATUS_FD: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfPhyType {
    NoPhy,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfNvmType {
    None,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfPromisc {
    Disabled,
    Unicast,
    Multicast,
    Enabled,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfMacParams {
    pub media_unknown: bool,
    pub asf: bool,
    pub arc_valid: bool,
    pub adaptive_ifs: bool,
    pub mta_count: u16,
    pub rar_count: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfState {
    pub perm_addr: [u8; 6],
    pub addr: [u8; 6],
    pub get_link_status: bool,
    pub mailbox_timeout: u32,
}

pub trait VfMailbox {
    fn check_for_reset(&mut self) -> DevResult;
    fn write_posted(&mut self, message: &[u32]) -> DevResult;
    fn read_posted(&mut self, message: &mut [u32]) -> DevResult;
    fn read(&mut self, message: &mut [u32], unlock: bool) -> DevResult;
}

pub trait VfOps {
    fn setup_link(&mut self) -> DevResult;
    fn reset_hw(&mut self) -> DevResult;
    fn init_hw(&mut self) -> DevResult;
    fn init_phy_params(&mut self) -> DevResult;
    fn init_nvm_params(&mut self) -> DevResult;
    fn init_mbx_params(&mut self) -> DevResult;
}

/// upstream: e1000_vf.c e1000_init_phy_params_vf()
pub const fn init_phy_params_vf() -> VfPhyType {
    VfPhyType::NoPhy
}
/// upstream: e1000_vf.c e1000_init_nvm_params_vf()
pub const fn init_nvm_params_vf() -> VfNvmType {
    VfNvmType::None
}

/// upstream: e1000_vf.c e1000_init_mac_params_vf()
pub const fn init_mac_params_vf() -> VfMacParams {
    VfMacParams {
        media_unknown: true,
        asf: false,
        arc_valid: false,
        adaptive_ifs: false,
        mta_count: 128,
        rar_count: 1,
    }
}

/// upstream: e1000_vf.c e1000_init_function_pointers_vf()
pub const fn init_function_pointers_vf() -> (bool, bool, bool, bool) {
    (true, true, true, true)
}

/// upstream: e1000_vf.c e1000_acquire_vf()
pub const fn acquire_vf() -> DevResult {
    Err(DevError::Unsupported)
}
/// upstream: e1000_vf.c e1000_release_vf()
pub const fn release_vf() {}
/// upstream: e1000_vf.c e1000_setup_link_vf()
pub const fn setup_link_vf() -> DevResult {
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfBusInfo {
    pub reserved_type: bool,
    pub speed_mbps: u16,
}
/// upstream: e1000_vf.c e1000_get_bus_info_pcie_vf()
pub const fn get_bus_info_pcie_vf() -> VfBusInfo {
    VfBusInfo {
        reserved_type: true,
        speed_mbps: 2500,
    }
}

/// upstream: e1000_vf.c e1000_get_link_up_info_vf()
pub fn get_link_up_info_vf<I: E1000RegisterIo>(io: &mut I) -> DevResult<(u16, u16)> {
    let status = io.read_register(E1000_STATUS)?;
    let speed = if status & STATUS_SPEED_1000 != 0 {
        1000
    } else if status & STATUS_SPEED_100 != 0 {
        100
    } else {
        10
    };
    let duplex = if status & STATUS_FD != 0 { 2 } else { 1 };
    Ok((speed, duplex))
}

/// upstream: e1000_vf.c e1000_reset_hw_vf()
pub fn reset_hw_vf<I: E1000RegisterIo, M: VfMailbox>(
    io: &mut I,
    mailbox: &mut M,
    state: &mut VfState,
    reset_timeout: usize,
) -> DevResult {
    let ctrl = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    let mut remaining = reset_timeout;
    while remaining != 0 && mailbox.check_for_reset().is_err() {
        remaining -= 1;
        io.delay_us(5);
    }
    if remaining == 0 {
        return Err(DevError::ResourceBusy);
    }
    state.mailbox_timeout = MAILBOX_TIMEOUT;
    let mut request = [VF_RESET, u32::MAX, u32::MAX];
    mailbox.write_posted(&request)?;
    io.delay_us(10_000);
    mailbox.read_posted(&mut request)?;
    let response = request[0] & !MSG_CTS;
    match response {
        x if x == (VF_RESET | MSG_ACK) => {
            state.perm_addr = [
                request[1] as u8,
                (request[1] >> 8) as u8,
                (request[1] >> 16) as u8,
                (request[1] >> 24) as u8,
                request[2] as u8,
                (request[2] >> 8) as u8,
            ];
            Ok(())
        }
        x if x == (VF_RESET | MSG_NACK) && request[1] == 0 && request[2] == 0 => {
            state.perm_addr = [0; 6];
            Ok(())
        }
        _ => Err(DevError::Io),
    }
}

/// upstream: e1000_vf.c e1000_init_hw_vf()
pub fn init_hw_vf<M: VfMailbox>(mailbox: &mut M, state: &mut VfState) -> DevResult {
    let _ = rar_set_vf(mailbox, state, state.addr, 0);
    Ok(())
}

/// upstream: e1000_vf.c e1000_rar_set_vf()
pub fn rar_set_vf<M: VfMailbox>(
    mailbox: &mut M,
    state: &mut VfState,
    address: [u8; 6],
    _index: u32,
) -> DevResult {
    let mut message = [0u32; 3];
    message[0] = VF_SET_MAC;
    message[1] = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    message[2] = u32::from(address[4]) | (u32::from(address[5]) << 8);
    let result = mailbox
        .write_posted(&message)
        .and_then(|_| mailbox.read_posted(&mut message));
    message[0] &= !MSG_CTS;
    if result.is_ok() && message[0] == (VF_SET_MAC | MSG_NACK) {
        read_mac_addr_vf(state);
    }
    // The upstream setter intentionally reports success even when PF rejects it.
    Ok(())
}

/// upstream: e1000_vf.c e1000_hash_mc_addr_vf()
pub fn hash_mc_addr_vf(address: [u8; 6], mta_regs: u16) -> u32 {
    let mask = u32::from(mta_regs) * 32 - 1;
    let mut shift = 1u8;
    while shift < 4 && mask >> shift != 0xff {
        shift += 1;
    }
    ((u32::from(address[4]) >> (8 - shift)) | (u32::from(address[5]) << shift)) & mask
}

/// upstream: e1000_vf.c e1000_write_msg_read_ack()
pub fn write_msg_read_ack<M: VfMailbox>(mailbox: &mut M, message: &[u32]) {
    let mut response = [0u32; MAILBOX_SIZE];
    if mailbox.write_posted(message).is_ok() {
        let _ = mailbox.read_posted(&mut response);
    }
}

/// upstream: e1000_vf.c e1000_set_uc_addr_vf()
pub fn set_uc_addr_vf<M: VfMailbox>(
    mailbox: &mut M,
    subcommand: u32,
    address: Option<[u8; 6]>,
) -> DevResult {
    let mut message = [0u32; 3];
    let command = VF_SET_MAC | subcommand;
    message[0] = command;
    if let Some(addr) = address {
        message[1] = u32::from(addr[0])
            | (u32::from(addr[1]) << 8)
            | (u32::from(addr[2]) << 16)
            | (u32::from(addr[3]) << 24);
        message[2] = u32::from(addr[4]) | (u32::from(addr[5]) << 8);
    }
    mailbox.write_posted(&message)?;
    mailbox.read_posted(&mut message)?;
    message[0] &= !MSG_CTS;
    if message[0] == (command | MSG_NACK) {
        Err(DevError::ResourceBusy)
    } else {
        Ok(())
    }
}

/// upstream: e1000_vf.c e1000_update_mc_addr_list_vf()
pub fn update_mc_addr_list_vf<M: VfMailbox>(mailbox: &mut M, addresses: &[[u8; 6]], mta_regs: u16) {
    let count = addresses.len().min(30);
    let mut words = [0u32; MAILBOX_SIZE];
    words[0] = VF_SET_MC | ((count as u32) << MSG_INFO_SHIFT);
    if addresses.len() > 30 {
        words[0] |= 0x80 << MSG_INFO_SHIFT;
    }
    for (index, address) in addresses.iter().take(count).enumerate() {
        let hash = hash_mc_addr_vf(*address, mta_regs) & 0x0fff;
        let shift = (index & 1) * 16;
        words[1 + index / 2] |= hash << shift;
    }
    write_msg_read_ack(mailbox, &words)
}

/// upstream: e1000_vf.c e1000_vfta_set_vf()
pub fn vfta_set_vf<M: VfMailbox>(mailbox: &mut M, vid: u16, set: bool) -> DevResult {
    let mut message = [u32::from(VF_SET_VLAN), u32::from(vid)];
    if set {
        message[0] |= 1 << MSG_INFO_SHIFT;
    }
    mailbox.write_posted(&message)?;
    mailbox.read_posted(&mut message[..1])?;
    if message[0] & 0xffff != VF_SET_VLAN || message[0] & MSG_ACK == 0 {
        Err(DevError::Io)
    } else {
        Ok(())
    }
}

/// upstream: e1000_vf.c e1000_rlpml_set_vf()
pub fn rlpml_set_vf<M: VfMailbox>(mailbox: &mut M, max_size: u16) {
    write_msg_read_ack(mailbox, &[VF_SET_LPE, u32::from(max_size)]);
}

/// upstream: e1000_vf.c e1000_promisc_set_vf()
pub fn promisc_set_vf<M: VfMailbox>(mailbox: &mut M, mode: VfPromisc) -> DevResult {
    let mut message = VF_SET_PROMISC;
    match mode {
        VfPromisc::Multicast => message |= 2 << MSG_INFO_SHIFT,
        VfPromisc::Enabled => message |= (1 | 2) << MSG_INFO_SHIFT,
        VfPromisc::Unicast => message |= 1 << MSG_INFO_SHIFT,
        VfPromisc::Disabled => {}
    }
    mailbox.write_posted(&[message])?;
    let mut response = [0];
    mailbox.read_posted(&mut response)?;
    if response[0] & MSG_ACK == 0 {
        Err(DevError::Io)
    } else {
        Ok(())
    }
}

/// upstream: e1000_vf.c e1000_read_mac_addr_vf()
pub fn read_mac_addr_vf(state: &mut VfState) {
    state.addr = state.perm_addr;
}

/// upstream: e1000_vf.c e1000_check_for_link_vf()
pub fn check_for_link_vf<I: E1000RegisterIo, M: VfMailbox>(
    io: &mut I,
    mailbox: &mut M,
    state: &mut VfState,
) -> DevResult {
    if mailbox.check_for_reset().is_err() || state.mailbox_timeout == 0 {
        state.get_link_status = true;
    }
    if !state.get_link_status || io.read_register(E1000_STATUS)? & 2 == 0 {
        return Ok(());
    }
    let mut message = [0u32];
    if mailbox.read(&mut message, true).is_err() {
        return Ok(());
    }
    if message[0] & MSG_CTS == 0 {
        if message[0] & MSG_NACK != 0 || message[0] & 0xffff == PF_CONTROL_MSG {
            return Err(DevError::Io);
        }
        return Ok(());
    }
    if state.mailbox_timeout == 0 {
        return Err(DevError::Io);
    }
    state.get_link_status = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vf_multicast_hash_and_mailbox_parameter_defaults() {
        assert_eq!(
            hash_mc_addr_vf([1, 0, 0, 0, 0, 1], 128),
            ((0 >> 4) | (1 << 4)) & 4095
        );
        assert_eq!(init_mac_params_vf().rar_count, 1);
        assert_eq!(init_mac_params_vf().mta_count, 128);
        assert_eq!(get_bus_info_pcie_vf().speed_mbps, 2500);
    }
}
