//! Intel 82540/82545/82546 family operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82540.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};
use super::{
    api::E1000MacType,
    mac::E1000MediaType,
    nvm::{E1000NvmAccess, E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    registers::*,
};

const PHY_ID_M88E1011_I: u32 = 0x01410c50;
const M88_PHY_SPEC_CTRL: u16 = 0x10;
const M88_PHY_EXT_CTRL: u16 = 0x14;
const M88_PHY_PAGE_SELECT: u16 = 0x1f;
const M88_PHY_GEN_CONTROL: u16 = 0x1e;
const M88_PHY_VCO_REG_BIT8: u16 = 1 << 8;
const M88_PHY_VCO_REG_BIT11: u16 = 1 << 11;
const NVM_RESERVED_WORD: u16 = 0xffff;
const NVM_SERDES_AMPLITUDE: u16 = 0x002c;
const NVM_SERDES_AMPLITUDE_MASK: u16 = 0x000f;
const NVM_PHY_CLASS_WORD: u16 = 0x000f;
const NVM_PHY_CLASS_A: u16 = 0x0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmOverride82540 { Auto, MicrowireLarge, MicrowireSmall }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyParams82540 {
    pub address: u8,
    pub autoneg_mask: u16,
    pub reset_delay_us: u32,
    pub phy_id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82540 {
    pub media: E1000MediaType,
    pub mta_register_count: u16,
    pub rar_entry_count: u16,
    pub setup_physical_is_copper: bool,
    pub link_check: LinkCheck82540,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkCheck82540 { Copper, Fiber, Serdes }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitPointers82540 { pub mac: bool, pub nvm: bool, pub phy: bool }

/// PHY register callbacks used by family-specific workarounds.
pub trait PhyRegisterIo {
    fn read_phy_register(&mut self, register: u16) -> DevResult<u16>;
    fn write_phy_register(&mut self, register: u16, value: u16) -> DevResult;
    fn power_down_phy_copper(&mut self) -> DevResult;
}

/// High-level common helpers called by the family link-setup routines.
pub trait LinkOps82540 {
    fn setup_copper_m88(&mut self) -> DevResult;
    fn setup_copper_generic(&mut self) -> DevResult;
    fn setup_fiber_serdes_generic(&mut self) -> DevResult;
}

/// Operations called by the 82540 hardware initialization sequence.
pub trait InitOps82540 {
    fn init_id_led(&mut self) -> DevResult;
    fn clear_vfta(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, count: u16) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn pcix_mmrbc_workaround(&mut self) -> DevResult;
    fn clear_base_counters(&mut self) -> DevResult;
}

/// upstream: e1000_82540.c e1000_init_phy_params_82540()
pub fn init_phy_params_82540(mac: E1000MacType, phy_id: u32) -> DevResult<PhyParams82540> {
    if !matches!(mac, E1000MacType::I82540 | E1000MacType::I82545 | E1000MacType::I82545Rev3 | E1000MacType::I82546 | E1000MacType::I82546Rev3)
        || phy_id != PHY_ID_M88E1011_I
    { return Err(DevError::Io); }
    Ok(PhyParams82540 { address: 1, autoneg_mask: 0x2f, reset_delay_us: 10_000, phy_id })
}

/// upstream: e1000_82540.c e1000_init_nvm_params_82540()
pub fn init_nvm_params_82540<I: E1000RegisterIo>(io: &mut I, override_kind: NvmOverride82540) -> DevResult<E1000NvmConfig> {
    let eecd = io.read_register(E1000_EECD)?;
    let (address_bits, word_size) = match override_kind {
        NvmOverride82540::MicrowireLarge => (8, 256),
        NvmOverride82540::MicrowireSmall => (6, 64),
        NvmOverride82540::Auto if eecd & E1000_EECD_SIZE != 0 => (8, 256),
        NvmOverride82540::Auto => (6, 64),
    };
    Ok(E1000NvmConfig { kind: E1000NvmType::Microwire, word_size, delay_usec: 50, opcode_bits: 3, address_bits, page_size: 1 })
}

/// upstream: e1000_82540.c e1000_init_mac_params_82540()
pub fn init_mac_params_82540(device_id: u16) -> DevResult<MacParams82540> {
    let media = match device_id {
        0x1011 | 0x1027 | 0x107a | 0x1012 => E1000MediaType::Fiber,
        0x1028 | 0x107b => E1000MediaType::InternalSerdes,
        _ => E1000MediaType::Copper,
    };
    let link_check = match media {
        E1000MediaType::Copper => LinkCheck82540::Copper,
        E1000MediaType::Fiber => LinkCheck82540::Fiber,
        E1000MediaType::InternalSerdes => LinkCheck82540::Serdes,
        E1000MediaType::Other => return Err(DevError::Unsupported),
    };
    Ok(MacParams82540 { media, mta_register_count: 128, rar_entry_count: E1000_RAR_ENTRIES as u16, setup_physical_is_copper: media == E1000MediaType::Copper, link_check })
}

/// upstream: e1000_82540.c e1000_init_function_pointers_82540()
pub const fn init_function_pointers_82540() -> InitPointers82540 { InitPointers82540 { mac: true, nvm: true, phy: true } }

/// upstream: e1000_82540.c e1000_reset_hw_82540()
pub fn reset_hw_82540<I: E1000RegisterIo>(io: &mut I, mac: E1000MacType) -> DevResult {
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    if matches!(mac, E1000MacType::I82545Rev3 | E1000MacType::I82546Rev3) {
        io.write_register(E1000_CTRL_DUP, ctrl | E1000_CTRL_RST)?;
    } else {
        io.write_register_io(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    }
    io.delay_us(5_000);
    let manc = io.read_register(E1000_MANC)? & !E1000_MANC_ARP_EN;
    io.write_register(E1000_MANC, manc)?;
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    Ok(())
}

/// upstream: e1000_82540.c e1000_set_phy_mode_82540()
pub fn set_phy_mode_82540<N: E1000NvmAccess, P: PhyRegisterIo>(nvm: &mut N, phy: &mut P, mac: E1000MacType) -> DevResult {
    if mac != E1000MacType::I82545Rev3 { return Ok(()); }
    let word = nvm.read_nvm_words(NVM_PHY_CLASS_WORD, 1)?.first().copied().ok_or(DevError::Io)?;
    if word != NVM_RESERVED_WORD && word & NVM_PHY_CLASS_A != 0 {
        phy.write_phy_register(M88_PHY_PAGE_SELECT, 0x000b).map_err(|_| DevError::Io)?;
        phy.write_phy_register(M88_PHY_GEN_CONTROL, 0x8104).map_err(|_| DevError::Io)?;
    }
    Ok(())
}

/// upstream: e1000_82540.c e1000_adjust_serdes_amplitude_82540()
pub fn adjust_serdes_amplitude_82540<N: E1000NvmAccess, P: PhyRegisterIo>(nvm: &mut N, phy: &mut P) -> DevResult {
    let word = nvm.read_nvm_words(NVM_SERDES_AMPLITUDE, 1)?.first().copied().ok_or(DevError::Io)?;
    if word != NVM_RESERVED_WORD { phy.write_phy_register(M88_PHY_EXT_CTRL, word & NVM_SERDES_AMPLITUDE_MASK)?; }
    Ok(())
}

/// upstream: e1000_82540.c e1000_set_vco_speed_82540()
pub fn set_vco_speed_82540<P: PhyRegisterIo>(phy: &mut P) -> DevResult {
    let default_page = phy.read_phy_register(M88_PHY_PAGE_SELECT)?;
    phy.write_phy_register(M88_PHY_PAGE_SELECT, 5)?;
    let value = phy.read_phy_register(M88_PHY_GEN_CONTROL)? & !M88_PHY_VCO_REG_BIT8;
    phy.write_phy_register(M88_PHY_GEN_CONTROL, value)?;
    phy.write_phy_register(M88_PHY_PAGE_SELECT, 4)?;
    let value = phy.read_phy_register(M88_PHY_GEN_CONTROL)? | M88_PHY_VCO_REG_BIT11;
    phy.write_phy_register(M88_PHY_GEN_CONTROL, value)?;
    phy.write_phy_register(M88_PHY_PAGE_SELECT, default_page)
}

/// upstream: e1000_82540.c e1000_setup_copper_link_82540()
pub fn setup_copper_link_82540<I: E1000RegisterIo, N: E1000NvmAccess, P: PhyRegisterIo, L: LinkOps82540>(io: &mut I, nvm: &mut N, phy: &mut P, link: &mut L, mac: E1000MacType) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    set_phy_mode_82540(nvm, phy, mac)?;
    if matches!(mac, E1000MacType::I82545Rev3 | E1000MacType::I82546Rev3) {
        let data = phy.read_phy_register(M88_PHY_SPEC_CTRL)? | 0x0008;
        phy.write_phy_register(M88_PHY_SPEC_CTRL, data)?;
    }
    link.setup_copper_m88()?;
    link.setup_copper_generic()
}

/// upstream: e1000_82540.c e1000_setup_fiber_serdes_link_82540()
pub fn setup_fiber_serdes_link_82540<N: E1000NvmAccess, P: PhyRegisterIo, L: LinkOps82540>(nvm: &mut N, phy: &mut P, link: &mut L, mac: E1000MacType, media: E1000MediaType) -> DevResult {
    if matches!(mac, E1000MacType::I82545Rev3 | E1000MacType::I82546Rev3) {
        if media == E1000MediaType::InternalSerdes { adjust_serdes_amplitude_82540(nvm, phy)?; }
        set_vco_speed_82540(phy)?;
    }
    link.setup_fiber_serdes_generic()
}

/// upstream: e1000_82540.c e1000_power_down_phy_copper_82540()
pub fn power_down_phy_copper_82540<I: E1000RegisterIo, P: PhyRegisterIo>(io: &mut I, phy: &mut P) -> DevResult {
    if io.read_register(E1000_MANC)? & E1000_MANC_SMBUS_EN == 0 { phy.power_down_phy_copper()?; }
    Ok(())
}

/// upstream: e1000_82540.c e1000_clear_hw_cntrs_82540()
pub fn clear_hw_cntrs_82540<I: E1000RegisterIo, O: InitOps82540>(io: &mut I, ops: &mut O) -> DevResult {
    ops.clear_base_counters()?;
    // These are the legacy 82540 counters read to clear their latched values.
    for register in [0x405c,0x4060,0x4064,0x4068,0x406c,0x4070,0x40d8,0x40dc,0x40e0,0x40e4,0x40e8,0x40ec,0x4004,0x400c,0x4034,0x403c,0x40f8,0x40fc,0x40b4,0x40b8,0x40bc] {
        let _ = io.read_register(register)?;
    }
    Ok(())
}

/// upstream: e1000_82540.c e1000_init_hw_82540()
pub fn init_hw_82540<I: E1000RegisterIo, O: InitOps82540>(io: &mut I, ops: &mut O, mac: E1000MacType, device_id: u16, mta_count: u16, rar_count: u16) -> DevResult {
    let _ = ops.init_id_led(); // ID LED failure is intentionally nonfatal.
    let early_mac = matches!(mac, E1000MacType::I82540 | E1000MacType::I82545 | E1000MacType::I82546);
    if early_mac { io.write_register(E1000_VET, 0)?; }
    ops.clear_vfta()?;
    ops.init_rx_addrs(rar_count)?;
    for i in 0..mta_count { io.write_register(E1000_MTA + u32::from(i) * 4, 0)?; let _ = io.read_register(E1000_STATUS)?; }
    if early_mac { ops.pcix_mmrbc_workaround()?; }
    let link_result = ops.setup_link();
    let txdctl = (io.read_register(0x03828)? & !E1000_TXDCTL_WTHRESH) | E1000_TXDCTL_FULL_TX_DESC_WB;
    io.write_register(0x03828, txdctl)?;
    clear_hw_cntrs_82540(io, ops)?;
    if matches!(device_id, 0x101d | 0x10b5) {
        let ctrl_ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_RO_DIS;
        io.write_register(E1000_CTRL_EXT, ctrl_ext)?;
    }
    link_result
}

/// upstream: e1000_82540.c e1000_read_mac_addr_82540()
pub fn read_mac_addr_82540<N: E1000NvmAccess>(nvm: &mut N, pci_function: u8) -> DevResult<[u8; 6]> {
    let mut address = [0u8; 6];
    for i in (0..6).step_by(2) {
        let word = nvm.read_nvm_words((i / 2) as u16, 1)?.first().copied().ok_or(DevError::Io)?;
        address[i] = word as u8;
        address[i + 1] = (word >> 8) as u8;
    }
    if pci_function == 1 { address[5] ^= 1; }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io { regs: alloc::vec::Vec<(u32,u32)>, writes: alloc::vec::Vec<(u32,u32)>, delay: u32 }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, reg: u32) -> DevResult<u32> { Ok(self.regs.iter().find(|(r,_)| *r == reg).map(|(_,v)| *v).unwrap_or(0)) }
        fn write_register(&mut self, reg: u32, value: u32) -> DevResult { self.writes.push((reg,value)); if let Some((_,v))=self.regs.iter_mut().find(|(r,_)| *r==reg) {*v=value;} else {self.regs.push((reg,value));} Ok(()) }
        fn delay_us(&mut self, us: u32) { self.delay += us; }
        fn invalid_tail_write(&mut self, _: &'static str) {}
        fn write_register_io(&mut self, reg: u32, value: u32) -> DevResult { self.writes.push((reg,value)); Ok(()) }
    }
    #[test]
    fn family_parameters_and_reset_preserve_82540_source_values() {
        assert_eq!(init_phy_params_82540(E1000MacType::I82545Rev3, PHY_ID_M88E1011_I).unwrap().reset_delay_us, 10_000);
        assert!(init_phy_params_82540(E1000MacType::I82540, 0).is_err());
        assert_eq!(init_mac_params_82540(0x1028).unwrap().media, E1000MediaType::InternalSerdes);
        assert_eq!(init_nvm_params_82540(&mut Io::default(), NvmOverride82540::MicrowireLarge).unwrap().word_size, 256);
        assert_eq!(init_function_pointers_82540(), InitPointers82540 { mac: true, nvm: true, phy: true });
        let mut io = Io::default();
        reset_hw_82540(&mut io, E1000MacType::I82540).unwrap();
        assert!(io.writes.contains(&(E1000_CTRL, E1000_CTRL_RST)));
        assert_eq!(io.delay, 15_000);
        let mut io = Io::default();
        reset_hw_82540(&mut io, E1000MacType::I82545Rev3).unwrap();
        assert!(io.writes.contains(&(E1000_CTRL_DUP, E1000_CTRL_RST)));
    }
}
