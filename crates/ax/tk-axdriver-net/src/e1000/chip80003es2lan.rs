//! Intel 80003ES2LAN/ESB2 family helpers.
//!
//! Translated from FreeBSD sys/dev/e1000/e1000_80003es2lan.c revision
//! c2b7fe4a9e94a0edba9dd2772874928b565c4f9e (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    chip82571::{NvmWriteOps82571, PhyOps82571},
    mac::E1000MediaType,
    nvm::{E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    registers::*,
};

const GG_ID: u32 = 0x01410ca0;
const PAGE_SHIFT: u32 = 5;
const MIN_ALT: u32 = 30;
const PAGE_SEL: u32 = 22;
const PAGE_SEL_ALT: u32 = 29;
const GG_SPEC_CTRL: u32 = 16;
const GG_SPEC_CTRL2: u32 = 26;
const GG_MAC_SPEC_CTRL: u32 = (2 << 5) | 21;
const GG_DSP_DISTANCE: u32 = (5 << 5) | 26;
const GG_KMRN_MODE: u32 = (193 << 5) | 16;
const GG_PWR_MGMT: u32 = (193 << 5) | 20;
const GG_INBAND: u32 = (194 << 5) | 18;
const PHY0: u16 = 2;
const PHY1: u16 = 4;
const CSR: u16 = 8;
const EEP: u16 = 1;
const KMRN_OFFSET_SHIFT: u32 = 16;
const KMRN_OFFSET_MASK: u32 = 0x001f0000;
const KMRN_READ: u32 = 0x00200000;
const KMRN_FIFO_RX_BYPASS: u16 = 0x0008;
const KMRN_FIFO_TX_BYPASS: u16 = 0x0800;
const KMRN_OPMODE_E_IDLE: u16 = 0x2000;
const KMRN_OPMODE_MASK: u16 = 0x000c;
const KMRN_OPMODE_INBAND_MDIO: u16 = 0x0004;
const GG_MAX_KMRN_RETRY: usize = 5;
const CABLE: [u16; 11] = [0, 60, 115, 150, 150, 60, 115, 150, 180, 180, 0xff];
const CROSSOVER_MASK: u16 = 0x0060;
const CROSSOVER_MDI: u16 = 0;
const CROSSOVER_MDIX: u16 = 0x20;
const CROSSOVER_AUTO: u16 = 0x60;
const POLARITY_DISABLE: u16 = 2;
const ASSERT_CRS: u16 = 0x10;
const TXCLK_MASK: u16 = 7;
const TXCLK_10: u16 = 4;
const TXCLK_100: u16 = 5;
const TXCLK_1000: u16 = 7;
const FALSE_CARRIER: u16 = 0x0800;
const ELECTRICAL_IDLE: u16 = 1;
const DIS_PADDING: u16 = 0x10;
const REV_AUTONEG: u16 = 0x2000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyParamsEs2 {
    pub id: u32,
    pub address: u8,
    pub reset_delay_us: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParamsEs2 {
    pub media: E1000MediaType,
    pub mta_count: u16,
    pub rar_count: u16,
    pub has_fwsm: bool,
    pub arc_valid: bool,
    pub adaptive_ifs: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CableLengthEs2 {
    pub min: u16,
    pub max: u16,
    pub average: u16,
}
pub trait SwFwEs2 {
    fn acquire_swfw(&mut self, mask: u16) -> DevResult;
    fn release_swfw(&mut self, mask: u16);
    fn acquire_nvm_generic(&mut self) -> DevResult;
    fn release_nvm_generic(&mut self);
}
pub trait LinkOpsEs2 {
    fn generic_link_setup(&mut self) -> DevResult;
    fn generic_copper_link(&mut self) -> DevResult;
    fn generic_speed_duplex(&mut self) -> DevResult<(u16, u16)>;
    fn generic_power_down(&mut self) -> DevResult;
    fn generic_alt_mac(&mut self) -> DevResult;
    fn generic_read_mac(&mut self) -> DevResult<[u8; 6]>;
    fn generic_rx_addrs(&mut self, count: u16) -> DevResult;
    fn clear_vfta(&mut self) -> DevResult;
    fn id_led_init(&mut self) -> DevResult;
    fn clear_base_counters(&mut self) -> DevResult;
}
pub trait Es2FamilyOps {
    fn disable_pcie_master(&mut self) -> DevResult;
    fn auto_read_done(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn mng_enabled(&mut self) -> bool;
    fn reset_blocked(&mut self) -> bool;
}

/// upstream: e1000_80003es2lan.c e1000_init_phy_params_80003es2lan()
pub fn init_phy_params_80003es2lan(
    media: E1000MediaType,
    id: u32,
) -> DevResult<Option<PhyParamsEs2>> {
    if media != E1000MediaType::Copper {
        return Ok(None);
    }
    if id != GG_ID {
        return Err(DevError::Io);
    }
    Ok(Some(PhyParamsEs2 {
        id,
        address: 1,
        reset_delay_us: 100,
    }))
}
/// upstream: e1000_80003es2lan.c e1000_init_nvm_params_80003es2lan()
pub fn init_nvm_params_80003es2lan<I: E1000RegisterIo>(
    io: &mut I,
    large: bool,
    small: bool,
) -> DevResult<E1000NvmConfig> {
    let eecd = io.read_register(E1000_EECD)?;
    let address_bits = if large {
        16
    } else if small {
        8
    } else if eecd & 0x400 != 0 {
        16
    } else {
        8
    };
    let page_size = if address_bits == 16 { 32 } else { 8 };
    let mut shift = ((eecd & 0x7800) >> 11) + 6;
    if shift > 14 {
        shift = 14
    }
    let word_size = 1u16.checked_shl(shift).ok_or(DevError::InvalidParam)?;
    Ok(E1000NvmConfig {
        kind: E1000NvmType::Spi,
        word_size,
        delay_usec: 1,
        opcode_bits: 8,
        address_bits,
        page_size,
    })
}
/// upstream: e1000_80003es2lan.c e1000_init_mac_params_80003es2lan()
pub fn init_mac_params_80003es2lan<I: E1000RegisterIo>(
    io: &mut I,
    device_id: u16,
) -> DevResult<MacParamsEs2> {
    let media = if device_id == 0x1098 {
        E1000MediaType::InternalSerdes
    } else {
        E1000MediaType::Copper
    };
    let fwsm = io.read_register(E1000_FWSM)?;
    Ok(MacParamsEs2 {
        media,
        mta_count: 128,
        rar_count: E1000_RAR_ENTRIES as u16,
        has_fwsm: true,
        arc_valid: fwsm & 0xe != 0,
        adaptive_ifs: false,
    })
}
/// upstream: e1000_80003es2lan.c e1000_init_function_pointers_80003es2lan()
pub const fn init_function_pointers_80003es2lan() -> (bool, bool, bool) {
    (true, true, true)
}
/// upstream: e1000_80003es2lan.c e1000_acquire_phy_80003es2lan()
pub fn acquire_phy_80003es2lan<S: SwFwEs2>(s: &mut S, function: u8) -> DevResult {
    s.acquire_swfw(if function != 0 { PHY1 } else { PHY0 })
}
/// upstream: e1000_80003es2lan.c e1000_release_phy_80003es2lan()
pub fn release_phy_80003es2lan<S: SwFwEs2>(s: &mut S, function: u8) {
    s.release_swfw(if function != 0 { PHY1 } else { PHY0 })
}
/// upstream: e1000_80003es2lan.c e1000_acquire_mac_csr_80003es2lan()
pub fn acquire_mac_csr_80003es2lan<S: SwFwEs2>(s: &mut S) -> DevResult {
    s.acquire_swfw(CSR)
}
/// upstream: e1000_80003es2lan.c e1000_release_mac_csr_80003es2lan()
pub fn release_mac_csr_80003es2lan<S: SwFwEs2>(s: &mut S) {
    s.release_swfw(CSR)
}
/// upstream: e1000_80003es2lan.c e1000_acquire_nvm_80003es2lan()
pub fn acquire_nvm_80003es2lan<S: SwFwEs2>(s: &mut S) -> DevResult {
    s.acquire_swfw(EEP)?;
    if let Err(e) = s.acquire_nvm_generic() {
        s.release_swfw(EEP);
        return Err(e);
    }
    Ok(())
}
/// upstream: e1000_80003es2lan.c e1000_release_nvm_80003es2lan()
pub fn release_nvm_80003es2lan<S: SwFwEs2>(s: &mut S) {
    s.release_nvm_generic();
    s.release_swfw(EEP)
}

fn gg_page_select(offset: u32) -> u32 {
    if offset & 31 < MIN_ALT {
        PAGE_SEL
    } else {
        PAGE_SEL_ALT
    }
}
/// upstream: e1000_80003es2lan.c e1000_read_phy_reg_gg82563_80003es2lan()
pub fn read_phy_reg_gg82563_80003es2lan<P: PhyOps82571, S: SwFwEs2>(
    phy: &mut P,
    sem: &mut S,
    function: u8,
    offset: u32,
    mdic_wa: bool,
) -> DevResult<u16> {
    acquire_phy_80003es2lan(sem, function)?;
    let select = gg_page_select(offset) as u16;
    let page = (offset >> PAGE_SHIFT) as u16;
    let ret = (|| {
        phy.write_phy(select, page)?;
        if mdic_wa {
            phy.delay_us(200);
            if phy.read_phy(select)? != page {
                return Err(DevError::Io);
            }
            phy.delay_us(200)
        }
        let data = phy.read_phy((offset & 31) as u16)?;
        if mdic_wa {
            phy.delay_us(200)
        }
        Ok(data)
    })();
    release_phy_80003es2lan(sem, function);
    ret
}
/// upstream: e1000_80003es2lan.c e1000_write_phy_reg_gg82563_80003es2lan()
pub fn write_phy_reg_gg82563_80003es2lan<P: PhyOps82571, S: SwFwEs2>(
    phy: &mut P,
    sem: &mut S,
    function: u8,
    offset: u32,
    value: u16,
    mdic_wa: bool,
) -> DevResult {
    acquire_phy_80003es2lan(sem, function)?;
    let select = gg_page_select(offset) as u16;
    let page = (offset >> PAGE_SHIFT) as u16;
    let ret = (|| {
        phy.write_phy(select, page)?;
        if mdic_wa {
            phy.delay_us(200);
            if phy.read_phy(select)? != page {
                return Err(DevError::Io);
            }
            phy.delay_us(200)
        }
        phy.write_phy((offset & 31) as u16, value)?;
        if mdic_wa {
            phy.delay_us(200)
        }
        Ok(())
    })();
    release_phy_80003es2lan(sem, function);
    ret
}
/// upstream: e1000_80003es2lan.c e1000_write_nvm_80003es2lan()
pub fn write_nvm_80003es2lan<N: NvmWriteOps82571>(
    nvm: &mut N,
    offset: u16,
    words: &[u16],
) -> DevResult {
    nvm.write_spi(offset, words)
}
/// upstream: e1000_80003es2lan.c e1000_get_cfg_done_80003es2lan()
pub fn get_cfg_done_80003es2lan<I: E1000RegisterIo>(io: &mut I, function: u8) -> DevResult {
    let mask = if function == 1 { 0x80000 } else { 0x40000 };
    for _ in 0..100 {
        if io.read_register(E1000_EEMNGCTL)? & mask != 0 {
            return Ok(());
        }
        io.delay_us(1000)
    }
    Err(DevError::Io)
}
/// upstream: e1000_80003es2lan.c e1000_phy_force_speed_duplex_80003es2lan()
pub fn phy_force_speed_duplex_80003es2lan<P: PhyOps82571>(
    phy: &mut P,
    forced: u16,
    wait: bool,
) -> DevResult {
    let spec = phy.read_phy(GG_SPEC_CTRL as u16)? & !CROSSOVER_MASK;
    phy.write_phy(GG_SPEC_CTRL as u16, spec)?;
    let mut ctrl = phy.read_phy(0)?;
    if forced & 0x2000 != 0 {
        ctrl |= 0x2000
    } else {
        ctrl &= !0x2000
    }
    ctrl |= 0x8000;
    phy.write_phy(0, ctrl)?;
    phy.delay_us(1);
    if wait {
        let mut link = phy.phy_has_link()?;
        if !link {
            phy.reset_phy()?;
            link = phy.phy_has_link()?;
        }
        if !link {
            phy.reset_phy()?;
            let _ = phy.phy_has_link()?;
        }
    }
    let mut mac = phy.read_phy(GG_MAC_SPEC_CTRL as u16)?;
    mac &= !TXCLK_MASK;
    mac |= if forced & 3 != 0 { TXCLK_10 } else { TXCLK_100 };
    mac |= ASSERT_CRS;
    phy.write_phy(GG_MAC_SPEC_CTRL as u16, mac)
}
/// upstream: e1000_80003es2lan.c e1000_get_cable_length_80003es2lan()
pub fn get_cable_length_80003es2lan<P: PhyOps82571>(phy: &mut P) -> DevResult<CableLengthEs2> {
    let data = phy.read_phy(GG_DSP_DISTANCE as u16)?;
    let i = usize::from(data & 7);
    if i >= CABLE.len() - 5 {
        return Err(DevError::Io);
    }
    let min = CABLE[i];
    let max = CABLE[i + 5];
    Ok(CableLengthEs2 {
        min,
        max,
        average: (min + max) / 2,
    })
}
/// upstream: e1000_80003es2lan.c e1000_get_link_up_info_80003es2lan()
pub fn get_link_up_info_80003es2lan<O: LinkOpsEs2>(
    ops: &mut O,
    media: E1000MediaType,
) -> DevResult<(u16, u16)> {
    if media == E1000MediaType::Copper {
        let speed = ops.generic_speed_duplex()?;
        ops.generic_link_setup()?;
        Ok(speed)
    } else {
        Err(DevError::Unsupported)
    }
}

/// upstream: e1000_80003es2lan.c e1000_read_kmrn_reg_80003es2lan()
pub fn read_kmrn_reg_80003es2lan<I: E1000RegisterIo, S: SwFwEs2>(
    io: &mut I,
    sem: &mut S,
    offset: u32,
) -> DevResult<u16> {
    acquire_mac_csr_80003es2lan(sem)?;
    let cmd = ((offset << KMRN_OFFSET_SHIFT) & KMRN_OFFSET_MASK) | KMRN_READ;
    io.write_register(E1000_KMRNCTRLSTA, cmd)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(2);
    let data = io.read_register(E1000_KMRNCTRLSTA)? as u16;
    release_mac_csr_80003es2lan(sem);
    Ok(data)
}

/// upstream: e1000_80003es2lan.c e1000_write_kmrn_reg_80003es2lan()
pub fn write_kmrn_reg_80003es2lan<I: E1000RegisterIo, S: SwFwEs2>(
    io: &mut I,
    sem: &mut S,
    offset: u32,
    data: u16,
) -> DevResult {
    acquire_mac_csr_80003es2lan(sem)?;
    let cmd = ((offset << KMRN_OFFSET_SHIFT) & KMRN_OFFSET_MASK) | u32::from(data);
    io.write_register(E1000_KMRNCTRLSTA, cmd)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(2);
    release_mac_csr_80003es2lan(sem);
    Ok(())
}

/// upstream: e1000_80003es2lan.c e1000_cfg_kmrn_10_100_80003es2lan()
pub fn cfg_kmrn_10_100_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, P: PhyOps82571>(
    io: &mut I,
    sem: &mut S,
    phy: &mut P,
    duplex: u16,
) -> DevResult {
    write_kmrn_reg_80003es2lan(io, sem, 0x10, 0x0004)?;
    let mut tipg = io.read_register(E1000_TIPG)?;
    tipg = (tipg & !E1000_TIPG_IPGT_MASK) | 9;
    io.write_register(E1000_TIPG, tipg)?;
    let mut value = 0;
    for _ in 0..GG_MAX_KMRN_RETRY {
        let a = phy.read_phy(GG_KMRN_MODE as u16)?;
        let b = phy.read_phy(GG_KMRN_MODE as u16)?;
        value = b;
        if a == b {
            break;
        }
    }
    if duplex == 0 {
        value |= FALSE_CARRIER
    } else {
        value &= !FALSE_CARRIER
    }
    phy.write_phy(GG_KMRN_MODE as u16, value)
}

/// upstream: e1000_80003es2lan.c e1000_cfg_kmrn_1000_80003es2lan()
pub fn cfg_kmrn_1000_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, P: PhyOps82571>(
    io: &mut I,
    sem: &mut S,
    phy: &mut P,
) -> DevResult {
    write_kmrn_reg_80003es2lan(io, sem, 0x10, 0)?;
    let mut tipg = io.read_register(E1000_TIPG)?;
    tipg = (tipg & !E1000_TIPG_IPGT_MASK) | 8;
    io.write_register(E1000_TIPG, tipg)?;
    let mut value = 0;
    for _ in 0..GG_MAX_KMRN_RETRY {
        let a = phy.read_phy(GG_KMRN_MODE as u16)?;
        let b = phy.read_phy(GG_KMRN_MODE as u16)?;
        value = b;
        if a == b {
            break;
        }
    }
    value &= !FALSE_CARRIER;
    phy.write_phy(GG_KMRN_MODE as u16, value)
}

/// upstream: e1000_80003es2lan.c e1000_cfg_on_link_up_80003es2lan()
pub fn cfg_on_link_up_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, P: PhyOps82571, O: LinkOpsEs2>(
    io: &mut I,
    sem: &mut S,
    phy: &mut P,
    ops: &mut O,
    media: E1000MediaType,
) -> DevResult {
    if media != E1000MediaType::Copper {
        return Ok(());
    }
    let (speed, duplex) = ops.generic_speed_duplex()?;
    if speed == 1000 {
        cfg_kmrn_1000_80003es2lan(io, sem, phy)
    } else {
        cfg_kmrn_10_100_80003es2lan(io, sem, phy, duplex)
    }
}

/// upstream: e1000_80003es2lan.c e1000_reset_hw_80003es2lan()
pub fn reset_hw_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, O: LinkOpsEs2>(
    io: &mut I,
    sem: &mut S,
    ops: &mut O,
    function: u8,
    mut disable_pcie_master: impl FnMut() -> DevResult,
    mut auto_read_done: impl FnMut() -> DevResult,
) -> DevResult {
    let _ = disable_pcie_master();
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    acquire_phy_80003es2lan(sem, function)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    release_phy_80003es2lan(sem, function);
    if let Ok(mut value) = read_kmrn_reg_80003es2lan(io, sem, 0) {
        value |= 0x0800;
        let _ = write_kmrn_reg_80003es2lan(io, sem, 0, value);
    }
    auto_read_done()?;
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    ops.generic_alt_mac()
}

/// upstream: e1000_80003es2lan.c e1000_initialize_hw_bits_80003es2lan()
pub fn initialize_hw_bits_80003es2lan<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
) -> DevResult {
    for q in 0..2 {
        let reg = 0x3828 + q * 0x100;
        let value = io.read_register(reg)? | (1 << 22);
        io.write_register(reg, value)?;
    }
    let mut tarc = io.read_register(0x3840)? & !(0xf << 27);
    if media != E1000MediaType::Copper {
        tarc &= !(1 << 20)
    }
    io.write_register(0x3840, tarc)?;
    let mut tarc1 = io.read_register(0x3940)?;
    if io.read_register(E1000_TCTL)? & E1000_TCTL_MULR != 0 {
        tarc1 &= !(1 << 28)
    } else {
        tarc1 |= 1 << 28
    }
    io.write_register(0x3940, tarc1)?;
    let rfctl =
        io.read_register(E1000_RFCTL)? | E1000_RFCTL_IPV6_EX_DIS | E1000_RFCTL_NEW_IPV6_EXT_DIS;
    io.write_register(E1000_RFCTL, rfctl)
}

/// upstream: e1000_80003es2lan.c e1000_init_hw_80003es2lan()
pub fn init_hw_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, O: LinkOpsEs2>(
    io: &mut I,
    sem: &mut S,
    ops: &mut O,
    mta_count: u16,
    rar_count: u16,
    function: u8,
    media: E1000MediaType,
    mut mdic_wa_enable: impl FnMut(bool),
) -> DevResult {
    initialize_hw_bits_80003es2lan(io, media)?;
    let _ = ops.id_led_init();
    ops.clear_vfta()?;
    ops.generic_rx_addrs(rar_count)?;
    for i in 0..mta_count {
        io.write_register(E1000_MTA + u32::from(i) * 4, 0)?;
    }
    let link = ops.generic_link_setup();
    link?;
    if let Ok(mut v) = read_kmrn_reg_80003es2lan(io, sem, 0) {
        v |= 0x0800;
        let _ = write_kmrn_reg_80003es2lan(io, sem, 0, v);
    }
    for q in 0..2 {
        let reg = 0x3828 + q * 0x100;
        let v = (io.read_register(reg)? & !E1000_TXDCTL_WTHRESH)
            | E1000_TXDCTL_FULL_TX_DESC_WB
            | E1000_TXDCTL_COUNT_DESC;
        io.write_register(reg, v)?;
    }
    let tctl = io.read_register(E1000_TCTL)? | 0x01000000;
    io.write_register(E1000_TCTL, tctl)?;
    let tctl_ext = (io.read_register(E1000_TCTL_EXT)? & !0x00ff0000) | 0x00030000;
    io.write_register(E1000_TCTL_EXT, tctl_ext)?;
    let tipg = (io.read_register(E1000_TIPG)? & !E1000_TIPG_IPGT_MASK) | 8;
    io.write_register(E1000_TIPG, tipg)?;
    let fflt = io.read_register(E1000_FFLT + 4)? & !0x00100000;
    io.write_register(E1000_FFLT + 4, fflt)?;
    mdic_wa_enable(true);
    if let Ok(v) = read_kmrn_reg_80003es2lan(io, sem, 0) {
        if v & KMRN_OPMODE_MASK == KMRN_OPMODE_INBAND_MDIO {
            mdic_wa_enable(false)
        }
    }
    clear_hw_cntrs_80003es2lan(io, || ops.clear_base_counters())?;
    let _ = function;
    Ok(())
}

/// upstream: e1000_80003es2lan.c e1000_read_mac_addr_80003es2lan()
pub fn read_mac_addr_80003es2lan<O: LinkOpsEs2>(ops: &mut O) -> DevResult<[u8; 6]> {
    ops.generic_alt_mac()?;
    ops.generic_read_mac()
}

/// upstream: e1000_80003es2lan.c e1000_power_down_phy_copper_80003es2lan()
pub fn power_down_phy_copper_80003es2lan<O: LinkOpsEs2>(
    ops: &mut O,
    management: bool,
    reset_blocked: bool,
) -> DevResult {
    if !management && !reset_blocked {
        ops.generic_power_down()?;
    }
    Ok(())
}

/// upstream: e1000_80003es2lan.c e1000_clear_hw_cntrs_80003es2lan()
pub fn clear_hw_cntrs_80003es2lan<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut base: B,
) -> DevResult {
    base()?;
    for r in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec, 0x4004, 0x400c, 0x4034, 0x403c, 0x40f8, 0x40fc, 0x40b4, 0x40b8, 0x40bc, 0x1400,
        0x4104, 0x4108, 0x4110, 0x4114, 0x4118, 0x411c, 0x4120, 0x4124,
    ] {
        let _ = io.read_register(r)?;
    }
    Ok(())
}

/// upstream: e1000_80003es2lan.c e1000_copper_link_setup_gg82563_80003es2lan()
pub fn copper_link_setup_gg82563_80003es2lan<I: E1000RegisterIo, S: SwFwEs2, P: PhyOps82571>(
    io: &mut I,
    sem: &mut S,
    phy: &mut P,
    function: u8,
    mdix: u8,
    disable_polarity: bool,
    management: bool,
) -> DevResult {
    let mut data = phy.read_phy(GG_MAC_SPEC_CTRL as u16)? | ASSERT_CRS;
    data = (data & !TXCLK_MASK) | TXCLK_1000;
    phy.write_phy(GG_MAC_SPEC_CTRL as u16, data)?;
    let mut spec = phy.read_phy(GG_SPEC_CTRL as u16)? & !CROSSOVER_MASK;
    spec |= match mdix {
        1 => CROSSOVER_MDI,
        2 => CROSSOVER_MDIX,
        _ => CROSSOVER_AUTO,
    };
    spec &= !POLARITY_DISABLE;
    if disable_polarity {
        spec |= POLARITY_DISABLE
    }
    phy.write_phy(GG_SPEC_CTRL as u16, spec)?;
    phy.reset_phy()?;
    write_kmrn_reg_80003es2lan(io, sem, 0, KMRN_FIFO_RX_BYPASS | KMRN_FIFO_TX_BYPASS)?;
    let mut mode = read_kmrn_reg_80003es2lan(io, sem, 0x1f)? | KMRN_OPMODE_E_IDLE;
    write_kmrn_reg_80003es2lan(io, sem, 0x1f, mode)?;
    let spec2 = phy.read_phy(GG_SPEC_CTRL2 as u16)? & !REV_AUTONEG;
    phy.write_phy(GG_SPEC_CTRL2 as u16, spec2)?;
    let ext = io.read_register(E1000_CTRL_EXT)? & !E1000_CTRL_EXT_LINK_MODE_MASK;
    io.write_register(E1000_CTRL_EXT, ext)?;
    let mut pwr = phy.read_phy(GG_PWR_MGMT as u16)?;
    if !management {
        pwr |= ELECTRICAL_IDLE;
        phy.write_phy(GG_PWR_MGMT as u16, pwr)?;
        mode = phy.read_phy(GG_KMRN_MODE as u16)? & !0x0800;
        phy.write_phy(GG_KMRN_MODE as u16, mode)?;
    }
    let inband = phy.read_phy(GG_INBAND as u16)? | DIS_PADDING;
    phy.write_phy(GG_INBAND as u16, inband)?;
    let _ = function;
    Ok(())
}

/// upstream: e1000_80003es2lan.c e1000_setup_copper_link_80003es2lan()
pub fn setup_copper_link_80003es2lan<
    I: E1000RegisterIo,
    S: SwFwEs2,
    P: PhyOps82571,
    O: LinkOpsEs2,
>(
    io: &mut I,
    sem: &mut S,
    phy: &mut P,
    ops: &mut O,
    function: u8,
    mdix: u8,
    disable_polarity: bool,
    management: bool,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    write_kmrn_reg_80003es2lan(io, sem, (0x34u32 << 5) | 4, 0xffff)?;
    let mut data = read_kmrn_reg_80003es2lan(io, sem, (0x34u32 << 5) | 9)? | 0x3f;
    write_kmrn_reg_80003es2lan(io, sem, (0x34u32 << 5) | 9, data)?;
    data = read_kmrn_reg_80003es2lan(io, sem, 0x02)? | 0x10;
    write_kmrn_reg_80003es2lan(io, sem, 0x02, data)?;
    copper_link_setup_gg82563_80003es2lan(
        io,
        sem,
        phy,
        function,
        mdix,
        disable_polarity,
        management,
    )?;
    ops.generic_copper_link()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io {
        regs: alloc::vec::Vec<(u32, u32)>,
        writes: alloc::vec::Vec<(u32, u32)>,
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, r: u32) -> DevResult<u32> {
            Ok(self
                .regs
                .iter()
                .find(|(x, _)| *x == r)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_register(&mut self, r: u32, v: u32) -> DevResult {
            self.writes.push((r, v));
            if let Some((_, x)) = self.regs.iter_mut().find(|(x, _)| *x == r) {
                *x = v
            } else {
                self.regs.push((r, v))
            }
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    struct Phy {
        value: u16,
    }
    impl PhyOps82571 for Phy {
        fn delay_us(&mut self, _: u32) {}
        fn read_phy(&mut self, _: u16) -> DevResult<u16> {
            Ok(self.value)
        }
        fn write_phy(&mut self, _: u16, v: u16) -> DevResult {
            self.value = v;
            Ok(())
        }
        fn generic_phy_id(&mut self) -> DevResult<(u32, u32)> {
            Ok((GG_ID, 0))
        }
        fn phy_has_link(&mut self) -> DevResult<bool> {
            Ok(true)
        }
        fn setup_igp(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_m88(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_copper_generic(&mut self) -> DevResult {
            Ok(())
        }
        fn reset_phy(&mut self) -> DevResult {
            Ok(())
        }
        fn config_fc(&mut self) -> DevResult {
            Ok(())
        }
        fn check_reset_block(&mut self) -> bool {
            false
        }
        fn power_down_phy(&mut self) -> DevResult {
            Ok(())
        }
    }
    #[test]
    fn es2_phy_nvm_cable_and_media_parameters_match_family() {
        assert!(
            init_phy_params_80003es2lan(E1000MediaType::Copper, GG_ID)
                .unwrap()
                .is_some()
        );
        assert!(init_phy_params_80003es2lan(E1000MediaType::Copper, 0).is_err());
        let mut io = Io::default();
        io.regs.push((E1000_EECD, 0x7800 | 0x400));
        let nvm = init_nvm_params_80003es2lan(&mut io, false, false).unwrap();
        assert_eq!(
            (nvm.address_bits, nvm.page_size, nvm.word_size),
            (16, 32, 0x4000)
        );
        let mac = init_mac_params_80003es2lan(&mut Io::default(), 0x1098).unwrap();
        assert_eq!(mac.media, E1000MediaType::InternalSerdes);
        assert!(!mac.adaptive_ifs);
        let mut phy = Phy { value: 4 };
        assert_eq!(
            get_cable_length_80003es2lan(&mut phy).unwrap(),
            CableLengthEs2 {
                min: 150,
                max: 180,
                average: 165
            }
        );
    }
}
